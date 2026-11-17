//! Application shell: menu, navigation pane, tabs, dialogs, status bar.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use egui::{Key, RichText};
use tsql_server::inspect::{ObjectEntry, ObjectKind};
use tsql_server::{tools, BatchResult, Event as SqlEvent, Server, StorageOptions};

use crate::backend::Backend;
use crate::datasheet::Datasheet;
use crate::remote::{RemoteBackend, Target};
use crate::design::Design;
use crate::query::QueryTab;
use crate::sample::SAMPLE_SQL;
use crate::session::Session;
use crate::ui::{result_grid, Event, ACCENT, ERROR};

enum Tab {
    Datasheet(Datasheet),
    Design(Design),
    Query(QueryTab),
}

impl Tab {
    fn title(&self) -> String {
        match self {
            Tab::Datasheet(d) => d.table.clone(),
            Tab::Design(d) => format!("{} (Design)", d.table),
            Tab::Query(q) => q.title(),
        }
    }

    /// Identity used to re-focus an already open view instead of duplicating it.
    fn key(&self) -> Option<String> {
        match self {
            Tab::Datasheet(d) => Some(format!("ds:{}", d.table.to_lowercase())),
            Tab::Design(d) => Some(format!("design:{}", d.table.to_lowercase())),
            Tab::Query(_) => None,
        }
    }

    fn references(&self, object: &str) -> bool {
        match self {
            Tab::Datasheet(d) => d.table.eq_ignore_ascii_case(object),
            Tab::Design(d) => d.table.eq_ignore_ascii_case(object),
            Tab::Query(_) => false,
        }
    }

    fn mark_dirty(&mut self) {
        match self {
            Tab::Datasheet(d) => d.mark_dirty(),
            Tab::Design(d) => d.mark_dirty(),
            Tab::Query(_) => {}
        }
    }
}

enum Confirm {
    Drop(ObjectEntry),
    /// A transaction is open and the user asked for something that would discard it.
    Transaction(Deferred),
}

/// What to do once the open transaction has been committed or rolled back.
#[derive(Clone)]
enum Deferred {
    /// Open this data directory (`None`: a new in-memory database).
    Open(Option<PathBuf>),
    /// Connect to this server.
    Connect(Target),
    Exit,
}

struct ImportDialog {
    file: String,
    text: String,
    name: String,
    create: bool,
    preview: Vec<Vec<String>>,
    error: Option<String>,
}

/// The "Connect to server" window.
struct ConnectDialog {
    target: Target,
    port: String,
    error: Option<String>,
}

/// "Export backup" and "Import backup".
struct BackupDialog {
    restore: bool,
    path: String,
    database: String,
    /// Write a Microsoft SQL Server `.bak` instead of this server's own format (export only).
    sql_server_format: bool,
    /// Replace the database when it exists (import only).
    replace: bool,
    /// What the file is, read from its first bytes (a file on this machine only).
    detected: Option<String>,
    error: Option<String>,
}

const DELIMITERS: [(&str, char); 4] = [("comma", ','), ("tab", '\t'), ("semicolon", ';'), ("pipe", '|')];

/// "Bulk import": a delimited file of any size into a table.
struct BulkDialog {
    path: String,
    table: String,
    delimiter: usize,
    header: bool,
    create: bool,
    batch: String,
    one_transaction: bool,
    preview: Vec<Vec<String>>,
    error: Option<String>,
}

/// A job that runs on the session's worker thread besides a query tab's script.
struct Task {
    label: String,
    /// Rows done, for a job that counts them.
    progress: Option<Arc<AtomicU64>>,
    cancel: Arc<AtomicBool>,
}

pub struct App {
    /// Things that must live as long as the window: the listener of a server this process started for itself.
    held: Vec<Box<dyn std::any::Any>>,
    server: Server,
    backup: Option<BackupDialog>,
    bulk: Option<BulkDialog>,
    task: Option<Task>,
    /// `host:port` while the session is on a server over TDS (`server` is then an unused empty engine).
    remote: Option<String>,
    connect: Option<ConnectDialog>,
    conn: Session,
    dir: Option<PathBuf>,
    opts: StorageOptions,
    tabs: Vec<Tab>,
    active: usize,
    status: String,
    error: Option<String>,
    objects: Vec<ObjectEntry>,
    databases: Vec<String>,
    current_db: String,
    nav_filter: String,
    confirm: Option<Confirm>,
    import: Option<ImportDialog>,
    about: bool,
    storage_window: bool,
    locks_window: bool,
    isolation: &'static str,
}

const ISOLATION_LEVELS: [(&str, &str); 5] = [
    ("Read uncommitted", "READ UNCOMMITTED"),
    ("Read committed", "READ COMMITTED"),
    ("Repeatable read", "REPEATABLE READ"),
    ("Serializable", "SERIALIZABLE"),
    ("Snapshot", "SNAPSHOT"),
];

fn kind_sql(k: &ObjectKind) -> &'static str {
    match k {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Synonym => "SYNONYM",
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, dir: Option<PathBuf>, opts: StorageOptions) -> Self {
        Self::with_context(&cc.egui_ctx, dir, opts)
    }

    /// Builds the application for an egui context; split from `new` so tests need no window.
    pub fn with_context(ctx: &egui::Context, dir: Option<PathBuf>, opts: StorageOptions) -> Self {
        let mut visuals = egui::Visuals::light();
        visuals.selection.bg_fill = ACCENT;
        visuals.selection.stroke.color = egui::Color32::WHITE;
        visuals.hyperlink_color = ACCENT;
        ctx.set_visuals(visuals);
        let server = Server::new();
        let conn = Session::new(server.connect());
        let mut app = App {
            held: vec![],
            server,
            remote: None,
            connect: None,
            backup: None,
            bulk: None,
            task: None,
            conn,
            dir: None,
            opts,
            tabs: vec![],
            active: 0,
            status: "Ready. Open a data folder from the File menu, or load the sample database from the Database menu.".into(),
            error: None,
            objects: vec![],
            databases: vec![],
            current_db: String::new(),
            nav_filter: String::new(),
            confirm: None,
            import: None,
            about: false,
            storage_window: false,
            locks_window: false,
            isolation: "READ COMMITTED",
        };
        if let Some(d) = dir {
            app.open_dir(Some(d));
        }
        app.refresh_nav();
        app
    }

    // ----- state changes --------------------------------------------------------

    /// Open a data directory, or start a new in-memory database.
    pub fn open_dir(&mut self, dir: Option<PathBuf>) {
        self.tabs.clear();
        self.active = 0;
        // release the old directory first: only one server may have it open
        self.server = Server::new();
        self.conn.replace(self.server.connect());
        self.remote = None;
        self.dir = None;
        match &dir {
            None => {
                self.status = "Started a new in-memory database".into();
                self.error = None;
            }
            Some(d) => match Server::open_with(d, self.opts) {
                Ok(s) => {
                    self.conn.replace(s.connect());
                    self.server = s;
                    self.dir = dir.clone();
                    self.status = format!("Opened {}", d.display());
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("Could not open '{}': {}", d.display(), e.message)),
            },
        }
        self.isolation = "READ COMMITTED";
        self.refresh_nav();
    }

    fn start_open(&mut self, dir: Option<PathBuf>) {
        if self.conn.transaction_count() > 0 {
            self.confirm = Some(Confirm::Transaction(Deferred::Open(dir)));
        } else {
            self.open_dir(dir);
        }
    }

    /// Keep `thing` alive as long as the app.
    pub fn hold(&mut self, thing: impl std::any::Any) {
        self.held.push(Box::new(thing));
    }

    /// Connect at start-up: when the server does not take the login, the connect window opens with the reason, ready for another try.
    pub fn connect_or_ask(&mut self, t: Target) {
        self.connect = Some(ConnectDialog { port: t.port.to_string(), target: t.clone(), error: None });
        self.start_connect(t);
    }

    /// Connect to a server over TDS (asks first when a transaction is open).
    pub fn start_connect(&mut self, t: Target) {
        if self.conn.transaction_count() > 0 {
            self.confirm = Some(Confirm::Transaction(Deferred::Connect(t)));
        } else {
            self.connect_remote(t);
        }
    }

    /// Replace the session by one on a server; on failure the dialog (or the error line) says why and nothing changes.
    pub fn connect_remote(&mut self, t: Target) -> bool {
        match RemoteBackend::connect(&t) {
            Ok(b) => {
                self.tabs.clear();
                self.active = 0;
                self.server = Server::new();
                self.remote = Some(format!("{}:{}", t.host, t.port));
                self.conn.replace(b);
                self.dir = None;
                self.status = format!("Connected to {}:{}", t.host, t.port);
                self.error = None;
                self.connect = None;
                self.isolation = "READ COMMITTED";
                self.refresh_nav();
                true
            }
            Err(e) => {
                match self.connect.as_mut() {
                    Some(d) => d.error = Some(e),
                    None => self.error = Some(e),
                }
                false
            }
        }
    }

    /// Start a job on the session's worker thread; its outcome is shown when it finishes (`finish_task`).
    fn start_task(&mut self, ctx: &egui::Context, label: String, progress: Option<Arc<AtomicU64>>, cancel: Arc<AtomicBool>, job: impl FnOnce(&mut dyn Backend) -> Vec<BatchResult> + Send + 'static) {
        let c = ctx.clone();
        self.conn.start_with(job, move || c.request_repaint());
        self.status = label.clone();
        self.error = None;
        self.task = Some(Task { label, progress, cancel });
    }

    /// The job has finished: the first error, or what it said, goes to the status bar.
    fn finish_task(&mut self, results: &[BatchResult], task: Task) {
        let error = results.iter().find_map(|r| r.first_error());
        match error {
            Some(e) => self.error = Some(format!("{} failed: Msg {}: {}", task.label, e.number, e.message)),
            None => {
                let said = results.iter().flat_map(|r| r.events.iter()).filter_map(|e| if let SqlEvent::Message(m) = e { Some(m.clone()) } else { None }).last();
                self.status = said.unwrap_or_else(|| format!("{}: done", task.label));
                self.error = None;
            }
        }
        self.after_change();
    }

    fn open_backup(&mut self, restore: bool) {
        let db = if restore { String::new() } else { self.current_db.clone() };
        self.backup = Some(BackupDialog { restore, path: String::new(), database: db, sql_server_format: false, replace: false, detected: None, error: None });
    }

    /// What kind of backup the file is, from its first bytes.
    fn detect_backup(path: &str) -> Option<String> {
        use std::io::Read;
        let mut head = [0u8; 4096];
        let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).ok()?;
        if head[..n].starts_with(b"TSQLBAK1") {
            Some("a tsql-server backup".into())
        } else if tsql_server::mssqlbak::is_bak(&head[..n]) || tsql_server::mtf::is_mtf(&head[..n]) {
            Some("a Microsoft SQL Server backup".into())
        } else {
            Some("not a backup this server reads".into())
        }
    }

    /// Start the export or import the dialog describes (the dialog stays open, with the reason, when it cannot).
    fn run_backup(&mut self, ctx: &egui::Context) {
        let d = self.backup.as_mut().expect("the dialog is open");
        if d.path.trim().is_empty() || d.database.trim().is_empty() {
            d.error = Some("Give a file and a database".into());
        } else {
            let path = d.path.trim().replace('\'', "''");
            let db = tools::bracket(d.database.trim());
            let (sql, label) = if d.restore {
                (format!("RESTORE DATABASE {db} FROM DISK = N'{path}'{}", if d.replace { " WITH REPLACE" } else { "" }), format!("Importing backup into {}", d.database.trim()))
            } else {
                (format!("BACKUP DATABASE {db} TO DISK = N'{path}'{}", if d.sql_server_format { " WITH SQL_SERVER_FORMAT" } else { "" }), format!("Exporting {} to a backup", d.database.trim()))
            };
            self.backup = None;
            self.start_task(ctx, label, None, Arc::new(AtomicBool::new(false)), move |c| c.execute_script(&sql));
        }
    }

    /// Start the bulk import the dialog describes.
    fn run_bulk(&mut self, ctx: &egui::Context) {
        let d = self.bulk.as_mut().expect("the dialog is open");
        let batch = d.batch.trim().parse::<usize>().ok().filter(|n| *n > 0);
        if d.path.trim().is_empty() || d.table.trim().is_empty() {
            d.error = Some("Give a file and a table".into());
        } else if batch.is_none() {
            d.error = Some("Rows per batch must be a positive number".into());
        } else {
            match std::fs::File::open(d.path.trim()) {
                Err(e) => d.error = Some(format!("Cannot read the file: {e}")),
                Ok(f) => {
                    let table = d.table.trim().to_string();
                    let opts = tools::BulkOptions { delimiter: DELIMITERS[d.delimiter].1, header: d.header, create: d.create, batch: batch.unwrap_or(1000), one_transaction: d.one_transaction };
                    let progress = Arc::new(AtomicU64::new(0));
                    let cancel = Arc::new(AtomicBool::new(false));
                    let (p, c, t) = (progress.clone(), cancel.clone(), table.clone());
                    self.bulk = None;
                    self.start_task(ctx, format!("Importing into {table}"), Some(progress), cancel, move |conn| {
                        let r = tools::bulk_import(conn, &t, std::io::BufReader::with_capacity(1 << 20, f), &opts, &mut |n| {
                            p.store(n, Ordering::Relaxed);
                            !c.load(Ordering::Relaxed)
                        });
                        let ev = match r {
                            Ok(n) => SqlEvent::Message(format!("Imported {n} rows into {t}")),
                            Err(e) => SqlEvent::Error(e),
                        };
                        vec![BatchResult { events: vec![ev] }]
                    });
                }
            }
        }
    }

    fn open_bulk(&mut self) {
        self.bulk = Some(BulkDialog { path: String::new(), table: String::new(), delimiter: 0, header: true, create: true, batch: "1000".into(), one_transaction: true, preview: vec![], error: None });
    }

    /// Read the first records of the file for the preview, and suggest a table name.
    fn bulk_preview(d: &mut BulkDialog) {
        d.preview.clear();
        d.error = None;
        match std::fs::File::open(&d.path) {
            Ok(f) => {
                let mut rd = tools::CsvReader::new(std::io::BufReader::new(f), DELIMITERS[d.delimiter].1);
                for _ in 0..6 {
                    match rd.next_record() {
                        Ok(Some(r)) => d.preview.push(r),
                        Ok(None) => break,
                        Err(e) => {
                            d.error = Some(e);
                            break;
                        }
                    }
                }
                if d.table.trim().is_empty() {
                    if let Some(stem) = std::path::Path::new(&d.path).file_stem() {
                        d.table = stem.to_string_lossy().into_owned();
                    }
                }
            }
            Err(e) => d.error = Some(format!("Cannot read the file: {e}")),
        }
    }

    pub fn refresh_nav(&mut self) {
        self.objects = self.conn.objects();
        self.databases = self.conn.databases();
        self.current_db = self.conn.current_database();
    }

    fn open_tab(&mut self, tab: Tab) {
        if let Some(k) = tab.key() {
            if let Some(i) = self.tabs.iter().position(|t| t.key().as_deref() == Some(&k)) {
                self.active = i;
                return;
            }
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Run a script (with `GO` separators); the first error is shown.
    pub fn run_script(&mut self, sql: &str, ok: &str) -> bool {
        let mut failed = false;
        for r in self.conn.execute_script(sql) {
            if let Some(e) = r.first_error() {
                self.error = Some(format!("Msg {}: {}", e.number, e.message));
                failed = true;
                break;
            }
        }
        if !failed {
            self.status = ok.to_string();
            self.error = None;
        }
        self.after_change();
        !failed
    }

    fn after_change(&mut self) {
        self.refresh_nav();
        self.tabs.iter_mut().for_each(Tab::mark_dirty);
    }

    fn tran(&mut self, sql: &str, ok: &str) -> bool {
        match self.conn.run(sql) {
            Ok(_) => {
                self.status = ok.to_string();
                self.error = None;
                self.after_change();
                true
            }
            Err(e) => {
                self.error = Some(e.message);
                false
            }
        }
    }

    fn begin_tx(&mut self) {
        self.tran("BEGIN TRAN", "Transaction started: nothing is permanent until you commit");
    }

    fn commit_tx(&mut self) -> bool {
        self.tran("COMMIT", "Transaction committed")
    }

    fn rollback_tx(&mut self) -> bool {
        self.tran("ROLLBACK", "Transaction rolled back")
    }

    fn set_isolation(&mut self, level: &'static str) {
        if self.tran(&format!("SET TRANSACTION ISOLATION LEVEL {level}"), &format!("Isolation level: {level}")) {
            self.isolation = level;
        }
    }

    fn handle(&mut self, events: Vec<Event>) {
        for e in events {
            match e {
                Event::Changed => {
                    self.refresh_nav();
                    self.tabs.iter_mut().for_each(Tab::mark_dirty);
                }
                Event::Error(m) => self.error = Some(m),
                Event::Status(m) => {
                    self.status = m;
                    self.error = None;
                }
                Event::OpenDatasheet(t) => self.open_tab(Tab::Datasheet(Datasheet::new(t))),
                Event::OpenDesign(t) => self.open_tab(Tab::Design(Design::new(t))),
                Event::OpenQuery { name, sql } => self.open_tab(Tab::Query(QueryTab::new(name, sql))),
            }
        }
    }

    fn export_active(&mut self) {
        let data: Option<(Vec<String>, Vec<Vec<tsql_server::Value>>, String)> = match self.tabs.get(self.active) {
            Some(Tab::Datasheet(d)) => match self.conn.execute(&format!("SELECT * FROM {}", tools::quote_name(&d.table))).resultsets().first() {
                Some(rs) => Some((rs.columns.clone(), rs.rows.clone(), d.table.clone())),
                None => None,
            },
            Some(Tab::Query(q)) => q.shown_results().map(|r| (r.columns.clone(), r.rows.clone(), "result".to_string())),
            _ => None,
        };
        let Some((columns, rows, name)) = data else {
            self.error = Some("Open a datasheet, or run a query, before exporting.".into());
            return;
        };
        let Some(file) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name(format!("{name}.csv")).save_file() else { return };
        let rs = tsql_server::ResultSet { columns, types: vec![], rows };
        match std::fs::write(&file, tools::to_csv(&rs)) {
            Ok(()) => self.status = format!("Exported {} row(s) to {}", rs.rows.len(), file.display()),
            Err(e) => self.error = Some(format!("Export failed: {e}")),
        }
    }

    fn pick_import(&mut self) {
        let Some(file) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() else { return };
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                let preview = tools::parse_csv(&text).map(|r| r.into_iter().take(6).collect()).unwrap_or_default();
                let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "imported".into());
                self.import = Some(ImportDialog { file: file.display().to_string(), text, name: stem, create: true, preview, error: None });
            }
            Err(e) => self.error = Some(format!("Cannot read file: {e}")),
        }
    }

    // ----- UI pieces ----------------------------------------------------------

    fn menu(&mut self, ctx: &egui::Context, ev: &mut Vec<Event>) {
        let busy = self.conn.is_busy();
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| self.menu_contents(ui, ctx, ev));
        });
    }

    fn menu_contents(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, ev: &mut Vec<Event>) {
        {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open data folder...").clicked() {
                        ui.close();
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.start_open(Some(p));
                        }
                    }
                    if ui.button("New in-memory database").clicked() {
                        ui.close();
                        self.start_open(None);
                    }
                    if ui.button("Connect to server...").on_hover_text("Work on a server over TDS: a cluster's control node or any SQL Server work-alike").clicked() {
                        ui.close();
                        self.connect = Some(ConnectDialog { target: Target::default(), port: "1433".into(), error: None });
                    }
                    ui.separator();
                    if ui.button("Import backup (.bak)...").on_hover_text("Restore a database from a backup file: this server's own format or a Microsoft SQL Server .bak").clicked() {
                        ui.close();
                        self.open_backup(true);
                    }
                    if ui.button("Export backup (.bak)...").on_hover_text("Back a database up to a file, in this server's own format or as a Microsoft SQL Server .bak").clicked() {
                        ui.close();
                        self.open_backup(false);
                    }
                    ui.separator();
                    if ui.button("Import CSV...").clicked() {
                        ui.close();
                        self.pick_import();
                    }
                    if ui.button("Bulk import...").on_hover_text("Load a large delimited file into a table, a batch of rows at a time").clicked() {
                        ui.close();
                        self.open_bulk();
                    }
                    if ui.button("Export current grid to CSV...").clicked() {
                        ui.close();
                        self.export_active();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Create", |ui| {
                    if ui.button("Query  (Ctrl+N)").clicked() {
                        ui.close();
                        ev.push(Event::OpenQuery { name: None, sql: String::new() });
                    }
                    if ui.button("Table").clicked() {
                        ui.close();
                        ev.push(Event::OpenQuery { name: None, sql: "CREATE TABLE dbo.NewTable (\n    Id INT IDENTITY(1,1) PRIMARY KEY,\n    Name NVARCHAR(100) NOT NULL\n)".into() });
                    }
                    if ui.button("View").clicked() {
                        ui.close();
                        ev.push(Event::OpenQuery { name: None, sql: "CREATE VIEW dbo.NewView AS\n    SELECT * FROM dbo.TableName".into() });
                    }
                    if ui.button("Stored procedure").clicked() {
                        ui.close();
                        ev.push(Event::OpenQuery { name: None, sql: "CREATE PROCEDURE dbo.NewProc @Id INT AS\nBEGIN\n    SELECT * FROM dbo.TableName WHERE Id = @Id\nEND".into() });
                    }
                    if ui.button("Trigger").clicked() {
                        ui.close();
                        ev.push(Event::OpenQuery { name: None, sql: "CREATE TRIGGER dbo.NewTrigger ON dbo.TableName AFTER INSERT AS\nBEGIN\n    SELECT * FROM inserted\nEND".into() });
                    }
                });
                ui.menu_button("Database", |ui| {
                    let open = self.conn.transaction_count() > 0;
                    if ui.add_enabled(!open, egui::Button::new("Begin transaction")).clicked() {
                        ui.close();
                        self.begin_tx();
                    }
                    if ui.add_enabled(open, egui::Button::new("Commit")).clicked() {
                        ui.close();
                        self.commit_tx();
                    }
                    if ui.add_enabled(open, egui::Button::new("Roll back")).clicked() {
                        ui.close();
                        self.rollback_tx();
                    }
                    ui.menu_button("Isolation level", |ui| {
                        for (label, level) in ISOLATION_LEVELS {
                            if ui.radio(self.isolation == level, label).clicked() {
                                ui.close();
                                self.set_isolation(level);
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Checkpoint").on_hover_text("Write all changes to the data files and empty the log").clicked() {
                        ui.close();
                        if self.remote.is_some() {
                            self.run_script("CHECKPOINT", "Checkpoint requested");
                        } else {
                            self.server.checkpoint();
                            self.status = if self.dir.is_some() { "Checkpoint requested".into() } else { "In-memory database: nothing to checkpoint".into() };
                        }
                    }
                    if ui.button("Storage statistics...").clicked() {
                        ui.close();
                        self.storage_window = true;
                    }
                    if ui.button("Locks...").clicked() {
                        ui.close();
                        self.locks_window = true;
                    }
                    ui.separator();
                    if ui.button("Load sample database").clicked() {
                        ui.close();
                        self.run_script(SAMPLE_SQL, "Sample database loaded");
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About").clicked() {
                        ui.close();
                        self.about = true;
                    }
                });
            });
        }
    }

    fn nav_pane(&mut self, ctx: &egui::Context, ev: &mut Vec<Event>) {
        let busy = self.conn.is_busy();
        egui::SidePanel::left("nav").resizable(true).default_width(240.0).show(ctx, |ui| {
            if busy {
                ui.disable();
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Database").strong().color(ACCENT));
                let mut switch: Option<String> = None;
                egui::ComboBox::from_id_salt("db").selected_text(self.current_db.clone()).show_ui(ui, |ui| {
                    for d in &self.databases {
                        if ui.selectable_label(d.eq_ignore_ascii_case(&self.current_db), d).clicked() {
                            switch = Some(d.clone());
                        }
                    }
                });
                if let Some(d) = switch {
                    if self.run_script(&format!("USE {}", tools::bracket(&d)), &format!("Now using database {d}")) {
                        self.tabs.clear();
                        self.active = 0;
                    }
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.nav_filter).hint_text("filter objects").desired_width(f32::INFINITY));
            ui.separator();
            let filter = self.nav_filter.to_lowercase();
            let objects: Vec<ObjectEntry> = self.objects.iter().filter(|o| filter.is_empty() || o.name.to_lowercase().contains(&filter)).cloned().collect();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (title, kind) in [("Tables", ObjectKind::Table), ("Views", ObjectKind::View), ("Procedures", ObjectKind::Procedure), ("Functions", ObjectKind::Function), ("Sequences", ObjectKind::Sequence)] {
                    let items: Vec<&ObjectEntry> = objects.iter().filter(|o| o.kind == kind).collect();
                    egui::CollapsingHeader::new(RichText::new(format!("{title} ({})", items.len())).strong()).default_open(matches!(kind, ObjectKind::Table | ObjectKind::View)).show(ui, |ui| {
                        for o in &items {
                            let q = o.qualified();
                            let label = if o.schema.eq_ignore_ascii_case("dbo") { o.name.clone() } else { q.clone() };
                            let r = ui.selectable_label(false, label);
                            if r.double_clicked() {
                                match kind {
                                    ObjectKind::Table | ObjectKind::View => ev.push(Event::OpenDatasheet(q.clone())),
                                    ObjectKind::Procedure | ObjectKind::Function => {
                                        if let Some(d) = self.conn.definition(&q) {
                                            ev.push(Event::OpenQuery { name: Some(o.name.clone()), sql: d });
                                        }
                                    }
                                    ObjectKind::Sequence | ObjectKind::Synonym => {}
                                }
                            }
                            r.context_menu(|ui| {
                                match kind {
                                    ObjectKind::Table | ObjectKind::View => {
                                        if ui.button("Open").clicked() {
                                            ev.push(Event::OpenDatasheet(q.clone()));
                                            ui.close();
                                        }
                                    }
                                    _ => {}
                                }
                                if kind == ObjectKind::Table {
                                    if ui.button("Design").clicked() {
                                        ev.push(Event::OpenDesign(q.clone()));
                                        ui.close();
                                    }
                                    if ui.button("Script").clicked() {
                                        if let Some(t) = self.conn.table_info(&q) {
                                            ev.push(Event::OpenQuery { name: None, sql: tools::script_table(&t) });
                                        }
                                        ui.close();
                                    }
                                    if ui.button("Select top 100").clicked() {
                                        ev.push(Event::OpenQuery { name: None, sql: format!("SELECT TOP 100 * FROM {}", tools::quote_name(&q)) });
                                        ui.close();
                                    }
                                }
                                if matches!(kind, ObjectKind::View | ObjectKind::Procedure | ObjectKind::Function) && ui.button("Show definition").clicked() {
                                    if let Some(d) = self.conn.definition(&q) {
                                        ev.push(Event::OpenQuery { name: Some(o.name.clone()), sql: d });
                                    }
                                    ui.close();
                                }
                                if ui.button("Delete...").clicked() {
                                    self.confirm = Some(Confirm::Drop((*o).clone()));
                                    ui.close();
                                }
                            });
                        }
                        if items.is_empty() {
                            ui.label(RichText::new("(none)").weak());
                        }
                    });
                }
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.conn.is_busy() {
            ui.disable();
        }
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            for (i, t) in self.tabs.iter().enumerate() {
                if ui.selectable_label(self.active == i, t.title()).clicked() {
                    self.active = i;
                }
                if ui.small_button("x").on_hover_text("Close").clicked() {
                    close = Some(i);
                }
                ui.add_space(8.0);
            }
        });
        if let Some(i) = close {
            self.tabs.remove(i);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match (&self.remote, &self.dir) {
                    (Some(a), _) => ui.label(RichText::new(format!("server {a}")).weak()),
                    (None, Some(p)) => ui.label(RichText::new(p.display().to_string()).weak()),
                    (None, None) => ui.label(RichText::new("in-memory database").weak()),
                };
                ui.separator();
                ui.label(RichText::new(format!("{}  ·  {}", self.conn.login(), self.isolation.to_lowercase())).weak());
                ui.separator();
                ui.label(&self.status);
                let n = self.conn.transaction_count();
                if n > 0 {
                    ui.separator();
                    ui.label(RichText::new(format!("Transaction open ({n})")).strong().color(ACCENT));
                    if ui.small_button("Commit").clicked() {
                        self.commit_tx();
                    }
                    if ui.small_button("Roll back").clicked() {
                        self.rollback_tx();
                    }
                }
            });
            if let Some(e) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(ERROR, e);
                    if ui.small_button("dismiss").clicked() {
                        self.error = None;
                    }
                });
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context, ev: &mut Vec<Event>) {
        // ---- CSV import
        if self.import.is_some() {
            let mut open = true;
            let mut go = false;
            let tables: Vec<String> = self.objects.iter().filter(|o| o.kind == ObjectKind::Table).map(|o| o.qualified()).collect();
            egui::Window::new("Import CSV").open(&mut open).collapsible(false).show(ctx, |ui| {
                if let Some(d) = &mut self.import {
                    ui.label(RichText::new(&d.file).weak());
                    ui.add_space(4.0);
                    ui.radio_value(&mut d.create, true, "Create a new table");
                    ui.radio_value(&mut d.create, false, "Append to an existing table");
                    ui.horizontal(|ui| {
                        ui.label("Table:");
                        ui.text_edit_singleline(&mut d.name);
                        if !d.create {
                            egui::ComboBox::from_id_salt("imp_tables").selected_text("choose").show_ui(ui, |ui| {
                                for t in &tables {
                                    if ui.selectable_label(false, t).clicked() {
                                        d.name = t.clone();
                                    }
                                }
                            });
                        }
                    });
                    ui.label(RichText::new("The first row names the columns. Everything is imported, or nothing is.").weak());
                    if !d.preview.is_empty() {
                        ui.add_space(4.0);
                        egui::Grid::new("imp_preview").striped(true).show(ui, |ui| {
                            for (i, r) in d.preview.iter().enumerate() {
                                for c in r {
                                    let t: String = c.chars().take(24).collect();
                                    ui.label(if i == 0 { RichText::new(t).strong() } else { RichText::new(t) });
                                }
                                ui.end_row();
                            }
                        });
                    }
                    if let Some(e) = &d.error {
                        ui.colored_label(ERROR, e);
                    }
                    if ui.button("Import").clicked() {
                        go = true;
                    }
                }
            });
            if go {
                if let Some(d) = &mut self.import {
                    match tools::import_csv(&mut *self.conn, d.name.trim(), &d.text, d.create) {
                        Ok(n) => {
                            let name = d.name.trim().to_string();
                            self.import = None;
                            self.status = format!("Imported {n} row(s) into '{name}'");
                            self.error = None;
                            self.refresh_nav();
                            let q = self.conn.table_info(&name).map(|t| format!("{}.{}", t.schema, t.name)).unwrap_or(name);
                            ev.push(Event::Changed);
                            ev.push(Event::OpenDatasheet(q));
                        }
                        Err(e) => d.error = Some(e.message),
                    }
                }
            } else if !open {
                self.import = None;
            }
        }

        // ---- a job on the worker thread (bulk import, backup, restore)
        if let Some(t) = &self.task {
            let done = t.progress.as_ref().map(|p| p.load(Ordering::Relaxed));
            let label = t.label.clone();
            let cancel = t.cancel.clone();
            let counted = t.progress.is_some();
            egui::Window::new("Working...").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label(&label);
                match done {
                    Some(n) => {
                        ui.label(format!("{n} rows so far"));
                    }
                    None => {
                        ui.spinner();
                    }
                }
                if counted && ui.button("Stop").on_hover_text("Stop the import; a single transaction is rolled back").clicked() {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // ---- import or export a backup
        if self.backup.is_some() {
            let mut go = false;
            let mut cancel = false;
            let remote = self.remote.is_some();
            let dbs = self.databases.clone();
            if let Some(d) = self.backup.as_mut() {
                let title = if d.restore { "Import backup" } else { "Export backup" };
                egui::Window::new(title).collapsible(false).resizable(false).show(ctx, |ui| {
                    egui::Grid::new("backup").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label(if remote { "File on the server" } else { "File" });
                        ui.horizontal(|ui| {
                            if ui.add(egui::TextEdit::singleline(&mut d.path).desired_width(340.0)).changed() && d.restore && !remote {
                                d.detected = Self::detect_backup(&d.path);
                            }
                            if !remote && ui.button("Browse...").clicked() {
                                let dlg = rfd::FileDialog::new().add_filter("Backup", &["bak"]);
                                let pick = if d.restore { dlg.pick_file() } else { dlg.set_file_name(format!("{}.bak", d.database)).save_file() };
                                if let Some(p) = pick {
                                    d.path = p.display().to_string();
                                    if d.restore {
                                        d.detected = Self::detect_backup(&d.path);
                                        if d.database.trim().is_empty() {
                                            d.database = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                        }
                                    }
                                }
                            }
                        });
                        ui.end_row();
                        if d.restore {
                            ui.label("Restore as database");
                            ui.text_edit_singleline(&mut d.database);
                            ui.end_row();
                            ui.label("The file is");
                            ui.label(d.detected.clone().unwrap_or_else(|| if remote { "read by the server".into() } else { "(choose a file)".into() }));
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut d.replace, "Replace the database if it exists");
                            ui.end_row();
                        } else {
                            ui.label("Database");
                            egui::ComboBox::from_id_salt("backup_db").selected_text(d.database.clone()).show_ui(ui, |ui| {
                                for n in &dbs {
                                    ui.selectable_value(&mut d.database, n.clone(), n);
                                }
                            });
                            ui.end_row();
                            ui.label("Format");
                            ui.vertical(|ui| {
                                ui.radio_value(&mut d.sql_server_format, false, "tsql-server (this server's own format)");
                                ui.radio_value(&mut d.sql_server_format, true, "Microsoft SQL Server .bak");
                            });
                            ui.end_row();
                        }
                    });
                    if d.restore {
                        ui.label(RichText::new("Either kind of file is read: the format is told from the first bytes of the file.").weak());
                    }
                    if let Some(e) = &d.error {
                        ui.colored_label(ERROR, e);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(if d.restore { "Import" } else { "Export" }).clicked() {
                            go = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            }
            if cancel {
                self.backup = None;
            } else if go {
                self.run_backup(ctx);
            }
        }

        // ---- bulk import
        if self.bulk.is_some() {
            let mut go = false;
            let mut cancel = false;
            let tables: Vec<String> = self.objects.iter().filter(|o| o.kind == ObjectKind::Table).map(|o| o.qualified()).collect();
            if let Some(d) = self.bulk.as_mut() {
                egui::Window::new("Bulk import").collapsible(false).resizable(false).show(ctx, |ui| {
                    egui::Grid::new("bulk").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label("File");
                        ui.horizontal(|ui| {
                            let changed = ui.add(egui::TextEdit::singleline(&mut d.path).desired_width(340.0)).changed();
                            let mut picked = false;
                            if ui.button("Browse...").clicked() {
                                if let Some(p) = rfd::FileDialog::new().add_filter("Delimited text", &["csv", "tsv", "txt", "tbl", "dat"]).pick_file() {
                                    d.path = p.display().to_string();
                                    d.table.clear();
                                    picked = true;
                                }
                            }
                            if changed || picked {
                                Self::bulk_preview(d);
                            }
                        });
                        ui.end_row();
                        ui.label("Table");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut d.table).desired_width(240.0));
                            egui::ComboBox::from_id_salt("bulk_table").selected_text("existing...").show_ui(ui, |ui| {
                                for t in &tables {
                                    if ui.selectable_label(false, t).clicked() {
                                        d.table = t.clone();
                                    }
                                }
                            });
                        });
                        ui.end_row();
                        ui.label("Delimiter");
                        let before = d.delimiter;
                        egui::ComboBox::from_id_salt("bulk_delim").selected_text(DELIMITERS[d.delimiter].0).show_ui(ui, |ui| {
                            for (i, (name, _)) in DELIMITERS.iter().enumerate() {
                                ui.selectable_value(&mut d.delimiter, i, *name);
                            }
                        });
                        if before != d.delimiter && !d.path.is_empty() {
                            Self::bulk_preview(d);
                        }
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut d.header, "The first line names the columns");
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut d.create, "Create the table when it does not exist (types are inferred from the first 1000 lines)");
                        ui.end_row();
                        ui.label("Rows per batch");
                        ui.add(egui::TextEdit::singleline(&mut d.batch).desired_width(80.0));
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut d.one_transaction, "All or nothing (one transaction); otherwise each batch commits by itself");
                        ui.end_row();
                    });
                    if !d.preview.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new("First lines").strong());
                        for r in &d.preview {
                            let line = r.iter().map(|c| c.chars().take(24).collect::<String>()).collect::<Vec<_>>().join("  |  ");
                            ui.label(RichText::new(line.chars().take(110).collect::<String>()).monospace());
                        }
                    }
                    if let Some(e) = &d.error {
                        ui.colored_label(ERROR, e);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Import").clicked() {
                            go = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            }
            if cancel {
                self.bulk = None;
            } else if go {
                self.run_bulk(ctx);
            }
        }

        // ---- connect to a server
        if self.connect.is_some() {
            let mut go = false;
            let mut cancel = false;
            if let Some(d) = self.connect.as_mut() {
                egui::Window::new("Connect to server").collapsible(false).resizable(false).show(ctx, |ui| {
                    egui::Grid::new("connect").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label("Server");
                        ui.text_edit_singleline(&mut d.target.host);
                        ui.end_row();
                        ui.label("Port");
                        ui.text_edit_singleline(&mut d.port);
                        ui.end_row();
                        ui.label("Login");
                        ui.text_edit_singleline(&mut d.target.login);
                        ui.end_row();
                        ui.label("Password");
                        let pw = ui.add(egui::TextEdit::singleline(&mut d.target.password).password(true));
                        ui.end_row();
                        ui.label("Database");
                        ui.text_edit_singleline(&mut d.target.database);
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut d.target.encrypt, "Encrypt (TLS)");
                        ui.end_row();
                        if pw.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            go = true;
                        }
                    });
                    if let Some(e) = &d.error {
                        ui.colored_label(ERROR, e);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Connect").clicked() {
                            go = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            }
            if cancel {
                self.connect = None;
            } else if go {
                let t = self.connect.as_mut().and_then(|d| match d.port.trim().parse::<u16>() {
                    Ok(p) => {
                        d.target.port = p;
                        d.error = None;
                        Some(d.target.clone())
                    }
                    Err(_) => {
                        d.error = Some("The port must be a number".into());
                        None
                    }
                });
                if let Some(t) = t {
                    self.start_connect(t);
                }
            }
        }

        // ---- open transaction
        if matches!(self.confirm, Some(Confirm::Transaction(_))) {
            #[derive(Clone, Copy)]
            enum Choice {
                Commit,
                RollBack,
                Cancel,
            }
            let mut choice = None;
            egui::Window::new("Transaction in progress").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("A transaction is open; its changes are not permanent yet.");
                ui.label("Commit keeps them; Roll back discards them.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Commit").clicked() {
                        choice = Some(Choice::Commit);
                    }
                    if ui.button("Roll back").clicked() {
                        choice = Some(Choice::RollBack);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(Choice::Cancel);
                    }
                });
            });
            if let Some(choice) = choice {
                let Some(Confirm::Transaction(next)) = self.confirm.take() else { return };
                let finished = match choice {
                    Choice::Commit => self.commit_tx(),
                    Choice::RollBack => self.rollback_tx(),
                    Choice::Cancel => false,
                };
                if finished {
                    match next {
                        Deferred::Open(dir) => self.open_dir(dir),
                        Deferred::Connect(t) => {
                            self.connect_remote(t);
                        }
                        Deferred::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    }
                }
            }
        }

        // ---- delete an object
        if let Some(Confirm::Drop(o)) = &self.confirm {
            let o = o.clone();
            let mut decision: Option<bool> = None;
            egui::Window::new("Confirm delete").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label(format!("Permanently delete the {} '{}'? This cannot be undone (unless a transaction is open).", kind_sql(&o.kind).to_lowercase(), o.qualified()));
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => {
                    self.confirm = None;
                    let q = o.qualified();
                    self.tabs.retain(|t| !t.references(&q));
                    self.active = self.active.min(self.tabs.len().saturating_sub(1));
                    self.run_script(&format!("DROP {} {}", kind_sql(&o.kind), tools::quote_name(&q)), &format!("Deleted '{q}'"));
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }

        if self.storage_window && self.remote.is_some() {
            let mut open = true;
            egui::Window::new("Storage statistics").open(&mut open).collapsible(false).show(ctx, |ui| {
                ui.label(RichText::new("The storage engine of a server is not visible over the wire; its metrics are on the server's /metrics endpoint.").weak());
            });
            self.storage_window = open;
        } else if self.storage_window {
            let (data, temp) = self.server.storage_stats();
            egui::Window::new("Storage statistics").open(&mut self.storage_window).collapsible(false).show(ctx, |ui| {
                egui::Grid::new("storage").striped(true).spacing([20.0, 4.0]).show(ui, |ui| {
                    ui.label("");
                    ui.label(RichText::new("database").strong());
                    ui.label(RichText::new("temporary").strong());
                    ui.end_row();
                    let rows: [(&str, u64, u64); 8] = [
                        ("pool size (pages)", data.pool_capacity as u64, temp.pool_capacity as u64),
                        ("pages cached", data.pages_cached as u64, temp.pages_cached as u64),
                        ("pages in use", data.pages_live as u64, temp.pages_live as u64),
                        ("file size (pages)", data.file_blocks, temp.file_blocks),
                        ("free pages in file", data.free_blocks as u64, temp.free_blocks as u64),
                        ("page reads", data.reads, temp.reads),
                        ("page writes", data.writes, temp.writes),
                        ("cache hits / evictions", data.hits, data.evictions),
                    ];
                    for (l, a, b) in rows {
                        ui.label(l);
                        ui.label(a.to_string());
                        ui.label(b.to_string());
                        ui.end_row();
                    }
                });
                ui.label(RichText::new("A pool size of 0 means the store is unbounded (in-memory database).").weak());
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        if self.locks_window {
            let res = self.conn.execute("SELECT * FROM sys.dm_tran_locks");
            let mut open = true;
            egui::Window::new("Locks").open(&mut open).default_width(560.0).show(ctx, |ui| match res.resultsets().first() {
                Some(rs) if !rs.rows.is_empty() => result_grid(ui, "locks", &rs.columns, &rs.rows),
                _ => {
                    ui.label(RichText::new("No locks are held right now.").weak());
                }
            });
            self.locks_window = open;
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        if self.about {
            egui::Window::new("About tsql-gui").collapsible(false).resizable(false).open(&mut self.about).show(ctx, |ui| {
                ui.label("tsql-server — a Microsoft SQL Server work-alike with a T-SQL engine, written in Rust.");
                ui.label("Paged durable storage, lock-based isolation levels, permissions and a query optimizer.");
            });
        }
    }

    /// One frame of the application.
    pub fn frame(&mut self, ctx: &egui::Context) {
        let mut ev: Vec<Event> = Vec::new();

        // a script that was running on the worker thread has finished: hand the results to its tab
        if let Some(fin) = self.conn.poll() {
            if let Some(task) = self.task.take() {
                self.finish_task(&fin.results, task);
            } else {
                let mut fin = Some(fin);
                for t in self.tabs.iter_mut() {
                    if let (Tab::Query(q), true) = (&mut *t, fin.is_some()) {
                        if q.running {
                            q.finish(fin.take().unwrap(), &mut ev);
                        }
                    }
                }
            }
        }
        if self.conn.is_lost() {
            // the worker panicked and took the connection with it: start a new session on the same server
            self.conn.replace(self.server.connect());
            for t in self.tabs.iter_mut() {
                if let Tab::Query(q) = t {
                    q.running = false;
                }
            }
            self.task = None;
            self.error = Some("The running script crashed its session; a new session was started (an open transaction was lost).".into());
        }
        if self.conn.is_busy() {
            // keep the elapsed time ticking
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Closing the window must not silently throw a transaction away.
        if ctx.input(|i| i.viewport().close_requested()) && self.conn.transaction_count() > 0 && !matches!(self.confirm, Some(Confirm::Transaction(_))) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Transaction(Deferred::Exit));
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::N)) {
            ev.push(Event::OpenQuery { name: None, sql: String::new() });
        }

        self.menu(ctx, &mut ev);
        self.nav_pane(ctx, &mut ev);
        self.status_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.tabs.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.heading(RichText::new("tsql-server").color(ACCENT));
                    ui.label("Double-click a table in the navigation pane to open it, or press Ctrl+N for a new query.");
                    ui.label(RichText::new("File > Open data folder... keeps your data on disk; Database > Load sample database gives you something to explore.").weak());
                });
                return;
            }
            self.tab_bar(ui);
            ui.separator();
            let i = self.active.min(self.tabs.len() - 1);
            match &mut self.tabs[i] {
                Tab::Datasheet(_) | Tab::Design(_) if self.conn.is_busy() => {
                    ui.label("Waiting for the running script to finish...");
                }
                Tab::Datasheet(d) => d.ui(ui, &mut *self.conn, &mut ev),
                Tab::Design(d) => d.ui(ui, &mut *self.conn, &mut ev),
                Tab::Query(q) => q.ui(ui, &mut self.conn, &mut ev),
            }
        });
        if !self.conn.is_busy() {
            self.dialogs(ctx, &mut ev);
        }
        self.handle(ev);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (egui::Context, App) {
        let ctx = egui::Context::default();
        let a = App::with_context(&ctx, None, StorageOptions { pool_pages: 64, temp_pool_pages: 16 });
        (ctx, a)
    }

    /// Run `n` frames headlessly, like a window that is open.
    fn frames(ctx: &egui::Context, a: &mut App, n: usize) {
        for _ in 0..n {
            let _ = ctx.run(egui::RawInput::default(), |ctx| a.frame(ctx));
        }
    }

    #[test]
    fn a_running_script_does_not_block_the_window() {
        let (ctx, mut a) = app();
        let mut q = QueryTab::new(None, "DECLARE @i INT = 0; WHILE @i < 600000 SET @i += 1; SELECT 42 AS answer".into());
        q.start(&mut a.conn, &ctx);
        a.tabs.push(Tab::Query(q));
        assert!(a.conn.is_busy());
        // frames are drawn while the script runs, quickly, and the session reports its cached state
        let t = std::time::Instant::now();
        frames(&ctx, &mut a, 5);
        assert!(t.elapsed() < std::time::Duration::from_millis(800), "frames must not wait for the script");
        assert!(a.conn.is_busy());
        assert_eq!(a.conn.transaction_count(), 0);
        // when it finishes, the results reach the tab and the session is usable again
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while a.conn.is_busy() && std::time::Instant::now() < deadline {
            frames(&ctx, &mut a, 1);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        frames(&ctx, &mut a, 1);
        assert!(!a.conn.is_busy());
        match &a.tabs[0] {
            Tab::Query(q) => {
                assert!(!q.running);
                assert_eq!(q.shown_results().unwrap().rows[0][0].to_string(), "42");
            }
            _ => unreachable!(),
        }
        assert_eq!(a.conn.query("SELECT 1").unwrap().len(), 1);
    }

    #[test]
    fn a_transaction_started_in_a_background_script_stays_with_the_session() {
        let (ctx, mut a) = app();
        a.run_script("CREATE TABLE bg (x INT)", "ok");
        let mut q = QueryTab::new(None, "BEGIN TRAN; INSERT bg VALUES (1)".into());
        q.start(&mut a.conn, &ctx);
        let fin = a.conn.wait().expect("finished");
        q.finish(fin, &mut vec![]);
        assert_eq!(a.conn.transaction_count(), 1);
        assert_eq!(a.conn.query("SELECT COUNT(*) FROM bg").unwrap()[0][0].to_string(), "1");
    }

    #[test]
    fn sample_database_fills_the_navigation_pane() {
        let (ctx, mut a) = app();
        frames(&ctx, &mut a, 2);
        assert!(a.objects.is_empty());
        assert!(a.run_script(SAMPLE_SQL, "loaded"));
        let names: Vec<String> = a.objects.iter().map(|o| o.qualified()).collect();
        for n in ["dbo.Customers", "dbo.OrderLines", "dbo.OrderTotals", "dbo.CustomerOrders"] {
            assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
        }
        assert_eq!(a.conn.query("SELECT COUNT(*) FROM dbo.OrderTotals").unwrap()[0][0].to_string(), "4");
        // every kind of tab draws without trouble
        a.handle(vec![Event::OpenDatasheet("dbo.Customers".into()), Event::OpenDatasheet("dbo.OrderTotals".into()), Event::OpenDesign("dbo.Orders".into()), Event::OpenQuery { name: None, sql: "SELECT * FROM dbo.Products".into() }]);
        assert_eq!(a.tabs.len(), 4);
        for i in 0..a.tabs.len() {
            a.active = i;
            frames(&ctx, &mut a, 2);
        }
        // opening the same table again just switches to its tab
        a.handle(vec![Event::OpenDatasheet("dbo.customers".into())]);
        assert_eq!(a.tabs.len(), 4);
        assert_eq!(a.active, 0);
        assert!(a.error.is_none(), "{:?}", a.error);
    }

    #[test]
    fn transactions_commit_and_roll_back_from_the_menu_actions() {
        let (ctx, mut a) = app();
        a.run_script("CREATE TABLE t (a INT)", "ok");
        a.begin_tx();
        assert_eq!(a.conn.transaction_count(), 1);
        a.run_script("INSERT t VALUES (1)", "ok");
        frames(&ctx, &mut a, 1);
        assert!(a.rollback_tx());
        assert_eq!(a.conn.query("SELECT COUNT(*) FROM t").unwrap()[0][0].to_string(), "0");
        a.begin_tx();
        a.run_script("INSERT t VALUES (2)", "ok");
        assert!(a.commit_tx());
        assert_eq!(a.conn.query("SELECT COUNT(*) FROM t").unwrap()[0][0].to_string(), "1");
        // asking to leave with a transaction open is a question, not a silent rollback
        a.begin_tx();
        a.start_open(None);
        assert!(matches!(a.confirm, Some(Confirm::Transaction(Deferred::Open(None)))));
        frames(&ctx, &mut a, 1);
        assert_eq!(a.conn.transaction_count(), 1, "nothing was discarded");
    }

    #[test]
    fn isolation_level_and_database_switching() {
        let (_ctx, mut a) = app();
        a.set_isolation("SNAPSHOT");
        assert_eq!(a.isolation, "SNAPSHOT");
        a.set_isolation("SERIALIZABLE");
        assert_eq!(a.isolation, "SERIALIZABLE");
        assert!(a.databases.iter().any(|d| d == "tempdb"));
        assert!(a.run_script("USE tempdb", "ok"));
        assert_eq!(a.current_db, "tempdb");
    }

    #[test]
    fn opening_a_data_directory_keeps_data_and_reports_a_busy_directory() {
        let dir = tempfile::tempdir().unwrap();
        let opts = StorageOptions { pool_pages: 64, temp_pool_pages: 16 };
        let ctx = egui::Context::default();
        {
            let mut a = App::with_context(&ctx, Some(dir.path().to_path_buf()), opts);
            assert!(a.dir.is_some() && a.error.is_none());
            a.run_script("CREATE TABLE keep (a INT)\nGO\nINSERT keep VALUES (7)", "ok");
            // a second window on the same directory is refused with a message
            let mut b = App::with_context(&ctx, Some(dir.path().to_path_buf()), opts);
            assert!(b.dir.is_none() && b.error.as_deref().is_some_and(|e| e.contains("in use")), "{:?}", b.error);
            b.open_dir(None);
            assert!(b.error.is_none());
        }
        let mut a = App::with_context(&ctx, Some(dir.path().to_path_buf()), opts);
        assert_eq!(a.conn.table_info("keep").unwrap().approx_rows, 1);
    }

    #[test]
    fn deleting_an_object_closes_its_tabs() {
        let (ctx, mut a) = app();
        a.run_script(SAMPLE_SQL, "ok");
        a.handle(vec![Event::OpenDatasheet("dbo.OrderTotals".into()), Event::OpenQuery { name: None, sql: String::new() }]);
        let view = a.objects.iter().find(|o| o.name == "OrderTotals").unwrap().clone();
        a.confirm = Some(Confirm::Drop(view));
        frames(&ctx, &mut a, 1);
        assert!(a.confirm.is_some(), "waits for the answer");
        // answer the dialog the way the button would
        a.confirm = None;
        a.tabs.retain(|t| !t.references("dbo.OrderTotals"));
        assert!(a.run_script("DROP VIEW [dbo].[OrderTotals]", "deleted"));
        assert!(a.objects.iter().all(|o| o.name != "OrderTotals"));
        assert_eq!(a.tabs.len(), 1);
    }

    #[test]
    fn import_dialog_creates_a_table_from_csv() {
        let (ctx, mut a) = app();
        let text = "id,name\n1,Ann\n2,Bob\n".to_string();
        a.import = Some(ImportDialog { file: "x.csv".into(), text: text.clone(), name: "people".into(), create: true, preview: tools::parse_csv(&text).unwrap(), error: None });
        frames(&ctx, &mut a, 2);
        assert_eq!(tools::import_csv(&mut *a.conn, "people", &text, true).unwrap(), 2);
        a.refresh_nav();
        assert!(a.objects.iter().any(|o| o.name == "people"));
    }

    /// Let the worker thread finish the job (a backup, a bulk import) while frames are drawn.
    fn finish_task(ctx: &egui::Context, a: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while a.task.is_some() && std::time::Instant::now() < deadline {
            frames(ctx, a, 1);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(a.task.is_none(), "the job did not finish");
    }

    #[test]
    fn backups_are_exported_and_imported_in_both_formats() {
        let (ctx, mut a) = app();
        assert!(a.run_script("CREATE DATABASE shop
GO
USE shop
GO
CREATE TABLE dbo.t (id INT PRIMARY KEY, s NVARCHAR(20))
GO
INSERT dbo.t VALUES (1, N'one'), (2, N'it''s two')", "ok"));
        let dir = tempfile::tempdir().unwrap();
        for (sql_server, name, kind) in [(false, "native", "a tsql-server backup"), (true, "mssql", "a Microsoft SQL Server backup")] {
            let path = dir.path().join(format!("{name}.bak"));
            a.open_backup(false);
            {
                let d = a.backup.as_mut().unwrap();
                d.database = "shop".into();
                d.path = path.display().to_string();
                d.sql_server_format = sql_server;
            }
            a.run_backup(&ctx);
            assert!(a.backup.is_none() && a.task.is_some());
            finish_task(&ctx, &mut a);
            assert!(a.error.is_none(), "{:?}", a.error);
            assert_eq!(App::detect_backup(&path.display().to_string()).as_deref(), Some(kind));
            // import it as a new database; either kind of file is read
            a.open_backup(true);
            {
                let d = a.backup.as_mut().unwrap();
                d.path = path.display().to_string();
                d.database = format!("copy_{name}");
            }
            a.run_backup(&ctx);
            finish_task(&ctx, &mut a);
            assert!(a.error.is_none(), "{:?}", a.error);
            assert!(a.databases.iter().any(|d| d == &format!("copy_{name}")), "{:?}", a.databases);
            let rows = a.conn.query(&format!("SELECT COUNT(*), MAX(s) FROM [copy_{name}].dbo.t")).unwrap();
            assert_eq!(rows[0][0].to_string(), "2");
            assert_eq!(rows[0][1].to_string(), "one");
        }
        // an import over an existing database needs REPLACE; a bad file is reported
        a.open_backup(true);
        {
            let d = a.backup.as_mut().unwrap();
            d.path = dir.path().join("native.bak").display().to_string();
            d.database = "copy_native".into();
        }
        a.run_backup(&ctx);
        finish_task(&ctx, &mut a);
        assert!(a.error.as_deref().is_some_and(|e| e.contains("failed")), "{:?}", a.error);
        a.open_backup(true);
        {
            let d = a.backup.as_mut().unwrap();
            d.path = dir.path().join("native.bak").display().to_string();
            d.database = "copy_native".into();
            d.replace = true;
        }
        a.run_backup(&ctx);
        finish_task(&ctx, &mut a);
        assert!(a.error.is_none(), "{:?}", a.error);
        a.open_backup(true);
        a.backup.as_mut().unwrap().path.clear();
        a.run_backup(&ctx);
        assert!(a.backup.as_ref().unwrap().error.is_some(), "the dialog stays open and says what is missing");
    }

    #[test]
    fn bulk_import_loads_a_big_file_on_the_worker_thread() {
        let (ctx, mut a) = app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.tsv");
        let mut text = String::from("id	name
");
        for i in 1..=5000 {
            text.push_str(&format!("{i}	row {i}
"));
        }
        std::fs::write(&path, text).unwrap();
        a.open_bulk();
        {
            let d = a.bulk.as_mut().unwrap();
            d.path = path.display().to_string();
            App::bulk_preview(d);
            assert_eq!(d.table, "big", "the table is named after the file");
            assert!(d.preview.iter().all(|r| r.len() == 1) && d.preview[0] == vec!["id	name"], "a comma file: one field per line");
            d.delimiter = 1;
            App::bulk_preview(d);
            assert_eq!(d.preview[1], vec!["1", "row 1"]);
            d.batch = "500".into();
        }
        a.run_bulk(&ctx);
        assert!(a.bulk.is_none() && a.task.is_some());
        finish_task(&ctx, &mut a);
        assert!(a.error.is_none(), "{:?}", a.error);
        assert!(a.status.contains("5000"), "{}", a.status);
        assert_eq!(a.conn.query("SELECT COUNT(*), MAX(name) FROM big WHERE id > 4990").unwrap()[0][0].to_string(), "10");
        // a bad value stops the import and takes it all back
        std::fs::write(&path, "id	name
9001	fine
bad	oops
").unwrap();
        a.open_bulk();
        {
            let d = a.bulk.as_mut().unwrap();
            d.path = path.display().to_string();
            d.table = "big".into();
            d.delimiter = 1;
        }
        a.run_bulk(&ctx);
        finish_task(&ctx, &mut a);
        assert!(a.error.as_deref().is_some_and(|e| e.contains("record 3")), "{:?}", a.error);
        assert_eq!(a.conn.query("SELECT COUNT(*) FROM big").unwrap()[0][0].to_string(), "5000");
    }
}
