//! How fast the engine takes rows through `execute_stmts` (what the gather uses), by chunk size and table shape.
use std::time::Instant;
use tsql_server::ast::*;
use tsql_server::{Server, Value};

fn main() {
    let server = Server::new();
    let mut c = server.connect();
    let rows: Vec<Vec<Value>> = (0..150_000i64).map(|i| vec![Value::Int(i as i32), Value::Int((i % 15000) as i32), Value::Str(format!("special requests {}", i % 7))]).collect();
    for (shape, ddl) in [("heap", "CREATE TABLE t (a INT, b INT, c VARCHAR(40))"), ("primary key", "CREATE TABLE t (a INT PRIMARY KEY, b INT, c VARCHAR(40))")] {
        for chunk in [100usize, 1000, 10_000, 50_000] {
            let _ = c.execute("DROP TABLE IF EXISTS t");
            assert!(c.execute(ddl).first_error().is_none());
            let t = Instant::now();
            for part in rows.chunks(chunk) {
                let vals: Vec<Vec<Expr>> = part.iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
                let ins = Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(vec!["t".into()]), cols: None, output: None, src: InsertSrc::Values(vals), top: None }));
                assert!(c.execute_stmts(&[ins]).first_error().is_none());
            }
            let s = t.elapsed().as_secs_f64();
            println!("{:<12} chunk {:>6}: {:.2}s  ({:.0} rows/s)  INSERT statements", shape, chunk, s, rows.len() as f64 / s);
        }
        for chunk in [1000usize, 50_000] {
            let _ = c.execute("DROP TABLE IF EXISTS t");
            assert!(c.execute(ddl).first_error().is_none());
            let t = Instant::now();
            for part in rows.chunks(chunk) {
                c.load_rows(&["t".to_string()], None, part.to_vec()).unwrap();
            }
            let s = t.elapsed().as_secs_f64();
            let r = c.execute("SELECT COUNT(*), SUM(CAST(a AS BIGINT)), MAX(c) FROM t");
            if let Some(e) = r.first_error() { println!("ERROR after load_rows: {}", e); }
            println!("{:<12} chunk {:>6}: {:.2}s  ({:.0} rows/s)  load_rows -> {:?}", shape, chunk, s, rows.len() as f64 / s, r.resultsets()[0].rows[0].iter().map(|v| v.to_string()).collect::<Vec<_>>());
        }
    }
}
