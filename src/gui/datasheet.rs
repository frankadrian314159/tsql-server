//! Datasheet view: browse a table or view, and edit a table that has a primary key.
use egui::{Key, RichText, Sense, TextEdit};
use egui_extras::{Column, TableBuilder};
use tsql_server::inspect::TableInfo;
use tsql_server::{tools, Connection, Value};

use crate::ui::{display, Event, ERROR};

const LIMITS: [usize; 4] = [100, 1000, 10_000, 100_000];

struct Edit {
    row: usize,
    col: usize,
    text: String,
    focused: bool,
}

pub struct Datasheet {
    /// `schema.name` as listed in the navigation pane.
    pub table: String,
    info: Option<TableInfo>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    total: Option<i64>,
    limit: usize,
    filter: String,
    applied_filter: String,
    sort: Option<(usize, bool)>,
    dirty: bool,
    error: Option<String>,
    editing: Option<Edit>,
    selected: Option<usize>,
    confirm_delete: bool,
    new_row: Vec<String>,
}

impl Datasheet {
    pub fn new(table: String) -> Self {
        Datasheet { table, info: None, columns: vec![], rows: vec![], total: None, limit: 1000, filter: String::new(), applied_filter: String::new(), sort: None, dirty: true, error: None, editing: None, selected: None, confirm_delete: false, new_row: vec![] }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Editing needs a primary key to say which row a change is for.
    pub fn editable(&self) -> bool {
        self.info.as_ref().is_some_and(|i| !i.primary_key().is_empty())
    }

    /// Read the rows again (and the table's structure, which may have changed).
    pub fn load(&mut self, conn: &mut Connection) {
        self.dirty = false;
        self.error = None;
        self.editing = None;
        self.confirm_delete = false;
        self.info = conn.table_info(&self.table);
        let name = tools::quote_name(&self.table);
        let wh = if self.applied_filter.trim().is_empty() { String::new() } else { format!(" WHERE {}", self.applied_filter) };
        let order = match self.sort {
            Some((c, desc)) if c < self.columns.len() => format!(" ORDER BY {}{}", tools::bracket(&self.columns[c]), if desc { " DESC" } else { "" }),
            _ => String::new(),
        };
        let res = conn.execute(&format!("SELECT TOP ({}) * FROM {}{}{}", self.limit, name, wh, order));
        if let Some(e) = res.first_error() {
            self.error = Some(e.message.clone());
            self.rows.clear();
            return;
        }
        if let Some(rs) = res.resultsets().first() {
            self.columns = rs.columns.clone();
            self.rows = rs.rows.clone();
        }
        self.selected = self.selected.filter(|&r| r < self.rows.len());
        self.total = conn.query(&format!("SELECT COUNT_BIG(*) FROM {}{}", name, wh)).ok().and_then(|r| r.first().and_then(|r| r[0].to_string().parse().ok()));
        if self.new_row.len() != self.columns.len() {
            self.new_row = vec![String::new(); self.columns.len()];
        }
    }

    fn where_for_row(&self, row: usize) -> Option<String> {
        let info = self.info.as_ref()?;
        let mut parts = vec![];
        for (i, c) in self.columns.iter().enumerate() {
            if info.columns.iter().any(|ci| ci.primary_key && ci.name.eq_ignore_ascii_case(c)) {
                let v = self.rows.get(row)?.get(i)?;
                parts.push(format!("{} = {}", tools::bracket(c), tools::sql_literal(v)));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" AND "))
        }
    }

    /// Change one cell. `text` is what the user typed; an empty value is NULL for a nullable column.
    pub fn commit_edit(&mut self, conn: &mut Connection, row: usize, col: usize, text: &str) -> Result<(), String> {
        let info = self.info.as_ref().ok_or("this object cannot be edited")?;
        let ci = info.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&self.columns[col])).ok_or("unknown column")?;
        if ci.computed || ci.identity.is_some() || ci.ty == "timestamp" {
            return Err(format!("column '{}' is generated and cannot be edited", ci.name));
        }
        let lit = tools::literal_for_column(text, &ci.ty, ci.nullable)?;
        let wh = self.where_for_row(row).ok_or("this table has no primary key, so its rows cannot be edited")?;
        let sql = format!("UPDATE {} SET {} = {} WHERE {}", info.quoted(), tools::bracket(&ci.name), lit, wh);
        conn.run(&sql).map_err(|e| e.message)?;
        Ok(())
    }

    pub fn delete_row(&mut self, conn: &mut Connection, row: usize) -> Result<(), String> {
        let info = self.info.as_ref().ok_or("this object cannot be edited")?;
        let wh = self.where_for_row(row).ok_or("this table has no primary key, so its rows cannot be deleted")?;
        conn.run(&format!("DELETE FROM {} WHERE {}", info.quoted(), wh)).map_err(|e| e.message)?;
        Ok(())
    }

    /// Insert the values typed in the new-row boxes; boxes left empty get the column's default.
    pub fn insert_new_row(&mut self, conn: &mut Connection) -> Result<(), String> {
        let info = self.info.as_ref().ok_or("this object cannot be edited")?;
        let mut cols = vec![];
        let mut vals = vec![];
        for (i, text) in self.new_row.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let ci = info.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&self.columns[i])).ok_or("unknown column")?;
            vals.push(tools::literal_for_column(text, &ci.ty, ci.nullable).map_err(|e| format!("{}: {e}", ci.name))?);
            cols.push(tools::bracket(&ci.name));
        }
        let sql = if cols.is_empty() { format!("INSERT {} DEFAULT VALUES", info.quoted()) } else { format!("INSERT {} ({}) VALUES ({})", info.quoted(), cols.join(", "), vals.join(", ")) };
        conn.run(&sql).map_err(|e| e.message)?;
        self.new_row = vec![String::new(); self.columns.len()];
        Ok(())
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, conn: &mut Connection, ev: &mut Vec<Event>) {
        ui.push_id(("datasheet", self.table.clone()), |ui| self.show(ui, conn, ev));
    }

    fn show(&mut self, ui: &mut egui::Ui, conn: &mut Connection, ev: &mut Vec<Event>) {
        if self.dirty {
            self.load(conn);
        }
        let mut reload = false;
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                reload = true;
            }
            ui.label("Filter:");
            let r = ui.add(TextEdit::singleline(&mut self.filter).hint_text("WHERE condition, e.g. amount > 100").desired_width(260.0));
            if (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || ui.button("Apply").clicked() {
                self.applied_filter = self.filter.clone();
                reload = true;
            }
            if ui.button("Clear").clicked() {
                self.filter.clear();
                self.applied_filter.clear();
                self.sort = None;
                reload = true;
            }
            ui.label("Rows:");
            egui::ComboBox::from_id_salt("limit").selected_text(self.limit.to_string()).show_ui(ui, |ui| {
                for l in LIMITS {
                    if ui.selectable_value(&mut self.limit, l, l.to_string()).clicked() {
                        reload = true;
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            let shown = self.rows.len();
            match self.total {
                Some(t) if (t as usize) > shown => ui.label(RichText::new(format!("showing the first {shown} of {t} rows")).weak()),
                Some(t) => ui.label(RichText::new(format!("{t} row(s)")).weak()),
                None => ui.label(RichText::new(format!("{shown} row(s)")).weak()),
            };
            if self.info.is_none() {
                ui.label(RichText::new("read-only (view)").weak());
            } else if !self.editable() {
                ui.label(RichText::new("read-only: the table has no primary key").weak());
            } else {
                ui.label(RichText::new("double-click a cell to edit; Enter saves, Esc cancels; empty = NULL").weak());
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(ERROR, e);
        }

        let editable = self.editable();
        let mut click_row: Option<usize> = None;
        let mut start_edit: Option<(usize, usize)> = None;
        let mut sort_col: Option<usize> = None;
        let mut commit: Option<(usize, usize, String)> = None;
        let mut cancel = false;
        let ncols = self.columns.len();
        if ncols > 0 {
            let sort = self.sort;
            let editing = &mut self.editing;
            let rows = &self.rows;
            let columns = &self.columns;
            let selected = self.selected;
            let reserve = if editable { 120.0 } else { 20.0 };
            ui.push_id("grid", |ui| {
                let max_h = (ui.available_height() - reserve).max(100.0);
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .max_scroll_height(max_h)
                    .columns(Column::initial(130.0).at_least(50.0).clip(true), ncols)
                    .header(24.0, |mut h| {
                        for (i, c) in columns.iter().enumerate() {
                            h.col(|ui| {
                                let arrow = match sort {
                                    Some((s, false)) if s == i => " ▲",
                                    Some((s, true)) if s == i => " ▼",
                                    _ => "",
                                };
                                if ui.add(egui::Label::new(RichText::new(format!("{c}{arrow}")).strong()).sense(Sense::click())).on_hover_text("Click to sort").clicked() {
                                    sort_col = Some(i);
                                }
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(22.0, rows.len(), |mut row| {
                            let ri = row.index();
                            row.set_selected(selected == Some(ri));
                            for (ci, v) in rows[ri].iter().enumerate() {
                                row.col(|ui| {
                                    if let Some(e) = editing.as_mut().filter(|e| e.row == ri && e.col == ci) {
                                        let r = ui.add(TextEdit::singleline(&mut e.text).desired_width(f32::INFINITY));
                                        if !e.focused {
                                            r.request_focus();
                                            e.focused = true;
                                        }
                                        if ui.input(|i| i.key_pressed(Key::Escape)) {
                                            cancel = true;
                                        } else if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                            commit = Some((ri, ci, e.text.clone()));
                                        } else if r.lost_focus() {
                                            cancel = true;
                                        }
                                    } else {
                                        let text = display(v);
                                        let label = if v.is_null() { egui::Label::new(RichText::new(text).weak().italics()) } else { egui::Label::new(text) };
                                        let r = ui.add(label.truncate().sense(Sense::click()));
                                        if r.clicked() {
                                            click_row = Some(ri);
                                        }
                                        if editable && r.double_clicked() {
                                            start_edit = Some((ri, ci));
                                        }
                                    }
                                });
                            }
                            if row.response().clicked() {
                                click_row = Some(ri);
                            }
                        });
                    });
            });
        } else if self.error.is_none() {
            ui.label(RichText::new("(no columns)").weak());
        }
        if let Some(i) = sort_col {
            self.sort = match self.sort {
                Some((c, false)) if c == i => Some((i, true)),
                Some((c, true)) if c == i => None,
                _ => Some((i, false)),
            };
            reload = true;
        }
        if let Some(r) = click_row {
            self.selected = Some(r);
            self.confirm_delete = false;
        }
        if let Some((r, c)) = start_edit {
            let current = &self.rows[r][c];
            self.editing = Some(Edit { row: r, col: c, text: if current.is_null() { String::new() } else { tools::cell_text(current) }, focused: false });
        }
        if cancel {
            self.editing = None;
        }
        if let Some((r, c, text)) = commit {
            self.editing = None;
            match self.commit_edit(conn, r, c, &text) {
                Ok(()) => {
                    ev.push(Event::Changed);
                    ev.push(Event::Status(format!("Updated row in '{}'", self.table)));
                }
                Err(e) => ev.push(Event::Error(e)),
            }
        }

        if editable {
            ui.separator();
            ui.horizontal(|ui| {
                let has_sel = self.selected.is_some();
                if self.confirm_delete {
                    ui.label(RichText::new("Delete the selected row?").strong());
                    if ui.button("Yes, delete").clicked() {
                        if let Some(r) = self.selected {
                            match self.delete_row(conn, r) {
                                Ok(()) => {
                                    ev.push(Event::Changed);
                                    ev.push(Event::Status(format!("Deleted a row from '{}'", self.table)));
                                }
                                Err(e) => ev.push(Event::Error(e)),
                            }
                        }
                        self.confirm_delete = false;
                        self.selected = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete = false;
                    }
                } else if ui.add_enabled(has_sel, egui::Button::new("Delete selected row")).clicked() {
                    self.confirm_delete = true;
                }
            });
            egui::CollapsingHeader::new(RichText::new("New row").strong()).default_open(false).show(ui, |ui| {
                let info = self.info.clone();
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, c) in self.columns.iter().enumerate() {
                            let ci = info.as_ref().and_then(|t| t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)));
                            let generated = ci.is_some_and(|x| x.computed || x.identity.is_some() || x.ty == "timestamp");
                            ui.vertical(|ui| {
                                ui.label(RichText::new(c).strong());
                                ui.add_enabled(!generated, TextEdit::singleline(&mut self.new_row[i]).desired_width(110.0).hint_text(ci.map(|x| x.ty.clone()).unwrap_or_default()));
                            });
                        }
                    });
                });
                if ui.button("Insert row").clicked() {
                    match self.insert_new_row(conn) {
                        Ok(()) => {
                            ev.push(Event::Changed);
                            ev.push(Event::Status(format!("Inserted a row into '{}'", self.table)));
                        }
                        Err(e) => ev.push(Event::Error(e)),
                    }
                }
            });
        }
        if reload {
            self.load(conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsql_server::Server;

    fn setup() -> (Connection, Datasheet) {
        let mut c = Server::new().connect();
        for s in [
            "CREATE TABLE dbo.p (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(30) NOT NULL, amt DECIMAL(10,2) NULL)",
            "INSERT p (name, amt) VALUES (N'a', 1.5), (N'b', NULL), (N'c', 3)",
            "CREATE TABLE dbo.nokey (a INT)",
            "CREATE VIEW dbo.v AS SELECT name FROM dbo.p",
        ] {
            c.run(s).unwrap();
        }
        let mut d = Datasheet::new("dbo.p".into());
        d.load(&mut c);
        (c, d)
    }

    #[test]
    fn loading_shows_rows_and_the_total() {
        let (_c, d) = setup();
        assert_eq!(d.columns, vec!["id", "name", "amt"]);
        assert_eq!(d.rows.len(), 3);
        assert_eq!(d.total, Some(3));
        assert!(d.editable());
    }

    #[test]
    fn filters_sorting_and_limits_apply() {
        let (mut c, mut d) = setup();
        d.applied_filter = "amt IS NOT NULL".into();
        d.sort = Some((1, true));
        d.load(&mut c);
        assert_eq!(d.rows.iter().map(|r| r[1].to_string()).collect::<Vec<_>>(), vec!["c", "a"]);
        assert_eq!(d.total, Some(2));
        d.applied_filter = "bad syntax here (".into();
        d.load(&mut c);
        assert!(d.error.is_some() && d.rows.is_empty());
        d.applied_filter.clear();
        d.limit = 2;
        d.load(&mut c);
        assert_eq!((d.rows.len(), d.total), (2, Some(3)));
    }

    #[test]
    fn editing_updates_by_primary_key_with_typed_values() {
        let (mut c, mut d) = setup();
        d.commit_edit(&mut c, 0, 1, "O'Neil").unwrap();
        d.commit_edit(&mut c, 1, 2, "9.25").unwrap();
        d.commit_edit(&mut c, 0, 2, "").unwrap(); // NULL
        d.load(&mut c);
        assert_eq!(d.rows[0][1].to_string(), "O'Neil");
        assert!(d.rows[0][2].is_null());
        assert_eq!(d.rows[1][2].to_string(), "9.25");
        // bad input is rejected without touching the data
        assert!(d.commit_edit(&mut c, 0, 2, "abc").unwrap_err().contains("not a number"));
        // the identity column cannot be edited
        assert!(d.commit_edit(&mut c, 0, 0, "5").unwrap_err().contains("generated"));
        // constraint errors come back as messages
        assert!(d.commit_edit(&mut c, 0, 1, &"x".repeat(40)).is_err());
    }

    #[test]
    fn rows_can_be_inserted_and_deleted() {
        let (mut c, mut d) = setup();
        d.new_row = vec![String::new(), "new".into(), "7".into()];
        d.insert_new_row(&mut c).unwrap();
        d.load(&mut c);
        assert_eq!(d.rows.len(), 4);
        assert_eq!(d.rows[3][0].to_string(), "4");
        assert!(d.new_row.iter().all(|s| s.is_empty()), "the boxes are cleared");
        d.new_row = vec![String::new(), String::new(), String::new()];
        assert!(d.insert_new_row(&mut c).is_err(), "a NOT NULL column without a default needs a value");
        d.delete_row(&mut c, 1).unwrap();
        d.load(&mut c);
        assert_eq!(d.rows.iter().map(|r| r[1].to_string()).collect::<Vec<_>>(), vec!["a", "c", "new"]);
    }

    #[test]
    fn views_and_keyless_tables_are_read_only() {
        let (mut c, _) = setup();
        let mut v = Datasheet::new("dbo.v".into());
        v.load(&mut c);
        assert_eq!(v.rows.len(), 3);
        assert!(!v.editable());
        assert!(v.commit_edit(&mut c, 0, 0, "x").is_err());
        let mut n = Datasheet::new("dbo.nokey".into());
        c.run("INSERT nokey VALUES (1)").unwrap();
        n.load(&mut c);
        assert!(!n.editable());
        assert!(n.delete_row(&mut c, 0).unwrap_err().contains("no primary key"));
    }
}
