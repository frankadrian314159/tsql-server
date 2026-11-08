//! `tsql` — an interactive sqlcmd-style shell for the tsql-server engine.
use std::io::{self, BufRead, IsTerminal, Write};
use tsql_server::{convert, BatchResult, Connection, Event, ResultSet, Server, Value};

fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Bit(_))
}

fn print_resultset(rs: &ResultSet, out: &mut impl Write) {
    let ncols = rs.columns.len();
    let cells: Vec<Vec<String>> = rs.rows.iter().map(|r| r.iter().map(convert::display).collect()).collect();
    let columns: Vec<String> = rs.columns.iter().map(|c| if c.is_empty() { "(No column name)".to_string() } else { c.clone() }).collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count().max(1)).collect();
    for r in &cells {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |vals: &[String], right: &[bool]| -> String {
        let mut s = String::new();
        for i in 0..ncols {
            if i > 0 {
                s.push(' ');
            }
            let w = widths[i];
            if right[i] {
                s.push_str(&format!("{:>w$}", vals[i], w = w));
            } else {
                s.push_str(&format!("{:<w$}", vals[i], w = w));
            }
        }
        s.trim_end().to_string()
    };
    let left = vec![false; ncols];
    let _ = writeln!(out, "{}", line(&columns, &left));
    let dashes: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    let _ = writeln!(out, "{}", line(&dashes, &left));
    for (ri, r) in cells.iter().enumerate() {
        let right: Vec<bool> = rs.rows[ri].iter().map(is_numeric).collect();
        let _ = writeln!(out, "{}", line(r, &right));
    }
    let _ = writeln!(out);
}

fn print_result(res: &BatchResult, out: &mut impl Write) {
    for ev in &res.events {
        match ev {
            Event::Rows(rs) => print_resultset(rs, out),
            Event::Message(m) => {
                let _ = writeln!(out, "{}", m);
            }
            Event::RowCount(n) => {
                let _ = writeln!(out, "({} row{} affected)\n", n, if *n == 1 { "" } else { "s" });
            }
            Event::Error(e) => {
                let _ = writeln!(out, "Msg {}, Level {}, State {}, Line 1\n{}\n", e.number, e.severity, e.state, e.message);
            }
        }
    }
}

fn run_script(conn: &mut Connection, script: &str) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for res in conn.execute_script(script) {
        print_result(&res, &mut out);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // connection options first: -D <data dir>, -U <login>, -P <password>
    let (mut data_dir, mut user, mut pass): (Option<String>, Option<String>, String) = (None, None, String::new());
    let mut opts = tsql_server::StorageOptions::default();
    let mut k = 0;
    while k < args.len() {
        match args[k].as_str() {
            "-D" | "--data" => {
                data_dir = args.get(k + 1).cloned();
                k += 2;
            }
            "--pool-pages" => {
                match args.get(k + 1).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => opts.pool_pages = n,
                    None => {
                        eprintln!("--pool-pages needs a number of 8 KiB pages");
                        std::process::exit(1);
                    }
                }
                k += 2;
            }
            "-U" => {
                user = args.get(k + 1).cloned();
                k += 2;
            }
            "-P" => {
                pass = args.get(k + 1).cloned().unwrap_or_default();
                k += 2;
            }
            _ => k += 1,
        }
    }
    let server = match &data_dir {
        Some(d) => match Server::open_with(d, opts) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        },
        None => Server::new(),
    };
    let mut conn = match &user {
        Some(u) => match server.connect_as(u, &pass) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Msg {}, Level {}, State {}
{}", e.number, e.severity, e.state, e.message);
                std::process::exit(1);
            }
        },
        None => server.connect(),
    };
    let mut i = 0;
    let mut ran = false;
    while i < args.len() {
        match args[i].as_str() {
            "-Q" | "-q" => {
                if let Some(q) = args.get(i + 1) {
                    run_script(&mut conn, q);
                    ran = true;
                }
                i += 2;
            }
            "-i" => {
                if let Some(f) = args.get(i + 1) {
                    match std::fs::read_to_string(f) {
                        Ok(s) => run_script(&mut conn, &s),
                        Err(e) => eprintln!("Cannot read {}: {}", f, e),
                    }
                    ran = true;
                }
                i += 2;
            }
            "-h" | "--help" => {
                println!("tsql - T-SQL server shell

Usage: tsql [-D data_dir [--pool-pages N]] [-U login -P password] [-Q \"query\"] [-i script.sql]

Without -D the database lives in memory only. With -D it is stored in that directory and the buffer pool caches N 8 KiB pages (default 16384 = 128 MiB).

With no arguments an interactive shell starts. Enter T-SQL, then GO to run the batch.
Type EXIT or QUIT to leave.");
                return;
            }
            _ => i += 1,
        }
    }
    if ran {
        return;
    }
    let interactive = io::stdin().is_terminal();
    let stdin = io::stdin();
    let mut buf = String::new();
    let mut line_no = 1;
    let prompt = |n: usize, db: &str| {
        if io::stdin().is_terminal() {
            print!("{}> ", if n == 1 { db.to_string() } else { n.to_string() });
            let _ = io::stdout().flush();
        }
    };
    prompt(line_no, &conn.current_database());
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let t = line.trim();
        let tl = t.to_ascii_lowercase();
        if buf.trim().is_empty() && (tl == "exit" || tl == "quit" || tl == ":quit") {
            break;
        }
        if tl == "go" || tl.starts_with("go ") {
            let cnt = tl[2..].trim().parse::<usize>().unwrap_or(1);
            let batch = std::mem::take(&mut buf);
            if !batch.trim().is_empty() {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                for _ in 0..cnt {
                    print_result(&conn.execute(&batch), &mut out);
                }
            }
            line_no = 1;
        } else {
            buf.push_str(&line);
            buf.push('\n');
            line_no += 1;
        }
        prompt(line_no, &conn.current_database());
    }
    if !buf.trim().is_empty() {
        let stdout = io::stdout();
        print_result(&conn.execute(&buf), &mut stdout.lock());
    }
    let _ = interactive;
}
