//! Helpers shared by the shell and the GUI: result grids, CSV import/export, SQL literals and
//! table scripts.
use crate::engine::ResultSet;
use crate::error::SqlError;
use crate::inspect::TableInfo;
use crate::{convert, Connection, Value};

/// `[name]` with any `]` doubled, safe to splice into SQL as an identifier.
pub fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// A value written the way T-SQL would read it back.
pub fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Bit(_) | Value::Bin(_) => convert::display(v),
        Value::Str(s) => format!("N'{}'", s.replace('\'', "''")),
        other => format!("'{}'", convert::display(other).replace('\'', "''")),
    }
}

fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_))
}

/// The text a cell shows in a grid (newlines are shown as `⏎`).
pub fn cell_text(v: &Value) -> String {
    convert::display(v).replace(['\r', '\n'], "⏎")
}

/// A result set as an aligned text grid with box-drawing separators and a row count; numbers are
/// right-aligned, long values are cut at `max_width` characters.
pub fn format_grid(rs: &ResultSet, max_width: usize) -> String {
    let cut = |s: String| {
        if s.chars().count() > max_width {
            format!("{}…", s.chars().take(max_width.saturating_sub(1)).collect::<String>())
        } else {
            s
        }
    };
    let columns: Vec<String> = rs.columns.iter().map(|c| if c.is_empty() { "(No column name)".to_string() } else { c.clone() }).collect();
    let body: Vec<Vec<String>> = rs.rows.iter().map(|r| r.iter().map(|v| cut(cell_text(v))).collect()).collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count().max(1)).collect();
    for row in &body {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let numeric: Vec<bool> = (0..columns.len()).map(|i| !rs.rows.is_empty() && rs.rows.iter().all(|r| is_numeric(&r[i]) || r[i].is_null())).collect();
    let line = |cells: &[String]| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .zip(&numeric)
            .map(|((c, w), num)| {
                let pad = w - c.chars().count();
                if *num {
                    format!("{}{c}", " ".repeat(pad))
                } else {
                    format!("{c}{}", " ".repeat(pad))
                }
            })
            .collect();
        parts.join(" │ ").trim_end().to_string()
    };
    let mut out = String::new();
    out.push_str(&line(&columns));
    out.push('\n');
    out.push_str(&widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─"));
    out.push('\n');
    for row in &body {
        out.push_str(&line(row));
        out.push('\n');
    }
    out.push_str(&format!("({} row{})\n", body.len(), if body.len() == 1 { "" } else { "s" }));
    out
}

// ---------------------------------------------------------------------- CSV
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A result set as CSV (header row, CRLF line ends, NULL as an empty field).
pub fn to_csv(rs: &ResultSet) -> String {
    let mut out = String::new();
    out.push_str(&rs.columns.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(","));
    out.push_str("\r\n");
    for r in &rs.rows {
        let cells: Vec<String> = r.iter().map(|v| if v.is_null() { String::new() } else { csv_field(&convert::display(v)) }).collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

/// Parse CSV (RFC 4180: quoted fields, doubled quotes, embedded line breaks; a leading BOM is skipped).
pub fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows: Vec<Vec<String>> = vec![];
    let mut row: Vec<String> = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if quoted {
        return Err("the file ends inside a quoted field".into());
    }
    if any && (!field.is_empty() || !row.is_empty() || was_quoted) {
        row.push(field);
        rows.push(row);
    }
    // a blank line is not a record
    rows.retain(|r| !(r.len() == 1 && r[0].is_empty()));
    Ok(rows)
}

fn infer_type(values: &[&str]) -> String {
    let vals: Vec<&str> = values.iter().copied().filter(|v| !v.trim().is_empty()).collect();
    if vals.is_empty() {
        return "NVARCHAR(100)".into();
    }
    if vals.iter().all(|v| v.trim().parse::<i32>().is_ok()) {
        return "INT".into();
    }
    if vals.iter().all(|v| v.trim().parse::<i64>().is_ok()) {
        return "BIGINT".into();
    }
    if vals.iter().all(|v| v.trim().parse::<f64>().map(|f| f.is_finite()).unwrap_or(false)) {
        return "FLOAT".into();
    }
    let date = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
    let datetime = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2}(\.\d{1,7})?)?$").unwrap();
    if vals.iter().all(|v| date.is_match(v.trim())) {
        return "DATE".into();
    }
    if vals.iter().all(|v| datetime.is_match(v.trim())) {
        return "DATETIME2".into();
    }
    let longest = vals.iter().map(|v| v.chars().count()).max().unwrap_or(1);
    if longest > 4000 {
        "NVARCHAR(MAX)".into()
    } else {
        format!("NVARCHAR({})", (longest.max(10) + 9) / 10 * 10)
    }
}

/// The literal to put in SQL for text typed or read from a file for a column of type `ty`
/// (`nvarchar(50)`, `int`, ...). An empty value is NULL for a nullable column.
pub fn literal_for_column(cell: &str, ty: &str, nullable: bool) -> Result<String, String> {
    cell_literal(cell, ty, nullable)
}

fn cell_literal(cell: &str, ty: &str, nullable: bool) -> Result<String, String> {
    let base = ty.split('(').next().unwrap_or(ty).trim().to_lowercase();
    let t = cell.trim();
    let textual = matches!(base.as_str(), "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml");
    if cell.is_empty() || (t.is_empty() && !textual) {
        return Ok(if nullable { "NULL".into() } else if textual { "''".into() } else { return Err("an empty value for a NOT NULL column".into()) });
    }
    match base.as_str() {
        "tinyint" | "smallint" | "int" | "bigint" => t.parse::<i64>().map(|n| n.to_string()).map_err(|_| format!("'{}' is not an integer", cell)),
        "decimal" | "numeric" | "money" | "smallmoney" | "float" | "real" => t.parse::<f64>().map(|_| t.to_string()).map_err(|_| format!("'{}' is not a number", cell)),
        "bit" => match t.to_lowercase().as_str() {
            "1" | "true" | "yes" => Ok("1".into()),
            "0" | "false" | "no" => Ok("0".into()),
            _ => Err(format!("'{}' is not a bit value", cell)),
        },
        "binary" | "varbinary" | "image" => {
            if t.starts_with("0x") || t.starts_with("0X") {
                Ok(t.to_string())
            } else {
                Err(format!("'{}' is not a hexadecimal binary value", cell))
            }
        }
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml" => Ok(format!("N'{}'", cell.replace('\'', "''"))),
        _ => Ok(format!("'{}'", t.replace('\'', "''"))),
    }
}

/// What the import functions need of a session: the engine inside the process (`Connection`), or whatever the desktop app
/// has connected to.
pub trait SqlSession {
    /// Run a batch; the first error of it, if any.
    fn run_sql(&mut self, sql: &str) -> Result<(), SqlError>;
    /// Structure of a table of the current database.
    fn describe(&mut self, name: &str) -> Option<TableInfo>;
    /// `@@TRANCOUNT`.
    fn open_transactions(&self) -> i32;
}

impl SqlSession for Connection {
    fn run_sql(&mut self, sql: &str) -> Result<(), SqlError> {
        self.run(sql).map(|_| ())
    }
    fn describe(&mut self, name: &str) -> Option<TableInfo> {
        self.table_info(name)
    }
    fn open_transactions(&self) -> i32 {
        self.transaction_count()
    }
}

/// Reads the records of delimited text one at a time (quoted fields, doubled quotes and line breaks inside quotes as in
/// RFC 4180; a leading BOM is skipped; blank lines are not records), so a file of any size is never held in memory.
pub struct CsvReader<R: std::io::BufRead> {
    input: R,
    delimiter: char,
    first: bool,
    /// Records returned so far.
    pub records: u64,
}

impl<R: std::io::BufRead> CsvReader<R> {
    pub fn new(input: R, delimiter: char) -> Self {
        CsvReader { input, delimiter, first: true, records: 0 }
    }

    pub fn next_record(&mut self) -> Result<Option<Vec<String>>, String> {
        loop {
            let mut text = String::new();
            loop {
                let mut line = String::new();
                let n = self.input.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                if self.first {
                    self.first = false;
                    if let Some(rest) = line.strip_prefix('\u{feff}') {
                        line = rest.to_string();
                    }
                }
                text.push_str(&line);
                // an odd number of quotes: the record goes on in the next line
                if text.matches('"').count() % 2 == 0 {
                    break;
                }
            }
            if text.is_empty() {
                return Ok(None);
            }
            if text.trim_end_matches(['\r', '\n']).is_empty() {
                continue;
            }
            let rec = parse_record(&text, self.delimiter)?;
            self.records += 1;
            return Ok(Some(rec));
        }
    }
}

fn parse_record(text: &str, delim: char) -> Result<Vec<String>, String> {
    let mut row = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            c if c == delim => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' | '\n' => {}
            c => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field is not closed".into());
    }
    row.push(field);
    Ok(row)
}

/// How `bulk_import` reads and loads.
#[derive(Clone, Debug)]
pub struct BulkOptions {
    pub delimiter: char,
    /// The first record names the columns. Without it the fields are the table's columns, in order.
    pub header: bool,
    /// Create the table first, with types inferred from the first records, when it does not exist.
    pub create: bool,
    /// Rows per `INSERT` (at most 1000, what a `VALUES` list takes).
    pub batch: usize,
    /// All rows or none (one transaction); otherwise each batch commits by itself and an error leaves the earlier batches in.
    pub one_transaction: bool,
}

impl Default for BulkOptions {
    fn default() -> Self {
        BulkOptions { delimiter: ',', header: true, create: false, batch: 1000, one_transaction: true }
    }
}

/// Load delimited text of any size into a table, a batch of rows at a time. `progress` gets the number of rows loaded after each
/// batch and returns false to stop (with one transaction the rows so far are rolled back; the error is 50001). Returns the
/// number of rows loaded.
pub fn bulk_import<C: SqlSession + ?Sized>(conn: &mut C, table: &str, input: impl std::io::BufRead, opts: &BulkOptions, progress: &mut dyn FnMut(u64) -> bool) -> Result<u64, SqlError> {
    let fail = |m: String| SqlError::new(50000, m);
    let mut rd = CsvReader::new(input, opts.delimiter);
    let batch = opts.batch.clamp(1, 1000);
    let header: Option<Vec<String>> = if opts.header { rd.next_record().map_err(fail)? } else { None };
    if opts.header && header.is_none() {
        return Err(fail("the file is empty".into()));
    }
    if header.as_ref().map(|h| h.iter().any(|c| c.trim().is_empty())).unwrap_or(false) {
        return Err(fail("the header row has an empty column name".into()));
    }
    // records read ahead of the load: the sample the types of a new table are inferred from
    let mut ahead: std::collections::VecDeque<Vec<String>> = std::collections::VecDeque::new();
    let own_tran = opts.one_transaction && conn.open_transactions() == 0;
    if own_tran {
        conn.run_sql("BEGIN TRAN")?;
    }
    let mut loaded: u64 = 0;
    let result = (|| -> Result<u64, SqlError> {
        let mut info = conn.describe(table);
        if info.is_none() && opts.create {
            while ahead.len() < 1000 {
                match rd.next_record().map_err(fail)? {
                    Some(r) => ahead.push_back(r),
                    None => break,
                }
            }
            let width = header.as_ref().map(|h| h.len()).or_else(|| ahead.front().map(|r| r.len())).ok_or_else(|| fail("there is nothing to create the table from".into()))?;
            let names: Vec<String> = header.clone().unwrap_or_else(|| (1..=width).map(|i| format!("column{}", i)).collect());
            let cols: Vec<String> = (0..width)
                .map(|i| {
                    let col: Vec<&str> = ahead.iter().map(|r| r.get(i).map(|c| c.as_str()).unwrap_or("")).collect();
                    format!("{} {} NULL", bracket(names[i].trim()), infer_type(&col))
                })
                .collect();
            conn.run_sql(&format!("CREATE TABLE {} ({})", quote_name(table), cols.join(", ")))?;
            info = conn.describe(table);
        }
        let info = info.ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", table)))?;
        let cols: Vec<crate::inspect::ColumnInfo> = match &header {
            Some(h) => {
                let mut v = vec![];
                for name in h {
                    match info.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())) {
                        Some(c) => v.push(c.clone()),
                        None => return Err(SqlError::new(207, format!("Invalid column name '{}'.", name.trim()))),
                    }
                }
                v
            }
            None => info.columns.iter().filter(|c| !c.computed).cloned().collect(),
        };
        let list = cols.iter().map(|c| bracket(&c.name)).collect::<Vec<_>>().join(", ");
        let first_record = if header.is_some() { 2 } else { 1 };
        let mut done = false;
        while !done {
            let mut values = vec![];
            while values.len() < batch {
                let rec = match ahead.pop_front() {
                    Some(r) => Some(r),
                    None => rd.next_record().map_err(fail)?,
                };
                let Some(r) = rec else {
                    done = true;
                    break;
                };
                let n = loaded + values.len() as u64 + first_record;
                if r.len() != cols.len() {
                    return Err(fail(format!("record {} has {} fields, expected {}", n, r.len(), cols.len())));
                }
                let mut cells = vec![];
                for (c, cell) in cols.iter().zip(&r) {
                    cells.push(cell_literal(cell, &c.ty, c.nullable).map_err(|e| fail(format!("record {}, column {}: {}", n, c.name, e)))?);
                }
                values.push(format!("({})", cells.join(", ")));
            }
            if values.is_empty() {
                break;
            }
            conn.run_sql(&format!("INSERT {} ({}) VALUES {}", info.quoted(), list, values.join(", "))).map_err(|mut e| {
                if !opts.one_transaction && loaded > 0 {
                    e.message = format!("{} ({} rows were loaded before this batch and stay in the table)", e.message, loaded);
                }
                e
            })?;
            loaded += values.len() as u64;
            if !progress(loaded) {
                return Err(SqlError::new(50001, format!("the import was stopped after {} rows", loaded)));
            }
        }
        Ok(loaded)
    })();
    match result {
        Ok(n) => {
            if own_tran {
                conn.run_sql("COMMIT")?;
            }
            Ok(n)
        }
        Err(e) => {
            if own_tran {
                let _ = conn.run_sql("IF @@TRANCOUNT > 0 ROLLBACK");
            }
            Err(e)
        }
    }
}

/// Import CSV text into a table. The first record names the columns (matched case-insensitively; columns
/// that are not named get their default). With `create`, the table is created first, with column types
/// inferred from the data. Returns the number of rows inserted; either all rows are inserted or none.
pub fn import_csv<C: SqlSession + ?Sized>(conn: &mut C, table: &str, text: &str, create: bool) -> Result<usize, SqlError> {
    let fail = |m: String| SqlError::new(50000, m);
    let rows = parse_csv(text).map_err(fail)?;
    let (header, data) = match rows.split_first() {
        Some((h, d)) => (h.clone(), d.to_vec()),
        None => return Err(fail("the file is empty".into())),
    };
    if header.iter().any(|h| h.trim().is_empty()) {
        return Err(fail("the header row has an empty column name".into()));
    }
    if let Some((n, r)) = data.iter().enumerate().find(|(_, r)| r.len() != header.len()) {
        return Err(fail(format!("record {} has {} fields, the header has {}", n + 2, r.len(), header.len())));
    }
    let own_tran = conn.open_transactions() == 0;
    if own_tran {
        conn.run_sql("BEGIN TRAN")?;
    }
    let result = (|| -> Result<usize, SqlError> {
        if create {
            let cols: Vec<String> = (0..header.len())
                .map(|i| {
                    let col: Vec<&str> = data.iter().map(|r| r[i].as_str()).collect();
                    format!("{} {} NULL", bracket(header[i].trim()), infer_type(&col))
                })
                .collect();
            conn.run_sql(&format!("CREATE TABLE {} ({})", quote_name(table), cols.join(", ")))?;
        }
        let info = conn.describe(table).ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", table)))?;
        let mut cols = vec![];
        for h in &header {
            match info.columns.iter().find(|c| c.name.eq_ignore_ascii_case(h.trim())) {
                Some(c) => cols.push(c.clone()),
                None => return Err(SqlError::new(207, format!("Invalid column name '{}'.", h.trim()))),
            }
        }
        let list = cols.iter().map(|c| bracket(&c.name)).collect::<Vec<_>>().join(", ");
        for (b, chunk) in data.chunks(200).enumerate() {
            let mut values = vec![];
            for (n, r) in chunk.iter().enumerate() {
                let mut cells = vec![];
                for (c, cell) in cols.iter().zip(r) {
                    cells.push(cell_literal(cell, &c.ty, c.nullable).map_err(|e| SqlError::new(50000, format!("record {}, column {}: {}", b * 200 + n + 2, c.name, e)))?);
                }
                values.push(format!("({})", cells.join(", ")));
            }
            conn.run_sql(&format!("INSERT {} ({}) VALUES {}", info.quoted(), list, values.join(", ")))?;
        }
        Ok(data.len())
    })();
    match result {
        Ok(n) => {
            if own_tran {
                conn.run_sql("COMMIT")?;
            }
            Ok(n)
        }
        Err(e) => {
            if own_tran {
                let _ = conn.run_sql("IF @@TRANCOUNT > 0 ROLLBACK");
            }
            Err(e)
        }
    }
}

/// A possibly schema-qualified name (`dbo.t`, `[dbo].[t]`) with each part bracketed.
pub fn quote_name(name: &str) -> String {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    for ch in name.trim().chars() {
        match ch {
            '[' if !quoted => quoted = true,
            ']' if quoted => quoted = false,
            '.' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts.iter().map(|p| bracket(p)).collect::<Vec<_>>().join(".")
}

/// T-SQL that recreates a table's structure: columns with their defaults and computed formulas, keys,
/// other indexes, check constraints and foreign keys.
pub fn script_table(t: &TableInfo) -> String {
    let mut lines: Vec<String> = vec![];
    for c in &t.columns {
        if let (true, Some(f)) = (c.computed, &c.computed_text) {
            lines.push(format!("    {} AS ({})", bracket(&c.name), f));
            continue;
        }
        let mut l = format!("    {} {}", bracket(&c.name), c.ty);
        if let Some((s, i)) = c.identity {
            l.push_str(&format!(" IDENTITY({},{})", s, i));
        }
        l.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if let Some(d) = &c.default_text {
            if let Some(n) = c.default_name.as_ref().filter(|n| !n.starts_with("DF__")) {
                l.push_str(&format!(" CONSTRAINT {}", bracket(n)));
            }
            l.push_str(&format!(" DEFAULT ({})", d));
        }
        lines.push(l);
    }
    for ix in t.indexes.iter().filter(|i| i.primary_key || i.unique) {
        let cols = ix.columns.iter().map(|(c, d)| format!("{}{}", bracket(c), if *d { " DESC" } else { "" })).collect::<Vec<_>>().join(", ");
        lines.push(format!("    CONSTRAINT {} {} ({})", bracket(&ix.name), if ix.primary_key { "PRIMARY KEY" } else { "UNIQUE" }, cols));
    }
    let mut out = format!("CREATE TABLE {} (\n{}\n);\n", t.quoted(), lines.join(",\n"));
    for ix in t.indexes.iter().filter(|i| !i.primary_key && !i.unique) {
        let cols = ix.columns.iter().map(|(c, d)| format!("{}{}", bracket(c), if *d { " DESC" } else { "" })).collect::<Vec<_>>().join(", ");
        let include = if ix.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", ix.include.iter().map(|c| bracket(c)).collect::<Vec<_>>().join(", ")) };
        let filter = ix.filter.as_ref().map(|f| format!(" WHERE {}", f)).unwrap_or_default();
        out.push_str(&format!("CREATE INDEX {} ON {} ({}){}{};\n", bracket(&ix.name), t.quoted(), cols, include, filter));
    }
    for x in &t.xml_indexes {
        let using = match (&x.using, x.secondary) {
            (Some(u), kind) => format!(
                " USING XML INDEX {}{}",
                bracket(u),
                match kind {
                    Some('P') => " FOR PATH",
                    Some('R') => " FOR PROPERTY",
                    _ => " FOR VALUE",
                }
            ),
            _ => String::new(),
        };
        out.push_str(&format!("CREATE {}XML INDEX {} ON {} ({}){};
", if x.primary { "PRIMARY " } else { "" }, bracket(&x.name), t.quoted(), bracket(&x.column), using));
    }
    for s in &t.extra_ddl {
        out.push_str(s);
        out.push_str(";
");
    }
    for f in &t.foreign_keys {
        let own = f.columns.iter().map(|c| bracket(c)).collect::<Vec<_>>().join(", ");
        let theirs = f.ref_columns.iter().map(|c| bracket(c)).collect::<Vec<_>>().join(", ");
        let target = f.ref_table.split('.').map(bracket).collect::<Vec<_>>().join(".");
        let mut s = format!("ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})", t.quoted(), bracket(&f.name), own, target, theirs);
        if !f.on_delete.is_empty() && !f.on_delete.eq_ignore_ascii_case("NO ACTION") {
            s.push_str(&format!(" ON DELETE {}", f.on_delete));
        }
        if !f.on_update.is_empty() && !f.on_update.eq_ignore_ascii_case("NO ACTION") {
            s.push_str(&format!(" ON UPDATE {}", f.on_update));
        }
        out.push_str(&s);
        out.push_str(";\n");
    }
    for ck in &t.check_defs {
        out.push_str(&format!("ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});
", t.quoted(), bracket(&ck.name), ck.text));
        if !ck.enabled {
            out.push_str(&format!("ALTER TABLE {} NOCHECK CONSTRAINT {};
", t.quoted(), bracket(&ck.name)));
        }
    }
    out
}
