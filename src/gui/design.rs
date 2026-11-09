//! Design view: a table's structure, with the common schema changes.
use egui::{RichText, TextEdit};
use tsql_server::inspect::TableInfo;
use tsql_server::{tools, Connection};

use crate::ui::{Event, ACCENT, ERROR};

enum Confirm {
    DropColumn(String),
    /// Index name and whether it is a constraint.
    DropIndex(String, bool),
    DropForeignKey(String),
}

pub struct Design {
    pub table: String,
    info: Option<TableInfo>,
    pub col_name: String,
    pub col_type: String,
    pub col_nullable: bool,
    pub col_default: String,
    pub ix_name: String,
    pub ix_cols: String,
    pub ix_unique: bool,
    confirm: Option<Confirm>,
    show_script: bool,
    dirty: bool,
}

impl Design {
    pub fn new(table: String) -> Self {
        Design { table, info: None, col_name: String::new(), col_type: "nvarchar(50)".into(), col_nullable: true, col_default: String::new(), ix_name: String::new(), ix_cols: String::new(), ix_unique: false, confirm: None, show_script: false, dirty: true }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn refresh(&mut self, conn: &Connection) {
        self.dirty = false;
        self.info = conn.table_info(&self.table);
    }

    fn quoted(&self) -> String {
        self.info.as_ref().map(|i| i.quoted()).unwrap_or_else(|| tools::quote_name(&self.table))
    }

    pub fn add_column_sql(&self) -> Result<String, String> {
        let name = self.col_name.trim();
        let ty = self.col_type.trim();
        if name.is_empty() {
            return Err("give the new column a name".into());
        }
        if ty.is_empty() {
            return Err("give the new column a type, for example int or nvarchar(50)".into());
        }
        let mut s = format!("ALTER TABLE {} ADD {} {} {}", self.quoted(), tools::bracket(name), ty, if self.col_nullable { "NULL" } else { "NOT NULL" });
        if !self.col_default.trim().is_empty() {
            s.push_str(&format!(" DEFAULT {}", self.col_default.trim()));
        }
        Ok(s)
    }

    pub fn add_index_sql(&self) -> Result<String, String> {
        let cols: Vec<String> = self
            .ix_cols
            .split(',')
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .map(|c| {
                // "name DESC" keeps its direction
                match c.rsplit_once(' ') {
                    Some((n, d)) if d.eq_ignore_ascii_case("desc") || d.eq_ignore_ascii_case("asc") => format!("{} {}", tools::bracket(n.trim()), d.to_uppercase()),
                    _ => tools::bracket(c),
                }
            })
            .collect();
        if cols.is_empty() {
            return Err("list the columns of the index, separated by commas".into());
        }
        let name = if self.ix_name.trim().is_empty() {
            let base: String = self.table.rsplit('.').next().unwrap_or("t").trim_matches(|c| c == '[' || c == ']').to_string();
            format!("ix_{}_{}", base, self.ix_cols.split(',').map(|c| c.trim().split(' ').next().unwrap_or("")).collect::<Vec<_>>().join("_"))
        } else {
            self.ix_name.trim().to_string()
        };
        Ok(format!("CREATE {}INDEX {} ON {} ({})", if self.ix_unique { "UNIQUE " } else { "" }, tools::bracket(&name), self.quoted(), cols.join(", ")))
    }

    pub fn drop_column_sql(&self, col: &str) -> String {
        format!("ALTER TABLE {} DROP COLUMN {}", self.quoted(), tools::bracket(col))
    }

    pub fn drop_index_sql(&self, name: &str, constraint: bool) -> String {
        if constraint {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", self.quoted(), tools::bracket(name))
        } else {
            format!("DROP INDEX {} ON {}", tools::bracket(name), self.quoted())
        }
    }

    pub fn drop_fk_sql(&self, name: &str) -> String {
        format!("ALTER TABLE {} DROP CONSTRAINT {}", self.quoted(), tools::bracket(name))
    }

    fn run(&mut self, conn: &mut Connection, sql: Result<String, String>, ok: &str, ev: &mut Vec<Event>) {
        match sql {
            Err(e) => ev.push(Event::Error(e)),
            Ok(sql) => match conn.run(&sql) {
                Ok(_) => {
                    ev.push(Event::Changed);
                    ev.push(Event::Status(ok.to_string()));
                    self.refresh(conn);
                }
                Err(e) => ev.push(Event::Error(e.message)),
            },
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, conn: &mut Connection, ev: &mut Vec<Event>) {
        ui.push_id(("design", self.table.clone()), |ui| self.show(ui, conn, ev));
    }

    fn show(&mut self, ui: &mut egui::Ui, conn: &mut Connection, ev: &mut Vec<Event>) {
        if self.dirty {
            self.refresh(conn);
        }
        let Some(info) = self.info.clone() else {
            ui.colored_label(ERROR, format!("The table '{}' no longer exists.", self.table));
            return;
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new(format!("{}.{}", info.schema, info.name)).color(ACCENT));
                ui.label(RichText::new(format!("about {} row(s)", info.approx_rows)).weak());
                if ui.button("Show script").clicked() {
                    self.show_script = !self.show_script;
                }
                if ui.button("Open script in a query tab").clicked() {
                    ev.push(Event::OpenQuery { name: None, sql: tools::script_table(&info) });
                }
            });
            if self.show_script {
                let mut text = tools::script_table(&info);
                ui.add(TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY).desired_rows(8));
            }

            // ---- confirmation of a destructive change
            if let Some(c) = &self.confirm {
                let what = match c {
                    Confirm::DropColumn(n) => format!("Drop column '{n}' and its data?"),
                    Confirm::DropIndex(n, _) => format!("Drop index '{n}'?"),
                    Confirm::DropForeignKey(n) => format!("Drop foreign key '{n}'?"),
                };
                let mut decision = None;
                ui.horizontal(|ui| {
                    ui.colored_label(ERROR, what);
                    if ui.button("Yes").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
                match decision {
                    Some(true) => {
                        let (sql, ok) = match self.confirm.take().unwrap() {
                            Confirm::DropColumn(n) => (self.drop_column_sql(&n), format!("Dropped column '{n}'")),
                            Confirm::DropIndex(n, c) => (self.drop_index_sql(&n, c), format!("Dropped index '{n}'")),
                            Confirm::DropForeignKey(n) => (self.drop_fk_sql(&n), format!("Dropped foreign key '{n}'")),
                        };
                        self.run(conn, Ok(sql), &ok, ev);
                        return;
                    }
                    Some(false) => self.confirm = None,
                    None => {}
                }
            }

            // ---- columns
            ui.add_space(6.0);
            ui.label(RichText::new("Columns").strong());
            egui::Grid::new("columns").striped(true).num_columns(7).spacing([16.0, 4.0]).show(ui, |ui| {
                for h in ["Name", "Type", "Null", "Identity", "Key", "Default", ""] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for c in &info.columns {
                    ui.label(&c.name);
                    ui.label(RichText::new(&c.ty).monospace());
                    ui.label(if c.nullable { "yes" } else { "no" });
                    ui.label(c.identity.map(|(s, i)| format!("({s},{i})")).unwrap_or_default());
                    ui.label(if c.primary_key { "primary" } else { "" });
                    ui.label(match (c.has_default, c.computed) {
                        (_, true) => "computed",
                        (true, _) => "yes",
                        _ => "",
                    });
                    if ui.small_button("Drop").clicked() {
                        self.confirm = Some(Confirm::DropColumn(c.name.clone()));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Add column:");
                ui.add(TextEdit::singleline(&mut self.col_name).hint_text("name").desired_width(120.0));
                ui.add(TextEdit::singleline(&mut self.col_type).hint_text("type").desired_width(120.0));
                ui.checkbox(&mut self.col_nullable, "NULL");
                ui.add(TextEdit::singleline(&mut self.col_default).hint_text("default, e.g. 0 or 'n/a'").desired_width(150.0));
                if ui.button("Add").clicked() {
                    let sql = self.add_column_sql();
                    self.run(conn, sql, &format!("Added column '{}'", self.col_name.trim()), ev);
                }
            });

            // ---- indexes and keys
            ui.add_space(10.0);
            ui.label(RichText::new("Indexes and keys").strong());
            if info.indexes.is_empty() {
                ui.label(RichText::new("(none)").weak());
            }
            egui::Grid::new("indexes").striped(true).num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
                for ix in &info.indexes {
                    let kind = if ix.primary_key {
                        "primary key"
                    } else if ix.constraint {
                        "unique constraint"
                    } else if ix.unique {
                        "unique index"
                    } else {
                        "index"
                    };
                    ui.label(&ix.name);
                    ui.label(RichText::new(kind).weak());
                    ui.label(ix.columns.iter().map(|(c, d)| format!("{c}{}", if *d { " DESC" } else { "" })).collect::<Vec<_>>().join(", "));
                    if ui.small_button("Drop").clicked() {
                        self.confirm = Some(Confirm::DropIndex(ix.name.clone(), ix.constraint));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Add index:");
                ui.add(TextEdit::singleline(&mut self.ix_name).hint_text("name (optional)").desired_width(130.0));
                ui.add(TextEdit::singleline(&mut self.ix_cols).hint_text("columns, e.g. last, first DESC").desired_width(200.0));
                ui.checkbox(&mut self.ix_unique, "unique");
                if ui.button("Create").clicked() {
                    let sql = self.add_index_sql();
                    self.run(conn, sql, "Created index", ev);
                }
            });

            // ---- foreign keys and the rest
            ui.add_space(10.0);
            ui.label(RichText::new("Foreign keys").strong());
            if info.foreign_keys.is_empty() {
                ui.label(RichText::new("(none)").weak());
            }
            for f in &info.foreign_keys {
                ui.horizontal(|ui| {
                    ui.label(format!("{}: ({}) → {} ({})", f.name, f.columns.join(", "), f.ref_table, f.ref_columns.join(", ")));
                    for (label, rule) in [("delete", &f.on_delete), ("update", &f.on_update)] {
                        if !rule.is_empty() && !rule.eq_ignore_ascii_case("NO ACTION") {
                            ui.label(RichText::new(format!("on {label} {rule}")).weak());
                        }
                    }
                    if ui.small_button("Drop").clicked() {
                        self.confirm = Some(Confirm::DropForeignKey(f.name.clone()));
                    }
                });
            }
            if !info.checks.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("Check constraints").strong());
                ui.label(info.checks.join(", "));
            }
            if !info.triggers.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("Triggers").strong());
                ui.label(info.triggers.join(", "));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsql_server::Server;

    fn setup() -> (Connection, Design) {
        let mut c = Server::new().connect();
        c.run("CREATE TABLE dbo.p (id INT PRIMARY KEY, name NVARCHAR(30) NOT NULL, amt INT NULL)").unwrap();
        c.run("INSERT p VALUES (1, N'a', 5)").unwrap();
        let mut d = Design::new("dbo.p".into());
        d.refresh(&c);
        (c, d)
    }

    fn apply(c: &mut Connection, d: &mut Design, sql: Result<String, String>) -> Result<(), String> {
        let sql = sql?;
        c.run(&sql).map_err(|e| e.message)?;
        d.refresh(c);
        Ok(())
    }

    #[test]
    fn columns_can_be_added_and_dropped() {
        let (mut c, mut d) = setup();
        d.col_name = "note".into();
        d.col_type = "varchar(20)".into();
        d.col_nullable = false;
        d.col_default = "'n/a'".into();
        assert_eq!(d.add_column_sql().unwrap(), "ALTER TABLE [dbo].[p] ADD [note] varchar(20) NOT NULL DEFAULT 'n/a'");
        let sql = d.add_column_sql();
        apply(&mut c, &mut d, sql).unwrap();
        assert_eq!(d.info.as_ref().unwrap().columns.last().unwrap().name, "note");
        assert_eq!(c.query("SELECT note FROM p").unwrap()[0][0].to_string(), "n/a");
        let sql = d.drop_column_sql("note");
        apply(&mut c, &mut d, Ok(sql)).unwrap();
        assert_eq!(d.info.as_ref().unwrap().columns.len(), 3);
        d.col_name.clear();
        assert!(d.add_column_sql().is_err());
        d.col_name = "weird]name".into();
        assert!(d.add_column_sql().unwrap().contains("[weird]]name]"));
    }

    #[test]
    fn indexes_and_constraints_are_created_and_dropped_the_right_way() {
        let (mut c, mut d) = setup();
        d.ix_cols = "name, amt DESC".into();
        d.ix_unique = true;
        assert_eq!(d.add_index_sql().unwrap(), "CREATE UNIQUE INDEX [ix_p_name_amt] ON [dbo].[p] ([name], [amt] DESC)");
        let sql = d.add_index_sql();
        apply(&mut c, &mut d, sql).unwrap();
        let info = d.info.clone().unwrap();
        let ix = info.indexes.iter().find(|i| i.name == "ix_p_name_amt").unwrap();
        assert!(ix.unique && !ix.constraint && ix.columns[1] == ("amt".to_string(), true));
        let sql = d.drop_index_sql("ix_p_name_amt", ix.constraint);
        apply(&mut c, &mut d, Ok(sql)).unwrap();
        assert!(d.info.as_ref().unwrap().indexes.iter().all(|i| i.name != "ix_p_name_amt"));
        // a unique constraint is dropped as a constraint
        c.run("ALTER TABLE p ADD CONSTRAINT uq_name UNIQUE (name)").unwrap();
        d.refresh(&c);
        let uq = d.info.clone().unwrap().indexes.into_iter().find(|i| i.name == "uq_name").unwrap();
        assert!(uq.constraint);
        let sql = d.drop_index_sql("uq_name", uq.constraint);
        apply(&mut c, &mut d, Ok(sql)).unwrap();
        assert!(d.info.as_ref().unwrap().indexes.iter().all(|i| i.name != "uq_name"));
        d.ix_cols = " , ".into();
        assert!(d.add_index_sql().is_err());
    }

    #[test]
    fn foreign_keys_can_be_dropped() {
        let (mut c, _) = setup();
        c.run("CREATE TABLE dbo.k (id INT PRIMARY KEY, pid INT, CONSTRAINT fk_k_p FOREIGN KEY (pid) REFERENCES dbo.p(id))").unwrap();
        let mut d = Design::new("dbo.k".into());
        d.refresh(&c);
        assert_eq!(d.info.as_ref().unwrap().foreign_keys.len(), 1);
        let sql = d.drop_fk_sql("fk_k_p");
        apply(&mut c, &mut d, Ok(sql)).unwrap();
        assert!(d.info.as_ref().unwrap().foreign_keys.is_empty());
    }
}
