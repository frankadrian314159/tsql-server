//! A session on a server over TDS: a cluster's control node, or any server that speaks the protocol.
//!
//! The client is `tiberius`. It runs on a private single-threaded runtime that the calling thread blocks on, so the app's
//! worker-thread scheme (the session moves to a thread while a script runs) works unchanged.
//!
//! The engine-side introspection (`Connection::table_info`, `objects`, ...) reads the catalog in memory; over the wire the
//! same information is read from the `sys` catalog views, which the server provides for client tools. What the wire does not
//! carry is left out: `PRINT` output and row counts of statements, XML and spatial index details of a table, and the
//! storage statistics of the engine.
use crate::backend::Backend;
use futures_util::TryStreamExt;
use std::time::Duration;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tsql_server::inspect::{CheckInfo, ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectEntry, ObjectKind, TableInfo};
use tsql_server::types::DtKind;
use tsql_server::{BatchResult, Event, ResultSet, SqlError, Value};

type Conn = Client<Compat<TcpStream>>;

pub struct RemoteBackend {
    rt: tokio::runtime::Runtime,
    client: Conn,
    login: String,
    trancount: i32,
}

/// Where and as whom to connect.
#[derive(Clone, Debug)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub login: String,
    pub password: String,
    pub database: String,
    /// Require TLS (and trust the server's certificate, which a work-alike generates itself).
    pub encrypt: bool,
}

impl Default for Target {
    fn default() -> Target {
        Target { host: "127.0.0.1".into(), port: 1433, login: "sa".into(), password: String::new(), database: String::new(), encrypt: false }
    }
}

fn sql_error(e: tiberius::error::Error) -> SqlError {
    match e {
        tiberius::error::Error::Server(t) => {
            let mut s = SqlError::new(t.code() as i32, t.message().to_string());
            s.severity = t.class();
            s.state = t.state();
            s
        }
        other => SqlError::new(50000, other.to_string()),
    }
}

/// A string as a T-SQL literal.
fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn convert(d: ColumnData<'static>) -> Value {
    match &d {
        ColumnData::U8(v) => v.map(|v| Value::Int(v as i32)).unwrap_or(Value::Null),
        ColumnData::I16(v) => v.map(|v| Value::Int(v as i32)).unwrap_or(Value::Null),
        ColumnData::I32(v) => v.map(Value::Int).unwrap_or(Value::Null),
        ColumnData::I64(v) => v.map(Value::BigInt).unwrap_or(Value::Null),
        ColumnData::F32(v) => v.map(|v| Value::Float(v as f64)).unwrap_or(Value::Null),
        ColumnData::F64(v) => v.map(Value::Float).unwrap_or(Value::Null),
        ColumnData::Bit(v) => v.map(Value::Bit).unwrap_or(Value::Null),
        ColumnData::String(v) => v.as_ref().map(|s| Value::Str(s.to_string())).unwrap_or(Value::Null),
        ColumnData::Guid(v) => v.map(|g| Value::Guid(uuid::Uuid::from_bytes(*g.as_bytes()))).unwrap_or(Value::Null),
        ColumnData::Binary(v) => v.as_ref().map(|b| Value::Bin(b.to_vec())).unwrap_or(Value::Null),
        ColumnData::Numeric(v) => v.map(|n| Value::Dec(tsql_server::decimal::Dec::new(n.value(), n.scale()))).unwrap_or(Value::Null),
        ColumnData::Xml(v) => v.as_ref().map(|x| Value::Str(x.clone().into_owned().into_string())).unwrap_or(Value::Null),
        ColumnData::DateTime(v) => {
            if v.is_none() {
                return Value::Null;
            }
            chrono::NaiveDateTime::from_sql(&d).ok().flatten().map(|t| Value::DateTime(t, DtKind::DateTime)).unwrap_or(Value::Null)
        }
        ColumnData::SmallDateTime(v) => {
            if v.is_none() {
                return Value::Null;
            }
            chrono::NaiveDateTime::from_sql(&d).ok().flatten().map(|t| Value::DateTime(t, DtKind::SmallDateTime)).unwrap_or(Value::Null)
        }
        ColumnData::DateTime2(v) => {
            if v.is_none() {
                return Value::Null;
            }
            chrono::NaiveDateTime::from_sql(&d).ok().flatten().map(|t| Value::DateTime(t, DtKind::DateTime2)).unwrap_or(Value::Null)
        }
        ColumnData::Date(_) => chrono::NaiveDate::from_sql(&d).ok().flatten().map(Value::Date).unwrap_or(Value::Null),
        ColumnData::Time(_) => chrono::NaiveTime::from_sql(&d).ok().flatten().map(Value::Time).unwrap_or(Value::Null),
        ColumnData::DateTimeOffset(_) => chrono::DateTime::<chrono::FixedOffset>::from_sql(&d).ok().flatten().map(Value::Dto).unwrap_or(Value::Null),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Str(s) => s.clone(),
        other => other.to_string(),
    }
}

fn num(v: &Value) -> i64 {
    match v {
        Value::Bit(b) => *b as i64,
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        Value::Dec(d) => (d.m / tsql_server::decimal::POW10[d.s as usize]) as i64,
        Value::Float(f) => *f as i64,
        Value::Str(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

impl RemoteBackend {
    /// Connect and log in. The error is what to show the user.
    pub fn connect(t: &Target) -> Result<RemoteBackend, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let mut cfg = Config::new();
        cfg.host(&t.host);
        cfg.port(t.port);
        cfg.authentication(AuthMethod::sql_server(&t.login, &t.password));
        if t.encrypt {
            cfg.encryption(EncryptionLevel::Required);
            cfg.trust_cert();
        } else {
            cfg.encryption(EncryptionLevel::NotSupported);
        }
        if !t.database.trim().is_empty() {
            cfg.database(t.database.trim());
        }
        let client = rt
            .block_on(async {
                let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(cfg.get_addr())).await.map_err(|_| "timed out connecting".to_string())?.map_err(|e| e.to_string())?;
                tcp.set_nodelay(true).ok();
                tokio::time::timeout(Duration::from_secs(15), Client::connect(cfg, tcp.compat_write())).await.map_err(|_| "timed out logging in".to_string())?.map_err(|e| e.to_string())
            })
            .map_err(|e: String| format!("Could not connect to {}:{}: {}", t.host, t.port, e))?;
        let mut b = RemoteBackend { rt, client, login: t.login.clone(), trancount: 0 };
        if let Some(n) = b.rows("SELECT SUSER_NAME()").first().map(|r| text(&r[0])).filter(|n| !n.is_empty()) {
            b.login = n;
        }
        Ok(b)
    }

    /// Rows of a query; empty when it fails (introspection shows what it can).
    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        let res = self.run_batch(sql);
        if res.first_error().is_some() {
            return vec![];
        }
        res.resultsets().first().map(|r| r.rows.clone()).unwrap_or_default()
    }

    fn run_batch(&mut self, sql: &str) -> BatchResult {
        let client = &mut self.client;
        let events = self.rt.block_on(async move {
            let mut events: Vec<Event> = vec![];
            let mut cur: Option<ResultSet> = None;
            let mut stream = match client.simple_query(sql).await {
                Ok(s) => s,
                Err(e) => return vec![Event::Error(sql_error(e))],
            };
            loop {
                match stream.try_next().await {
                    Ok(Some(QueryItem::Metadata(m))) => {
                        if let Some(rs) = cur.take() {
                            events.push(Event::Rows(rs));
                        }
                        cur = Some(ResultSet { columns: m.columns().iter().map(|c| c.name().to_string()).collect(), types: vec![], rows: vec![] });
                    }
                    Ok(Some(QueryItem::Row(r))) => {
                        if cur.is_none() {
                            cur = Some(ResultSet { columns: r.columns().iter().map(|c| c.name().to_string()).collect(), types: vec![], rows: vec![] });
                        }
                        if let Some(rs) = cur.as_mut() {
                            rs.rows.push(r.into_iter().map(convert).collect());
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if let Some(rs) = cur.take() {
                            events.push(Event::Rows(rs));
                        }
                        events.push(Event::Error(sql_error(e)));
                        return events;
                    }
                }
            }
            if let Some(rs) = cur.take() {
                events.push(Event::Rows(rs));
            }
            events
        });
        BatchResult { events }
    }

    fn type_text(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
        let n = name.to_lowercase();
        match n.as_str() {
            "nvarchar" | "nchar" => {
                if max_length < 0 {
                    format!("{n}(max)")
                } else {
                    format!("{n}({})", max_length / 2)
                }
            }
            "varchar" | "char" | "varbinary" | "binary" => {
                if max_length < 0 {
                    format!("{n}(max)")
                } else {
                    format!("{n}({max_length})")
                }
            }
            "decimal" | "numeric" => format!("{n}({precision},{scale})"),
            "datetime2" | "time" | "datetimeoffset" => format!("{n}({scale})"),
            _ => n,
        }
    }
}

impl Backend for RemoteBackend {
    fn execute(&mut self, sql: &str) -> BatchResult {
        let res = self.run_batch(sql);
        // the transaction count is a property of the session that the wire does not report: ask for it
        if let Some(n) = self.rows("SELECT @@TRANCOUNT").first().and_then(|r| r.first()).map(num) {
            self.trancount = n as i32;
        }
        res
    }

    fn table_info(&mut self, name: &str) -> Option<TableInfo> {
        let quoted = tsql_server::tools::quote_name(name);
        let head = self.rows(&format!("SELECT object_id, SCHEMA_NAME(schema_id), name FROM sys.tables WHERE object_id = OBJECT_ID({})", lit(&quoted)));
        let h = head.first()?;
        let (id, schema, tname) = (num(&h[0]), text(&h[1]), text(&h[2]));
        let database = self.current_database();
        let identity = self.rows(&format!("SELECT name, seed_value, increment_value FROM sys.identity_columns WHERE object_id = {id}"));
        let pk_cols: Vec<String> = self
            .rows(&format!("SELECT c.name FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.object_id = {id} AND i.is_primary_key = 1"))
            .iter()
            .map(|r| text(&r[0]))
            .collect();
        let columns = self
            .rows(&format!(
                "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed, dc.name, dc.definition, cm.definition \
                 FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                 LEFT JOIN sys.computed_columns cm ON cm.object_id = c.object_id AND cm.column_id = c.column_id \
                 WHERE c.object_id = {id} ORDER BY c.column_id"
            ))
            .iter()
            .map(|r| {
                let cname = text(&r[0]);
                let pk = pk_cols.contains(&cname);
                let is_identity = num(&r[6]) != 0;
                ColumnInfo {
                    ty: Self::type_text(&text(&r[1]), num(&r[2]), num(&r[3]), num(&r[4])),
                    nullable: num(&r[5]) != 0 && !pk,
                    identity: if is_identity { Some(identity.iter().find(|i| text(&i[0]) == cname).map(|i| (num(&i[1]), num(&i[2]))).unwrap_or((1, 1))) } else { None },
                    has_default: !r[9].is_null(),
                    default_text: if r[9].is_null() { None } else { Some(text(&r[9])) },
                    default_name: if r[8].is_null() { None } else { Some(text(&r[8])) },
                    computed: num(&r[7]) != 0,
                    computed_text: if r[10].is_null() { None } else { Some(text(&r[10])) },
                    primary_key: pk,
                    name: cname,
                }
            })
            .collect();
        // indexes: one row per column, in index order; key columns before included ones
        let mut indexes: Vec<IndexInfo> = vec![];
        for r in self.rows(&format!(
            "SELECT i.name, i.is_unique, i.is_primary_key, i.is_unique_constraint, i.filter_definition, ic.is_descending_key, ic.is_included_column, c.name \
             FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE i.object_id = {id} AND i.index_id > 0 ORDER BY i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        )) {
            let iname = text(&r[0]);
            if !indexes.iter().any(|i| i.name == iname) {
                let (pk, uq) = (num(&r[2]) != 0, num(&r[3]) != 0);
                indexes.push(IndexInfo { name: iname.clone(), columns: vec![], unique: num(&r[1]) != 0 || pk, primary_key: pk, constraint: pk || uq, include: vec![], filter: if r[4].is_null() { None } else { Some(text(&r[4])) } });
            }
            let ix = indexes.iter_mut().find(|i| i.name == iname).expect("just added");
            if num(&r[6]) != 0 {
                ix.include.push(text(&r[7]));
            } else {
                ix.columns.push((text(&r[7]), num(&r[5]) != 0));
            }
        }
        // the primary key and the unique constraints first, as the engine's own description has them
        indexes.sort_by_key(|i| !(i.primary_key || i.unique && i.constraint));
        let mut foreign_keys: Vec<ForeignKeyInfo> = vec![];
        for r in self.rows(&format!(
            "SELECT f.name, pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name, f.delete_referential_action_desc, f.update_referential_action_desc \
             FROM sys.foreign_keys f JOIN sys.foreign_key_columns fc ON fc.constraint_object_id = f.object_id \
             JOIN sys.columns pc ON pc.object_id = fc.parent_object_id AND pc.column_id = fc.parent_column_id \
             JOIN sys.columns rc ON rc.object_id = fc.referenced_object_id AND rc.column_id = fc.referenced_column_id \
             JOIN sys.tables rt ON rt.object_id = fc.referenced_object_id \
             WHERE f.parent_object_id = {id} ORDER BY f.object_id, fc.constraint_column_id"
        )) {
            let fname = text(&r[0]);
            if !foreign_keys.iter().any(|f| f.name == fname) {
                foreign_keys.push(ForeignKeyInfo {
                    name: fname.clone(),
                    columns: vec![],
                    ref_table: format!("{}.{}", text(&r[2]), text(&r[3])).to_lowercase(),
                    ref_columns: vec![],
                    on_delete: text(&r[5]).replace('_', " "),
                    on_update: text(&r[6]).replace('_', " "),
                });
            }
            let f = foreign_keys.iter_mut().find(|f| f.name == fname).expect("just added");
            f.columns.push(text(&r[1]));
            f.ref_columns.push(text(&r[4]));
        }
        let check_defs: Vec<CheckInfo> = self
            .rows(&format!("SELECT k.name, k.definition, k.is_disabled, c.name FROM sys.check_constraints k LEFT JOIN sys.columns c ON c.object_id = k.parent_object_id AND c.column_id = k.parent_column_id WHERE k.parent_object_id = {id}"))
            .iter()
            .map(|r| CheckInfo { name: text(&r[0]), text: text(&r[1]), enabled: num(&r[2]) == 0, column: if r[3].is_null() { None } else { Some(text(&r[3])) } })
            .collect();
        let triggers = self.rows(&format!("SELECT name FROM sys.triggers WHERE parent_id = {id}")).iter().map(|r| text(&r[0])).collect();
        let approx_rows = self.rows(&format!("SELECT SUM(row_count) FROM sys.dm_db_partition_stats WHERE object_id = {id} AND index_id IN (0, 1)")).first().and_then(|r| r.first()).map(num).unwrap_or(0).max(0) as usize;
        Some(TableInfo {
            database,
            schema,
            name: tname,
            columns,
            indexes,
            foreign_keys,
            checks: check_defs.iter().map(|c| c.name.clone()).collect(),
            check_defs,
            triggers,
            xml_indexes: vec![],
            extra_ddl: vec![],
            approx_rows,
        })
    }

    fn objects(&mut self) -> Vec<ObjectEntry> {
        let mut v: Vec<ObjectEntry> = self
            .rows("SELECT SCHEMA_NAME(schema_id), name, type FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('U','V','P','FN','IF','TF','SO','SN')")
            .iter()
            .filter_map(|r| {
                let kind = match text(&r[2]).trim() {
                    "U" => ObjectKind::Table,
                    "V" => ObjectKind::View,
                    "P" => ObjectKind::Procedure,
                    "FN" | "IF" | "TF" => ObjectKind::Function,
                    "SO" => ObjectKind::Sequence,
                    "SN" => ObjectKind::Synonym,
                    _ => return None,
                };
                Some(ObjectEntry { schema: text(&r[0]), name: text(&r[1]), kind })
            })
            .collect();
        v.sort_by(|a, b| (a.schema.to_lowercase(), a.name.to_lowercase()).cmp(&(b.schema.to_lowercase(), b.name.to_lowercase())));
        v
    }

    fn definition(&mut self, name: &str) -> Option<String> {
        let quoted = tsql_server::tools::quote_name(name);
        self.rows(&format!("SELECT OBJECT_DEFINITION(OBJECT_ID({}))", lit(&quoted))).first().and_then(|r| r.first()).filter(|v| !v.is_null()).map(text)
    }

    fn databases(&mut self) -> Vec<String> {
        self.rows("SELECT name FROM sys.databases ORDER BY name").iter().map(|r| text(&r[0])).collect()
    }

    fn current_database(&mut self) -> String {
        self.rows("SELECT DB_NAME()").first().and_then(|r| r.first()).map(text).unwrap_or_default()
    }

    fn transaction_count(&self) -> i32 {
        self.trancount
    }

    fn login(&self) -> String {
        self.login.clone()
    }

    fn explain(&mut self, sql: &str) -> Result<Vec<String>, SqlError> {
        // SHOWPLAN is a session setting: on for one batch, then off again whatever happened
        let on = self.run_batch("SET SHOWPLAN_TEXT ON");
        if let Some(e) = on.first_error() {
            return Err(e.clone());
        }
        let res = self.run_batch(sql);
        self.run_batch("SET SHOWPLAN_TEXT OFF");
        if let Some(e) = res.first_error() {
            return Err(e.clone());
        }
        Ok(res.resultsets().iter().flat_map(|rs| rs.rows.iter().map(|row| text(&row[0]))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsql_server::tds::{Config as ListenConfig, Listener};

    /// A server in the process, reached over a real socket like any other.
    fn server() -> (Listener, Target) {
        let server = tsql_server::Server::new();
        server.connect().execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
        let l = Listener::bind(server, "127.0.0.1:0", ListenConfig::default()).expect("listener");
        let t = Target { port: l.local_addr().port(), password: "Str0ng!pw".into(), ..Default::default() };
        (l, t)
    }

    #[test]
    fn a_remote_session_runs_batches_and_reads_values() {
        let (_l, t) = server();
        let mut b = RemoteBackend::connect(&t).expect("connect");
        assert_eq!(b.login(), "sa");
        let r = b.execute("CREATE TABLE dbo.v (i INT, big BIGINT, d DECIMAL(10,2), f FLOAT, s NVARCHAR(10), flag BIT, day DATE, at DATETIME2, g UNIQUEIDENTIFIER, bin VARBINARY(4), n INT NULL); INSERT dbo.v VALUES (1, 9000000000, 12.50, 1.5, N'héllo', 1, '2026-03-04', '2026-03-04 05:06:07', '6F9619FF-8B86-D011-B42D-00C04FC964FF', 0x0102, NULL)");
        assert!(r.first_error().is_none(), "{:?}", r.first_error());
        let rows = b.query("SELECT i, big, d, f, s, flag, day, at, g, bin, n FROM dbo.v").unwrap();
        let shown: Vec<String> = rows[0].iter().map(|v| v.to_string()).collect();
        assert_eq!(shown[0], "1");
        assert_eq!(shown[1], "9000000000");
        assert_eq!(shown[2], "12.50");
        assert_eq!(shown[4], "héllo");
        assert_eq!(shown[6], "2026-03-04");
        assert!(shown[7].starts_with("2026-03-04 05:06:07"), "{}", shown[7]);
        assert_eq!(shown[9], "0x01020304".replace("0304", ""), "{:?}", shown);
        assert!(matches!(rows[0][10], Value::Null));
        // several result sets, and an empty one still names its columns
        let r = b.execute("SELECT 1 AS a; SELECT 2 AS b WHERE 1 = 0");
        let sets = r.resultsets();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[1].columns, vec!["b"]);
        assert!(sets[1].rows.is_empty());
        // an error is the server's, with its number
        let e = b.run("SELECT * FROM dbo.missing").unwrap_err();
        assert_eq!(e.number, 208);
        // the session survives it, and a transaction is seen
        assert_eq!(b.transaction_count(), 0);
        b.execute("BEGIN TRAN");
        assert_eq!(b.transaction_count(), 1);
        b.execute("ROLLBACK");
        assert_eq!(b.transaction_count(), 0);
    }

    #[test]
    fn a_remote_session_describes_the_catalog_from_the_sys_views() {
        let (_l, t) = server();
        let mut b = RemoteBackend::connect(&t).expect("connect");
        let r = b.execute_script(
            "CREATE TABLE dbo.p (id INT IDENTITY(5,2) PRIMARY KEY, name NVARCHAR(50) NOT NULL DEFAULT 'x', amt DECIMAL(10,2) NULL, CONSTRAINT ck_amt CHECK (amt >= 0))
GO
CREATE TABLE dbo.c (id INT PRIMARY KEY, pid INT NOT NULL CONSTRAINT fk_p REFERENCES dbo.p(id) ON DELETE CASCADE, tag VARCHAR(8) NULL)
GO
CREATE INDEX ix_name ON dbo.p (name DESC) INCLUDE (amt)
GO
CREATE VIEW dbo.pv AS SELECT id, name FROM dbo.p
GO
CREATE PROCEDURE dbo.sp_one AS SELECT 1
GO
INSERT dbo.p (name, amt) VALUES (N'a', 1), (N'b', 2)",
        );
        assert!(r.iter().all(|x| x.first_error().is_none()), "{:?}", r.iter().filter_map(|x| x.first_error()).collect::<Vec<_>>());
        let objs = b.objects();
        let kinds: Vec<(String, ObjectKind)> = objs.iter().map(|o| (o.qualified(), o.kind.clone())).collect();
        assert!(kinds.contains(&("dbo.p".into(), ObjectKind::Table)));
        assert!(kinds.contains(&("dbo.pv".into(), ObjectKind::View)));
        assert!(kinds.contains(&("dbo.sp_one".into(), ObjectKind::Procedure)));
        let info = b.table_info("dbo.p").expect("table");
        assert_eq!((info.schema.as_str(), info.name.as_str()), ("dbo", "p"));
        let names: Vec<&str> = info.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "amt"]);
        assert_eq!(info.columns[0].identity, Some((5, 2)));
        assert!(info.columns[0].primary_key && !info.columns[0].nullable);
        assert_eq!(info.columns[1].ty, "nvarchar(50)");
        assert!(info.columns[1].has_default && info.columns[1].default_text.as_deref().unwrap().contains('x'));
        assert_eq!(info.columns[2].ty, "decimal(10,2)");
        assert!(info.columns[2].nullable);
        assert!(info.indexes[0].primary_key, "the key comes first: {:?}", info.indexes);
        let ix = info.indexes.iter().find(|i| i.name == "ix_name").expect("index");
        assert_eq!(ix.columns, vec![("name".to_string(), true)]);
        assert_eq!(ix.include, vec!["amt"]);
        assert_eq!(info.check_defs.len(), 1);
        assert!(info.check_defs[0].enabled && info.check_defs[0].text.contains("amt"));
        assert_eq!(info.approx_rows, 2);
        let c = b.table_info("c").expect("table found without the schema");
        assert_eq!(c.foreign_keys.len(), 1);
        assert_eq!(c.foreign_keys[0].ref_table, "dbo.p");
        assert_eq!(c.foreign_keys[0].columns, vec!["pid"]);
        assert_eq!(c.foreign_keys[0].on_delete, "CASCADE");
        assert!(b.table_info("dbo.pv").is_none(), "a view is not a table");
        assert!(b.definition("dbo.pv").unwrap().contains("CREATE VIEW"));
        assert!(b.databases().iter().any(|d| d == "master"));
        assert_eq!(b.current_database(), "master");
        assert!(!b.explain("SELECT * FROM dbo.p WHERE id = 5").unwrap().is_empty());
    }

    #[test]
    fn a_wrong_password_is_reported() {
        let (_l, mut t) = server();
        t.password = "nope".into();
        let e = RemoteBackend::connect(&t).err().expect("refused");
        assert!(e.contains("Could not connect"), "{e}");
    }
}
