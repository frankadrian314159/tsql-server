//! The distribution advisor.
//!
//! A table is sharded on the first column of its primary key unless `DISTRIBUTE TABLE` said otherwise, and a join on any other column
//! makes the cluster move rows: copy a small table to every shard (broadcast), re-spread one by the join column (repartition), or read
//! it into the control node (gather). Each of those costs something every time the query runs, and which of them a workload needs says
//! what the tables' distribution should have been. The control node therefore keeps count of what it had to move, per table, as well as
//! reads and writes, and `ADVISE DISTRIBUTION` turns the counts into advice:
//!
//! * a table that was **broadcast** again and again, is small enough to be, and has a high read-to-write ratio (read-mostly),
//!   should be `DISTRIBUTE TABLE t REPLICATED`: every shard then holds it and the copy for each query is not made;
//! * a table that was **repartitioned** by a column should be sharded `BY` that column when it is joined by it more often than by its
//!   present key (its partner stays where it is, and the join is then co-located);
//! * a table that was **gathered** (read into the control node because no plan over the shards could run the query) and is compared for
//!   equality with other tables on a column that is not its key should be sharded by that column.
//!
//! It is advice only: the distribution of a table that has rows cannot be changed in place (`DISTRIBUTE TABLE` needs an empty table), so
//! the change is made by creating the table again, distributing it, and loading it. Nothing here changes how a query is run.
use crate::{Dist, DistTable};
use std::collections::HashMap;
use std::sync::Mutex;
use tsql_server::{DataType, ResultSet, Value};

/// What was done to a table, and how often.
#[derive(Default, Clone, Debug)]
pub struct Moved {
    pub times: u64,
    /// rows involved (the sum over the times, as far as the control node knew the table's size)
    pub rows: u64,
    /// the columns the table was re-spread by or compared on, with how often
    pub columns: HashMap<String, u64>,
}

/// Workload read and write activity for a table.
#[derive(Default, Clone, Debug)]
pub struct Activity {
    pub reads: u64,
    pub writes: u64,
}

/// The counts, per table id and way of moving (`"broadcast"`, `"repartition"`, `"gather"`),
/// as well as table read and write operations.
#[derive(Default)]
pub struct Log {
    moved: Mutex<HashMap<(String, &'static str), Moved>>,
    activity: Mutex<HashMap<String, Activity>>,
}

impl Log {
    pub fn record(&self, table: &str, how: &'static str, rows: u64, columns: &[String]) {
        let mut m = self.moved.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry((table.to_string(), how)).or_default();
        e.times += 1;
        e.rows = e.rows.saturating_add(rows);
        for c in columns {
            *e.columns.entry(c.to_lowercase()).or_default() += 1;
        }
    }

    pub fn record_read(&self, table: &str) {
        let mut act = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        act.entry(table.to_string()).or_default().reads += 1;
    }

    pub fn record_write(&self, table: &str) {
        let mut act = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        act.entry(table.to_string()).or_default().writes += 1;
    }

    pub fn clear(&self) {
        self.moved.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.activity.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn snapshot(&self) -> (Vec<(String, &'static str, Moved)>, HashMap<String, Activity>) {
        let m = self.moved.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<_> = m.iter().map(|((t, h), s)| (t.clone(), *h, s.clone())).collect();
        v.sort_by(|a, b| b.2.rows.cmp(&a.2.rows).then(a.0.cmp(&b.0)).then(a.1.cmp(b.1)));
        let act = self.activity.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (v, act)
    }
}

/// How often a broadcast has to repeat before a table is worth replicating.
const REPEATS: u64 = 3;

fn present(t: &DistTable) -> String {
    match &t.dist {
        Dist::Replicated => "replicated".to_string(),
        Dist::Sharded { key, .. } => format!("sharded by {}", t.cols[*key].name),
    }
}

fn estimate_table_bytes(t: &DistTable, rows: u64) -> u64 {
    let row_bytes: u64 = t
        .cols
        .iter()
        .map(|c| {
            let ty = c.type_name.to_lowercase();
            match ty.as_str() {
                "tinyint" | "bit" => 1,
                "smallint" => 2,
                "int" | "integer" | "date" => 4,
                "bigint" | "float" | "real" | "datetime" | "datetime2" | "money" => 8,
                "uniqueidentifier" => 16,
                s if s.contains("char") || s.contains("text") || s.contains("binary") => 24,
                _ => 16,
            }
        })
        .sum::<u64>()
        .max(8);
    rows.saturating_mul(row_bytes)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// The advice for what the log holds: one row per table and way of moving: table, distribution now, what was done, times, rows, advice, reads, writes, read_ratio, table_size.
pub fn advise(
    log: &Log,
    tables: &[DistTable],
    broadcast_limit: u64,
    read_threshold_pct: u64,
    max_bytes: u64,
) -> ResultSet {
    let by_id: HashMap<&str, &DistTable> = tables.iter().map(|t| (t.id.as_str(), t)).collect();
    let (moved, activity) = log.snapshot();
    let mut rows = vec![];
    for (id, how, m) in moved {
        let Some(t) = by_id.get(id.as_str()) else { continue };
        let Dist::Sharded { key, .. } = &t.dist else { continue };
        let key_name = t.cols[*key].name.to_lowercase();
        let known = t.rows.map(|(_, n)| n);
        let act = activity.get(id.as_str()).cloned().unwrap_or_default();
        let total_ops = act.reads + act.writes;
        let read_ratio = if total_ops == 0 {
            1.0
        } else {
            act.reads as f64 / total_ops as f64
        };
        let read_ratio_pct = read_ratio * 100.0;
        let estimated_bytes = known.map(|r| estimate_table_bytes(t, r));
        let size_str = match (known, estimated_bytes) {
            (Some(r), Some(b)) => format!("{} rows, ~{}", r, format_bytes(b)),
            (Some(r), None) => format!("{} rows", r),
            _ => "size unknown".to_string(),
        };

        // the column named most often that is a column of the table and not its key
        let other = m.columns.iter().filter(|(c, _)| **c != key_name && t.cols.iter().any(|x| x.name.eq_ignore_ascii_case(c))).max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)));
        let advice = match how {
            "broadcast" if m.times >= REPEATS && known.is_some_and(|n| n <= broadcast_limit) && estimated_bytes.map_or(true, |b| b <= max_bytes) => {
                if read_ratio_pct >= read_threshold_pct as f64 {
                    format!("DISTRIBUTE TABLE {} REPLICATED (create it again, distribute it, load it): it is small ({}) and read-mostly ({:.1}% reads, {} reads, {} writes), copied to every shard each time", t.name.join("."), size_str, read_ratio_pct, act.reads, act.writes)
                } else {
                    format!("none: copied often ({} times) but read ratio {:.1}% ({} reads, {} writes) is below threshold {}%: replicating would cause write amplification", m.times, read_ratio_pct, act.reads, act.writes, read_threshold_pct)
                }
            }
            "broadcast" if m.times >= REPEATS => {
                format!("none: table is too big to replicate ({}), copied {} times", size_str, m.times)
            }
            "broadcast" => "none yet: copied rarely, or too big to replicate".to_string(),
            "repartition" => match other {
                Some((c, n)) if *n >= m.times / 2 + 1 => format!("DISTRIBUTE TABLE {} BY {} (create it again, distribute it, load it): the joins re-spread it by {}", t.name.join("."), c, c),
                _ => "none: its rows are moved by different columns".to_string(),
            },
            _ => match other {
                Some((c, _)) => format!("a query that reads {} into the control node compares {} with another table: consider DISTRIBUTE TABLE {} BY {}", t.name.join("."), c, t.name.join("."), c),
                None => "no column to suggest: the queries that gather it do not join it on another column".to_string(),
            },
        };
        rows.push(vec![
            Value::Str(t.name.join(".")),
            Value::Str(present(t)),
            Value::Str(how.to_string()),
            Value::BigInt(m.times as i64),
            Value::BigInt(m.rows.min(i64::MAX as u64) as i64),
            Value::Str(advice),
            Value::BigInt(act.reads as i64),
            Value::BigInt(act.writes as i64),
            Value::Str(format!("{:.1}%", read_ratio_pct)),
            Value::Str(size_str),
        ]);
    }
    ResultSet {
        columns: ["table_name", "distribution", "moved_how", "times", "rows_moved", "advice", "reads", "writes", "read_ratio", "table_size"].iter().map(|s| s.to_string()).collect(),
        types: vec![
            DataType::NVarChar(256),
            DataType::NVarChar(256),
            DataType::NVarChar(32),
            DataType::BigInt,
            DataType::BigInt,
            DataType::NVarChar(512),
            DataType::BigInt,
            DataType::BigInt,
            DataType::NVarChar(32),
            DataType::NVarChar(64),
        ],
        rows,
    }
}
