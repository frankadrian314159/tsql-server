//! `tsql` — an interactive shell for the tsql-server engine.
//!
//! ```text
//! tsql [DATA_DIR] [-D dir] [--pool-pages N] [-U login -P password] [-d database]
//!      [-Q sql]... [-i script.sql]...
//! ```
//!
//! Type T-SQL and finish a batch with `GO` on its own line. Lines starting with `.` are shell
//! commands (`.help` lists them).
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Instant;
use tsql_server::inspect::{ObjectKind, TableInfo};
use tsql_server::tools;
use tsql_server::{BatchResult, Connection, Event, Server, StorageOptions};

const HELP: &str = "\
Enter T-SQL and finish each batch with GO (GO 5 runs it five times). A transaction left open
shows as * in the prompt. EXIT or QUIT leaves.
Shell commands:
  .databases                 list databases
  .use NAME                  switch database (same as USE NAME)
  .tables | .views | .routines | .sequences
                             list objects of the current database
  .columns TABLE             columns of a table
  .indexes TABLE             indexes, keys and foreign keys of a table
  .schema [NAME]             T-SQL that recreates a table, view, procedure or function (all if no name)
  .import FILE TABLE         append CSV rows (header names matched to columns)
  .import --new FILE TABLE   create TABLE from a CSV file, inferring column types
  .export FILE SQL           write a query's result as CSV
  .read FILE                 run a script (with GO separators)
  .plan SQL                  show how SQL would be executed, without running it
  .optimizer on|off          turn the query optimizer on or off (to compare plans)
  .isolation LEVEL           SET TRANSACTION ISOLATION LEVEL (READ COMMITTED, SNAPSHOT, ...)
  .timer on|off              show how long each batch took
  .storage                   buffer pool and page file statistics
  .locks                     locks currently held
  .checkpoint                write a checkpoint now
  .help                      this text
  .quit                      leave";

const USAGE: &str = "usage: tsql [DATA_DIR] [-D dir] [--pool-pages N] [-U login -P password] [-d database] [-Q sql]... [-i script.sql]...

Without a data directory the database lives in memory only. With one it is stored there and the buffer
pool caches N 8 KiB pages (default 16384 = 128 MiB). -c and -f are accepted as aliases of -Q and -i.";

struct Shell {
    conn: Connection,
    server: Server,
    timer: bool,
    failed: bool,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut data_dir: Option<String> = None;
    let mut user: Option<String> = None;
    let mut pass = String::new();
    let mut database: Option<String> = None;
    let mut opts = StorageOptions::default();
    // scripts to run non-interactively, in order
    let mut jobs: Vec<String> = vec![];
    let bad = |m: &str| {
        eprintln!("{m}\n{USAGE}");
        ExitCode::from(2)
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" | "-?" => {
                println!("{USAGE}\n\n{HELP}");
                return ExitCode::SUCCESS;
            }
            "-D" | "--data" => match args.next() {
                Some(d) => data_dir = Some(d),
                None => return bad("-D needs a directory"),
            },
            "-U" => match args.next() {
                Some(u) => user = Some(u),
                None => return bad("-U needs a login name"),
            },
            "-P" => match args.next() {
                Some(p) => pass = p,
                None => return bad("-P needs a password"),
            },
            "-d" => match args.next() {
                Some(d) => database = Some(d),
                None => return bad("-d needs a database name"),
            },
            "--pool-pages" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) => opts.pool_pages = n,
                None => return bad("--pool-pages needs a number of 8 KiB pages"),
            },
            "-Q" | "-q" | "-c" => match args.next() {
                Some(q) => jobs.push(q),
                None => return bad("-Q needs a query"),
            },
            "-i" | "-f" => match args.next().map(fs::read_to_string) {
                Some(Ok(s)) => jobs.push(s),
                Some(Err(e)) => return bad(&format!("cannot read script: {e}")),
                None => return bad("-i needs a file"),
            },
            other if data_dir.is_none() && !other.starts_with('-') => data_dir = Some(other.to_string()),
            other => return bad(&format!("unknown argument '{other}'")),
        }
    }
    let server = match &data_dir {
        Some(d) => match Server::open_with(d, opts) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot open '{d}': {}", e.message);
                return ExitCode::FAILURE;
            }
        },
        None => Server::new(),
    };
    let conn = match &user {
        Some(u) => match server.connect_as(u, &pass) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Msg {}, Level {}, State {}\n{}", e.number, e.severity, e.state, e.message);
                return ExitCode::FAILURE;
            }
        },
        None => server.connect(),
    };
    let mut sh = Shell { conn, server, timer: false, failed: false };
    if let Some(d) = &database {
        sh.run_sql(&format!("USE {}", tools::bracket(d)));
    }
    if !jobs.is_empty() {
        for j in &jobs {
            sh.run_script(j);
        }
        if sh.conn.transaction_count() > 0 {
            eprintln!("warning: a transaction was still open; it was rolled back, not committed");
            sh.failed = true;
        }
        return if sh.failed { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }
    let interactive = io::stdin().is_terminal();
    if interactive {
        match &data_dir {
            Some(d) => println!("tsql - {d}   (.help for commands, GO runs a batch)"),
            None => println!("tsql - transient in-memory database   (.help for commands, GO runs a batch)"),
        }
    }
    sh.repl(interactive);
    if sh.conn.transaction_count() > 0 {
        eprintln!("warning: a transaction was still open; it was rolled back, not committed");
    }
    ExitCode::SUCCESS
}

impl Shell {
    fn repl(&mut self, interactive: bool) {
        let stdin = io::stdin();
        let mut buf = String::new();
        loop {
            if interactive {
                let open = if self.conn.transaction_count() > 0 { "*" } else { "" };
                if buf.is_empty() {
                    print!("{}{open}> ", self.conn.current_database());
                } else {
                    print!("      ...> ");
                }
                let _ = io::stdout().flush();
            }
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let t = line.trim();
            let lower = t.to_ascii_lowercase();
            if buf.trim().is_empty() {
                if matches!(lower.as_str(), "exit" | "quit" | ":quit") {
                    break;
                }
                if t.starts_with('.') {
                    if !self.dot_command(t) {
                        break;
                    }
                    continue;
                }
            }
            if lower == "go" || lower.starts_with("go ") && lower[2..].trim().parse::<usize>().is_ok() {
                let count = lower[2..].trim().parse::<usize>().unwrap_or(1);
                let batch = std::mem::take(&mut buf);
                if !batch.trim().is_empty() {
                    for _ in 0..count {
                        self.run_sql(&batch);
                    }
                }
                continue;
            }
            buf.push_str(&line);
        }
        if !buf.trim().is_empty() {
            self.run_sql(&buf);
        }
    }

    /// Run a script (batches separated by `GO`).
    fn run_script(&mut self, script: &str) {
        let t = Instant::now();
        let results = self.conn.execute_script(script);
        for r in &results {
            self.show(r);
        }
        self.show_time(t);
    }

    /// Run one batch.
    fn run_sql(&mut self, sql: &str) {
        let t = Instant::now();
        let r = self.conn.execute(sql);
        self.show(&r);
        self.show_time(t);
    }

    fn show_time(&self, t: Instant) {
        if self.timer {
            println!("({:.3} ms)", t.elapsed().as_secs_f64() * 1e3);
        }
    }

    fn show(&mut self, res: &BatchResult) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let mut after_rows = false;
        for ev in &res.events {
            // the grid already ends with its own row count
            let skip = after_rows && matches!(ev, Event::RowCount(_));
            after_rows = matches!(ev, Event::Rows(_));
            if skip {
                continue;
            }
            match ev {
                Event::Rows(rs) => {
                    let _ = writeln!(out, "{}", tools::format_grid(rs, 60));
                }
                Event::Message(m) => {
                    let _ = writeln!(out, "{m}");
                }
                Event::RowCount(n) => {
                    let _ = writeln!(out, "({} row{} affected)\n", n, if *n == 1 { "" } else { "s" });
                }
                Event::Error(e) => {
                    self.failed = true;
                    let _ = out.flush();
                    eprintln!("Msg {}, Level {}, State {}\n{}\n", e.number, e.severity, e.state, e.message);
                }
            }
        }
    }

    /// Run a query and print its grid.
    fn grid(&mut self, sql: &str) {
        self.run_sql(sql);
    }

    fn table(&self, name: &str) -> Option<TableInfo> {
        if name.is_empty() {
            eprintln!("give a table name");
            return None;
        }
        let t = self.conn.table_info(name);
        if t.is_none() {
            eprintln!("no such table '{name}'");
        }
        t
    }

    /// Returns false when the shell should exit.
    fn dot_command(&mut self, line: &str) -> bool {
        let mut parts = line.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        match cmd {
            ".quit" | ".exit" | ".q" => return false,
            ".help" => println!("{HELP}"),
            ".databases" => {
                for d in self.conn.databases() {
                    println!("{d}");
                }
            }
            ".use" => {
                if rest.is_empty() {
                    println!("{}", self.conn.current_database());
                } else {
                    self.run_sql(&format!("USE {}", tools::bracket(rest.trim_matches(|c| c == '[' || c == ']'))));
                }
            }
            ".tables" => self.list(ObjectKind::Table),
            ".views" => self.list(ObjectKind::View),
            ".sequences" => self.list(ObjectKind::Sequence),
            ".routines" => {
                for o in self.conn.objects() {
                    match o.kind {
                        ObjectKind::Procedure => println!("{}  (procedure)", o.qualified()),
                        ObjectKind::Function => println!("{}  (function)", o.qualified()),
                        _ => {}
                    }
                }
            }
            ".columns" => {
                if let Some(t) = self.table(rest) {
                    let mut rows = vec![];
                    for c in &t.columns {
                        let mut notes = vec![];
                        if c.primary_key {
                            notes.push("primary key".to_string());
                        }
                        if let Some((s, i)) = c.identity {
                            notes.push(format!("identity({s},{i})"));
                        }
                        if c.has_default {
                            notes.push("default".into());
                        }
                        if c.computed {
                            notes.push("computed".into());
                        }
                        rows.push(vec![tsql_server::Value::str(&c.name), tsql_server::Value::str(&c.ty), tsql_server::Value::str(if c.nullable { "YES" } else { "NO" }), tsql_server::Value::str(notes.join(", "))]);
                    }
                    let rs = tsql_server::ResultSet { columns: ["column", "type", "nullable", "notes"].map(String::from).to_vec(), types: vec![], rows };
                    println!("{}", tools::format_grid(&rs, 60));
                }
            }
            ".indexes" => {
                if let Some(t) = self.table(rest) {
                    for ix in &t.indexes {
                        let cols: Vec<String> = ix.columns.iter().map(|(c, d)| format!("{c}{}", if *d { " DESC" } else { "" })).collect();
                        let kind = if ix.primary_key { "PRIMARY KEY " } else if ix.unique { "UNIQUE " } else { "" };
                        println!("{kind}{}  ({})", ix.name, cols.join(", "));
                    }
                    for f in &t.foreign_keys {
                        println!("FOREIGN KEY {}  ({}) -> {} ({})", f.name, f.columns.join(", "), f.ref_table, f.ref_columns.join(", "));
                    }
                }
            }
            ".schema" => self.schema(rest),
            ".read" => match fs::read_to_string(rest) {
                Ok(s) => self.run_script(&s),
                Err(e) => eprintln!("cannot read '{rest}': {e}"),
            },
            ".import" => self.import(rest),
            ".export" => self.export(rest),
            ".plan" => match self.conn.explain(rest) {
                Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
                Err(e) => eprintln!("Msg {}, Level {}, State {}\n{}", e.number, e.severity, e.state, e.message),
            },
            ".optimizer" => {
                let on = !rest.eq_ignore_ascii_case("off");
                self.run_sql(&format!("SET OPTIMIZER {}", if on { "ON" } else { "OFF" }));
                println!("optimizer {}", if on { "on" } else { "off" });
            }
            ".isolation" => {
                if rest.is_empty() {
                    eprintln!("usage: .isolation READ UNCOMMITTED | READ COMMITTED | REPEATABLE READ | SERIALIZABLE | SNAPSHOT");
                } else {
                    self.run_sql(&format!("SET TRANSACTION ISOLATION LEVEL {rest}"));
                }
            }
            ".timer" => {
                self.timer = rest.eq_ignore_ascii_case("on");
                println!("timer {}", if self.timer { "on" } else { "off" });
            }
            ".storage" => self.grid("SELECT * FROM sys.dm_os_buffer_pool"),
            ".locks" => self.grid("SELECT * FROM sys.dm_tran_locks"),
            ".checkpoint" => {
                self.server.checkpoint();
                println!("checkpoint requested");
            }
            other => eprintln!("unknown command '{other}' (try .help)"),
        }
        true
    }

    fn list(&self, kind: ObjectKind) {
        for o in self.conn.objects() {
            if o.kind == kind {
                println!("{}", o.qualified());
            }
        }
    }

    fn schema(&self, name: &str) {
        let objs = self.conn.objects();
        let wanted: Vec<_> = if name.is_empty() {
            objs.iter().filter(|o| o.kind != ObjectKind::Sequence).collect()
        } else {
            objs.iter().filter(|o| o.name.eq_ignore_ascii_case(name.rsplit('.').next().unwrap_or(name).trim_matches(|c| c == '[' || c == ']'))).collect()
        };
        if wanted.is_empty() {
            eprintln!("no such object '{name}'");
        }
        for o in wanted {
            match o.kind {
                ObjectKind::Table => {
                    if let Some(t) = self.conn.table_info(&o.qualified()) {
                        println!("{}", tools::script_table(&t));
                    }
                }
                ObjectKind::Sequence => println!("-- sequence {}", o.qualified()),
                _ => {
                    if let Some(d) = self.conn.definition(&o.qualified()) {
                        println!("{}\nGO\n", d.trim_end());
                    }
                }
            }
        }
    }

    fn import(&mut self, rest: &str) {
        let (create, rest) = match rest.strip_prefix("--new") {
            Some(r) => (true, r.trim()),
            None => (false, rest),
        };
        let mut it = rest.split_whitespace();
        let (Some(file), Some(table)) = (it.next(), it.next()) else {
            eprintln!("usage: .import [--new] FILE TABLE");
            return;
        };
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => return eprintln!("cannot read '{file}': {e}"),
        };
        match tools::import_csv(&mut self.conn, table, &text, create) {
            Ok(n) => println!("{n} row{} imported", if n == 1 { "" } else { "s" }),
            Err(e) => {
                self.failed = true;
                eprintln!("Error: {}", e.message);
            }
        }
    }

    fn export(&mut self, rest: &str) {
        let Some((file, sql)) = rest.split_once(char::is_whitespace) else {
            return eprintln!("usage: .export FILE SQL");
        };
        let res = self.conn.execute(sql.trim().trim_end_matches(';'));
        if let Some(e) = res.first_error() {
            self.failed = true;
            return eprintln!("Msg {}, Level {}, State {}\n{}", e.number, e.severity, e.state, e.message);
        }
        match res.resultsets().first() {
            Some(rs) => match fs::write(file, tools::to_csv(rs)) {
                Ok(()) => println!("{} row{} written to {file}", rs.rows.len(), if rs.rows.len() == 1 { "" } else { "s" }),
                Err(e) => eprintln!("cannot write '{file}': {e}"),
            },
            None => eprintln!("the statement returned no result set"),
        }
    }
}
