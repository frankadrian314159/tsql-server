//! Application shell: menu, navigation pane, tabs, dialogs, status bar.
use std::path::PathBuf;

use egui::{Key, RichText};
use tsql_server::inspect::{ObjectEntry, ObjectKind};
use tsql_server::{tools, Connection, Server, StorageOptions};

use crate::datasheet::Datasheet;
use crate::design::Design;
use crate::query::QueryTab;
use crate::sample::SAMPLE_SQL;
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

pub struct App {
    server: Server,
    conn: Connection,
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
        let conn = server.connect();
        let mut app = App {
            server,
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
        self.conn = self.server.connect();
        self.dir = None;
        match &dir {
            None => {
                self.status = "Started a new in-memory database".into();
                self.error = None;
            }
            Some(d) => match Server::open_with(d, self.opts) {
                Ok(s) => {
                    self.conn = s.connect();
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
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
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
                    ui.separator();
                    if ui.button("Import CSV...").clicked() {
                        ui.close();
                        self.pick_import();
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
                        self.server.checkpoint();
                        self.status = if self.dir.is_some() { "Checkpoint requested".into() } else { "In-memory database: nothing to checkpoint".into() };
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
        });
    }

    fn nav_pane(&mut self, ctx: &egui::Context, ev: &mut Vec<Event>) {
        egui::SidePanel::left("nav").resizable(true).default_width(240.0).show(ctx, |ui| {
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
                                    ObjectKind::Sequence => {}
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
                match &self.dir {
                    Some(p) => ui.label(RichText::new(p.display().to_string()).weak()),
                    None => ui.label(RichText::new("in-memory database").weak()),
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
                    match tools::import_csv(&mut self.conn, d.name.trim(), &d.text, d.create) {
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

        if self.storage_window {
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
                Tab::Datasheet(d) => d.ui(ui, &mut self.conn, &mut ev),
                Tab::Design(d) => d.ui(ui, &mut self.conn, &mut ev),
                Tab::Query(q) => q.ui(ui, &mut self.conn, &mut ev),
            }
        });
        self.dialogs(ctx, &mut ev);
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
        let a = App::with_context(&ctx, Some(dir.path().to_path_buf()), opts);
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
        assert_eq!(tools::import_csv(&mut a.conn, "people", &text, true).unwrap(), 2);
        a.refresh_nav();
        assert!(a.objects.iter().any(|o| o.name == "people"));
    }
}
