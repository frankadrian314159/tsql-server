//! Types and widgets shared between the application shell and the individual views.
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use tsql_server::{tools, Value};

pub const ACCENT: Color32 = Color32::from_rgb(33, 94, 168);
pub const ERROR: Color32 = Color32::from_rgb(180, 30, 30);

/// Something a view wants the application to do once the frame is drawn.
pub enum Event {
    /// Data or schema changed; the navigation pane and every open view must refresh.
    Changed,
    Error(String),
    Status(String),
    OpenDatasheet(String),
    OpenDesign(String),
    OpenQuery { name: Option<String>, sql: String },
}

/// Text shown for a cell in a grid.
pub fn display(v: &Value) -> String {
    tools::cell_text(v)
}

/// A read-only, scrollable grid of values.
pub fn result_grid(ui: &mut egui::Ui, id: &str, columns: &[String], rows: &[Vec<Value>]) {
    ui.push_id(id, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .columns(Column::initial(130.0).at_least(50.0).clip(true), columns.len().max(1))
            .header(24.0, |mut h| {
                for c in columns {
                    h.col(|ui| {
                        ui.label(RichText::new(if c.is_empty() { "(No column name)" } else { c }).strong());
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    for v in r {
                        row.col(|ui| {
                            let text = display(v);
                            if v.is_null() {
                                ui.add(egui::Label::new(RichText::new(text).weak().italics()).truncate());
                            } else {
                                ui.add(egui::Label::new(text).truncate());
                            }
                        });
                    }
                });
            });
    });
}
