//! Helpers shared by the TPC benchmarks: a seeded random source, bulk loading and result reporting.
#![allow(dead_code)]
use rand::{rngs::StdRng, Rng as _, SeedableRng};
use std::time::Instant;
use tsql_server::Connection;

pub struct Rng(StdRng);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(StdRng::seed_from_u64(seed))
    }
    /// Uniform in `lo..=hi`.
    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        self.0.gen_range(lo..=hi)
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.0.gen_range(0..xs.len())]
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.0.gen_bool(p)
    }
    pub fn float(&mut self) -> f64 {
        self.0.gen::<f64>()
    }
    /// Words joined by spaces, from a small vocabulary.
    pub fn text(&mut self, words: usize) -> String {
        const W: &[&str] = &["the", "quick", "furiously", "carefully", "regular", "final", "pending", "ironic", "bold", "express", "deposits", "accounts", "packages", "theodolites", "foxes", "pinto", "beans", "requests", "platelets", "instructions", "dependencies", "excuses"];
        (0..words).map(|_| *self.pick(W)).collect::<Vec<_>>().join(" ")
    }
    pub fn digits(&mut self, n: usize) -> String {
        (0..n).map(|_| char::from(b'0' + self.0.gen_range(0..10u8))).collect()
    }
}

/// Escape a string for a T-SQL literal.
pub fn q(s: &str) -> String {
    s.replace('\'', "''")
}

/// Run a batch, panicking with the statement on an error.
pub fn must(c: &mut Connection, sql: &str) {
    let r = c.execute(sql);
    if let Some(e) = r.first_error() {
        let head: String = sql.chars().take(200).collect();
        panic!("{}\n=> {}", head, e);
    }
}

/// `INSERT table VALUES (...),(...)` in batches of 500 rows; each element of `rows` is one parenthesised tuple.
pub fn insert_rows(c: &mut Connection, table: &str, rows: &[String]) {
    for chunk in rows.chunks(500) {
        must(c, &format!("INSERT {} VALUES {}", table, chunk.join(",")));
    }
}

/// One line of a report.
pub struct Line {
    pub name: String,
    pub secs: f64,
    pub rows: usize,
    pub note: String,
}

#[derive(Default)]
pub struct Report {
    pub lines: Vec<Line>,
}

impl Report {
    pub fn add(&mut self, name: &str, secs: f64, rows: usize, note: &str) {
        println!("{:<34} {:>9.3}s {:>9} rows  {}", name, secs, rows, note);
        self.lines.push(Line { name: name.to_string(), secs, rows, note: note.to_string() });
    }
    pub fn total(&self) -> f64 {
        self.lines.iter().map(|l| l.secs).sum()
    }
}

/// Run a query, returning its rows as strings (or the error text) and the time it took.
pub fn timed(c: &mut Connection, sql: &str) -> (Result<Vec<Vec<String>>, String>, f64) {
    let t = Instant::now();
    let r = c.execute(sql);
    let dt = t.elapsed().as_secs_f64();
    if let Some(e) = r.first_error() {
        return (Err(format!("{}", e)), dt);
    }
    let rows = r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect()).unwrap_or_default();
    (Ok(rows), dt)
}

pub fn num(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// Are two numbers equal to within rounding?
pub fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 0.011 + 1e-9 * a.abs().max(b.abs())
}

/// Compare two result sets ignoring row order (ties under ORDER BY may come out in either order); numbers are
/// compared to within rounding. Returns a description of the first difference.
pub fn compare_rows(a: &[Vec<String>], b: &[Vec<String>]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} rows against {}", a.len(), b.len()));
    }
    let key = |r: &Vec<String>| r.join("\u{1}");
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort_by_key(key);
    b.sort_by_key(key);
    for (x, y) in a.iter().zip(b.iter()) {
        if x.len() != y.len() {
            return Some(format!("row widths {} and {}", x.len(), y.len()));
        }
        for (u, v) in x.iter().zip(y.iter()) {
            let same = u == v || (u.parse::<f64>().is_ok() && v.parse::<f64>().is_ok() && close(num(u), num(v)));
            if !same {
                return Some(format!("{:?} against {:?}", x, y));
            }
        }
    }
    None
}
