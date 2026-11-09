//! Query view: a T-SQL editor with result grids, messages and the optimizer's plan.
use std::path::PathBuf;

use egui::{Key, RichText, TextEdit};
use tsql_server::{tools, Connection, Event as SqlEvent, ResultSet, SqlError};

use crate::session::{Finished, Session};
use crate::ui::{result_grid, Event, ERROR};

/// What a batch produced, in order.
enum Item {
    Rows(ResultSet),
    Message(String),
    Error(SqlError),
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Pane {
    Results(usize),
    Messages,
    Plan,
}

pub struct QueryTab {
    /// The object whose definition the tab was opened from, for the tab title.
    pub name: Option<String>,
    pub sql: String,
    file: Option<PathBuf>,
    items: Vec<Item>,
    pane: Pane,
    elapsed_ms: Option<f64>,
    plan: Vec<String>,
    /// Selected text of the editor as a character range, from the last frame.
    selection: Option<(usize, usize)>,
    ran_selection: bool,
    /// A script of this tab is running on the session's worker thread.
    pub running: bool,
}

impl QueryTab {
    pub fn new(name: Option<String>, sql: String) -> Self {
        QueryTab { name, sql, file: None, items: vec![], pane: Pane::Messages, elapsed_ms: None, plan: vec![], selection: None, ran_selection: false, running: false }
    }

    pub fn title(&self) -> String {
        match (&self.file, &self.name) {
            (Some(f), _) => f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Query".into()),
            (None, Some(n)) => n.clone(),
            _ => "Query".into(),
        }
    }

    fn sets(&self) -> Vec<&ResultSet> {
        self.items.iter().filter_map(|i| if let Item::Rows(r) = i { Some(r) } else { None }).collect()
    }

    /// The result set on show, for exporting.
    pub fn shown_results(&self) -> Option<&ResultSet> {
        match self.pane {
            Pane::Results(i) => self.sets().get(i).copied(),
            _ => self.sets().first().copied(),
        }
    }

    pub fn error_count(&self) -> usize {
        self.items.iter().filter(|i| matches!(i, Item::Error(_))).count()
    }

    /// The text to run: the selection if there is one, otherwise everything.
    fn text_to_run(&self) -> String {
        match self.selection {
            Some((a, b)) if b > a => self.sql.chars().skip(a).take(b - a).collect(),
            _ => self.sql.clone(),
        }
    }

    /// Start running the selection (or the whole editor) on the session's worker thread; batches are separated
    /// by `GO`. The results arrive through [`QueryTab::finish`].
    pub fn start(&mut self, sess: &mut Session, ctx: &egui::Context) {
        let text = self.text_to_run();
        self.ran_selection = self.selection.is_some_and(|(a, b)| b > a);
        if text.trim().is_empty() || sess.is_busy() {
            return;
        }
        self.running = true;
        let ctx = ctx.clone();
        sess.start(text, move || ctx.request_repaint());
    }

    /// Take the results of the finished script.
    pub fn finish(&mut self, fin: Finished, ev: &mut Vec<Event>) {
        self.running = false;
        let results = fin.results;
        self.elapsed_ms = Some(fin.elapsed.as_secs_f64() * 1e3);
        self.items.clear();
        self.plan.clear();
        for r in results {
            let mut after_rows = false;
            for e in r.events {
                match e {
                    SqlEvent::Rows(rs) => {
                        self.items.push(Item::Rows(rs));
                        after_rows = true;
                        continue;
                    }
                    SqlEvent::Message(m) => self.items.push(Item::Message(m)),
                    SqlEvent::RowCount(n) => {
                        if !after_rows {
                            self.items.push(Item::Message(format!("({} row{} affected)", n, if n == 1 { "" } else { "s" })));
                        }
                    }
                    SqlEvent::Error(e) => self.items.push(Item::Error(e)),
                }
                after_rows = false;
            }
        }
        self.pane = if self.error_count() > 0 || self.sets().is_empty() { Pane::Messages } else { Pane::Results(0) };
        // any statement may have changed the catalog or the data
        ev.push(Event::Changed);
    }

    /// Describe the plan of the selection (or everything) without running it.
    pub fn explain(&mut self, conn: &mut Connection) {
        let text = self.text_to_run();
        self.items.clear();
        match conn.explain(&text) {
            Ok(lines) => {
                self.plan = if lines.is_empty() { vec!["(no plan: the batch contains no query)".to_string()] } else { lines };
                self.pane = Pane::Plan;
            }
            Err(e) => {
                self.plan.clear();
                self.items.push(Item::Error(e));
                self.pane = Pane::Messages;
            }
        }
    }

    fn message_lines(&self) -> Vec<(String, bool)> {
        let mut v = vec![];
        for i in &self.items {
            match i {
                Item::Message(m) => v.push((m.clone(), false)),
                Item::Error(e) => v.push((format!("Msg {}, Level {}, State {}\n{}", e.number, e.severity, e.state, e.message), true)),
                Item::Rows(_) => {}
            }
        }
        if let Some(ms) = self.elapsed_ms {
            let sets = self.sets();
            let rows: usize = sets.iter().map(|s| s.rows.len()).sum();
            v.push((format!("Completed in {ms:.2} ms ({} result set(s), {rows} row(s))", sets.len()), false));
        }
        v
    }

    fn open_file(&mut self) -> Option<String> {
        let f = rfd::FileDialog::new().add_filter("SQL script", &["sql", "txt"]).pick_file()?;
        match std::fs::read_to_string(&f) {
            Ok(s) => {
                self.sql = s;
                self.file = Some(f);
                None
            }
            Err(e) => Some(format!("Cannot read '{}': {e}", f.display())),
        }
    }

    fn save_file(&mut self) -> Option<String> {
        let target = match &self.file {
            Some(f) => f.clone(),
            None => rfd::FileDialog::new().add_filter("SQL script", &["sql"]).set_file_name("query.sql").save_file()?,
        };
        match std::fs::write(&target, &self.sql) {
            Ok(()) => {
                self.file = Some(target);
                None
            }
            Err(e) => Some(format!("Cannot write '{}': {e}", target.display())),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, conn: &mut Session, ev: &mut Vec<Event>) {
        ui.push_id(("query", self.name.clone()), |ui| self.show(ui, conn, ev));
    }

    fn show(&mut self, ui: &mut egui::Ui, conn: &mut Session, ev: &mut Vec<Event>) {
        let busy = conn.is_busy();
        let shortcut = !busy && ui.input(|i| (i.modifiers.command && i.key_pressed(Key::Enter)) || i.key_pressed(Key::F5));
        let mut run = shortcut;
        let mut explain = false;
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Run  (Ctrl+Enter)")).on_hover_text("Runs the selected text, or everything if nothing is selected").clicked() {
                run = true;
            }
            if ui.add_enabled(!busy, egui::Button::new("Explain")).on_hover_text("Show the optimizer's plan without running the query").clicked() {
                explain = true;
            }
            if let (true, Some(d)) = (self.running, conn.running_for()) {
                ui.spinner();
                ui.label(RichText::new(format!("Running... {:.1} s", d.as_secs_f64())).strong());
            }
            ui.separator();
            if ui.button("Open...").clicked() {
                if let Some(e) = self.open_file() {
                    ev.push(Event::Error(e));
                }
            }
            if ui.button("Save...").clicked() {
                if let Some(e) = self.save_file() {
                    ev.push(Event::Error(e));
                }
            }
            if ui.button("Clear").clicked() {
                self.sql.clear();
                self.items.clear();
                self.plan.clear();
            }
            ui.separator();
            let has = self.shown_results().is_some();
            if ui.add_enabled(has, egui::Button::new("Export CSV...")).clicked() {
                if let Some(rs) = self.shown_results() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("result.csv").save_file() {
                        match std::fs::write(&f, tools::to_csv(rs)) {
                            Ok(()) => ev.push(Event::Status(format!("Exported {} row(s) to {}", rs.rows.len(), f.display()))),
                            Err(e) => ev.push(Event::Error(format!("Export failed: {e}"))),
                        }
                    }
                }
            }
            if self.selection.is_some_and(|(a, b)| b > a) {
                ui.label(RichText::new("selection only").weak());
            }
        });
        let out = TextEdit::multiline(&mut self.sql).code_editor().desired_rows(10).desired_width(f32::INFINITY).hint_text("SELECT ... FROM ...   (GO separates batches)").show(ui);
        if let Some(r) = out.cursor_range {
            let (a, b) = (r.primary.index, r.secondary.index);
            self.selection = if a != b { Some((a.min(b), a.max(b))) } else { None };
        }
        if explain && !busy {
            self.explain(conn);
        } else if run {
            self.start(conn, ui.ctx());
        }
        ui.separator();

        let sets: Vec<(usize, usize)> = self.sets().iter().enumerate().map(|(i, s)| (i, s.rows.len())).collect();
        let msgs = self.message_lines();
        ui.horizontal(|ui| {
            for (i, n) in &sets {
                if ui.selectable_label(self.pane == Pane::Results(*i), format!("Results {} ({} row{})", i + 1, n, if *n == 1 { "" } else { "s" })).clicked() {
                    self.pane = Pane::Results(*i);
                }
            }
            if ui.selectable_label(self.pane == Pane::Messages, format!("Messages ({})", msgs.len())).clicked() {
                self.pane = Pane::Messages;
            }
            if !self.plan.is_empty() && ui.selectable_label(self.pane == Pane::Plan, "Plan").clicked() {
                self.pane = Pane::Plan;
            }
            if let Some(ms) = self.elapsed_ms {
                ui.label(RichText::new(format!("{ms:.2} ms")).weak());
            }
        });
        match self.pane {
            Pane::Results(i) => match self.sets().get(i) {
                Some(rs) => result_grid(ui, "query_results", &rs.columns, &rs.rows),
                None => {
                    ui.label(RichText::new("Write a statement and press Run.").weak());
                }
            },
            Pane::Messages => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if msgs.is_empty() {
                        ui.label(RichText::new("Write a statement and press Run. Use GO on its own line to separate batches.").weak());
                    }
                    for (m, is_err) in &msgs {
                        if *is_err {
                            ui.colored_label(ERROR, m);
                        } else {
                            ui.label(m);
                        }
                    }
                });
            }
            Pane::Plan => {
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    for l in &self.plan {
                        ui.label(RichText::new(l).monospace());
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsql_server::Server;

    fn conn() -> Session {
        Session::new(Server::new().connect())
    }

    /// Run the tab's script to completion, as the app does over several frames.
    fn run(q: &mut QueryTab, s: &mut Session, ev: &mut Vec<Event>) {
        q.start(s, &egui::Context::default());
        if let Some(f) = s.wait() {
            q.finish(f, ev);
        }
    }

    #[test]
    fn running_collects_result_sets_messages_and_errors() {
        let mut c = conn();
        let mut ev = vec![];
        let mut q = QueryTab::new(None, "CREATE TABLE t (a INT)\nGO\nINSERT t VALUES (1),(2)\nGO\nPRINT 'hello'\nSELECT a FROM t\nSELECT 1/0".into());
        run(&mut q, &mut c, &mut ev);
        assert!(matches!(ev.as_slice(), [Event::Changed]));
        assert_eq!(q.sets().len(), 1);
        assert_eq!(q.sets()[0].rows.len(), 2);
        assert_eq!(q.error_count(), 1);
        assert_eq!(q.pane, Pane::Messages, "errors take you to the messages");
        let lines = q.message_lines();
        assert!(lines.iter().any(|(m, e)| !*e && m == "(2 rows affected)"));
        assert!(lines.iter().any(|(m, e)| !*e && m == "hello"));
        assert!(lines.iter().any(|(m, e)| *e && m.contains("Divide by zero")));
        assert!(lines.last().unwrap().0.starts_with("Completed in"));
        assert_eq!(q.shown_results().unwrap().rows.len(), 2);
    }

    #[test]
    fn a_clean_run_shows_the_first_grid_and_a_selection_runs_alone() {
        let mut c = conn();
        let mut ev = vec![];
        let mut q = QueryTab::new(None, "SELECT 1 AS a; SELECT 2 AS b".into());
        run(&mut q, &mut c, &mut ev);
        assert_eq!(q.pane, Pane::Results(0));
        assert_eq!(q.sets().len(), 2);
        // select just "SELECT 2 AS b"
        q.selection = Some((15, 28));
        assert_eq!(q.text_to_run(), "SELECT 2 AS b");
        run(&mut q, &mut c, &mut ev);
        assert_eq!(q.sets().len(), 1);
        assert_eq!(q.sets()[0].columns, vec!["b"]);
        assert!(q.ran_selection);
    }

    #[test]
    fn explain_shows_the_plan_without_running() {
        let mut c = conn();
        let mut ev = vec![];
        let mut q = QueryTab::new(None, "CREATE TABLE t (id INT PRIMARY KEY, v INT)\nGO\nINSERT t VALUES (1, 1)".into());
        run(&mut q, &mut c, &mut ev);
        q.sql = "SELECT * FROM t WHERE id = 1".into();
        q.explain(&mut c);
        assert_eq!(q.pane, Pane::Plan);
        assert!(q.plan.iter().any(|l| l.contains("index seek")), "{:?}", q.plan);
        q.sql = "SELECT * FROM missing".into();
        q.explain(&mut c);
        assert_eq!(q.error_count(), 1);
        assert_eq!(q.pane, Pane::Messages);
    }
}
