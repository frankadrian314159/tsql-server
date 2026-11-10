//! `BULK INSERT table FROM 'file' WITH (...)`: load a delimited text file into a table.
//!
//! The file is read on the server, split into records and fields, and inserted by one ordinary INSERT, so the
//! statement is atomic, fires triggers, checks constraints and honours permissions and row-level security like any
//! other insert. Supported options: `FIELDTERMINATOR`, `ROWTERMINATOR`, `FIRSTROW`, `LASTROW`, `KEEPNULLS`,
//! `KEEPIDENTITY`, `FORMAT = 'CSV'` with `FIELDQUOTE`, `DATAFILETYPE` (`char`, `widechar`), and, accepted and ignored,
//! `BATCHSIZE`, `TABLOCK`, `CODEPAGE`, `MAXERRORS`, `ERRORFILE`, `CHECK_CONSTRAINTS`, `FIRE_TRIGGERS`.
use super::*;
use crate::engine::query::Env;

/// Expand `\n`, `\r`, `\t`, `\\`, `\0` and `0x..` hex escapes in a terminator.
fn terminator(raw: &str) -> String {
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect();
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let mut out = String::new();
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split `text` into records of fields. In CSV format a field may be quoted (with `quote`), containing field and row
/// terminators and doubled quotes.
fn split_records(text: &str, field: &str, row: &str, csv_quote: Option<char>) -> Vec<Vec<String>> {
    // a record terminated by "\n" also accepts "\r\n"
    let row_is_lf = row == "\n" || row == "\r\n";
    let mut records = vec![];
    let mut cur: Vec<String> = vec![];
    let mut f = String::new();
    let mut in_quote = false;
    let mut field_quoted = false;
    let b = text;
    let mut i = 0;
    let mut any = false;
    while i < b.len() {
        let rest = &b[i..];
        let c = rest.chars().next().unwrap();
        any = true;
        if in_quote {
            if Some(c) == csv_quote {
                if rest[c.len_utf8()..].starts_with(c) {
                    f.push(c);
                    i += 2 * c.len_utf8();
                } else {
                    in_quote = false;
                    i += c.len_utf8();
                }
            } else {
                f.push(c);
                i += c.len_utf8();
            }
            continue;
        }
        if Some(c) == csv_quote && f.is_empty() && !field_quoted {
            in_quote = true;
            field_quoted = true;
            i += c.len_utf8();
            continue;
        }
        if !field.is_empty() && rest.starts_with(field) {
            cur.push(std::mem::take(&mut f));
            field_quoted = false;
            i += field.len();
            continue;
        }
        let row_len = if row_is_lf {
            if rest.starts_with("\r\n") {
                2
            } else if rest.starts_with('\n') {
                1
            } else {
                0
            }
        } else if !row.is_empty() && rest.starts_with(row) {
            row.len()
        } else {
            0
        };
        if row_len > 0 {
            cur.push(std::mem::take(&mut f));
            records.push(std::mem::take(&mut cur));
            field_quoted = false;
            i += row_len;
            any = false;
            continue;
        }
        f.push(c);
        i += c.len_utf8();
    }
    if any || !cur.is_empty() || !f.is_empty() {
        cur.push(f);
        records.push(cur);
    }
    records
}

fn decode(bytes: &[u8], wide: bool) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) || wide {
        let b = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let b = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8_lossy(b).into_owned()
}

impl<'a> Engine<'a> {
    pub fn exec_bulk_insert(&mut self, b: &BulkStmt) -> Result<()> {
        if !self.is_sysadmin() {
            return Err(SqlError::with(4834, 14, 1, "You do not have permission to use the bulk load statement."));
        }
        let path = {
            let v = self.ev(&b.path, &Env::empty())?;
            convert::to_str(&v)
        };
        let mut opt: HashMap<String, Option<String>> = HashMap::new();
        for (n, e) in &b.opts {
            let v = match e {
                Some(e) => {
                    let v = self.ev(e, &Env::empty())?;
                    Some(convert::to_str(&v))
                }
                None => None,
            };
            opt.insert(n.clone(), v);
        }
        let get = |k: &str| opt.get(k).and_then(|v| v.clone());
        let num = |k: &str| -> Result<Option<usize>> {
            match get(k) {
                Some(s) => s.trim().parse::<usize>().map(Some).map_err(|_| SqlError::with(4860, 16, 1, format!("The value of option {} is not a valid number.", k))),
                None => Ok(None),
            }
        };
        let csv = get("FORMAT").is_some_and(|f| f.eq_ignore_ascii_case("CSV"));
        let field = terminator(&get("FIELDTERMINATOR").unwrap_or_else(|| if csv { ",".into() } else { "\t".into() }));
        let row = terminator(&get("ROWTERMINATOR").unwrap_or_else(|| "\n".into()));
        let quote = if csv { Some(get("FIELDQUOTE").and_then(|q| q.chars().next()).unwrap_or('"')) } else { None };
        let first = num("FIRSTROW")?.unwrap_or(1).max(1);
        let last = num("LASTROW")?;
        let wide = get("DATAFILETYPE").is_some_and(|t| t.to_lowercase().starts_with("wide"));
        let keep_nulls = opt.contains_key("KEEPNULLS");
        let keep_identity = opt.contains_key("KEEPIDENTITY");

        let bytes = std::fs::read(&path).map_err(|e| SqlError::with(4861, 16, 1, format!("Cannot bulk load because the file \"{}\" could not be opened. Operating system error: {}.", path, e)))?;
        let text = decode(&bytes, wide);
        let mut records = split_records(&text, &field, &row, quote);
        // FIRSTROW and LASTROW count records, starting at 1
        let upto = last.map(|l| l.min(records.len())).unwrap_or(records.len());
        records = if first > upto { vec![] } else { records[first - 1..upto].to_vec() };

        let (loc, name) = {
            let target = Target::Table(b.table.clone());
            self.dml_target_for_bulk(&target)?
        };
        let cols: Vec<(String, bool)> = {
            let t = self.table_ref(&loc).ok_or_else(|| self.invalid_object(&name))?;
            t.columns.iter().filter(|c| c.computed.is_none() && (keep_identity || c.identity.is_none())).map(|c| (c.name.clone(), c.default.is_some())).collect()
        };
        let mut rows: Vec<Vec<Expr>> = Vec::with_capacity(records.len());
        for (n, r) in records.iter().enumerate() {
            if r.len() != cols.len() {
                return Err(SqlError::with(
                    4832,
                    16,
                    1,
                    format!("Bulk load: An unexpected end of file was encountered in the data file. Record {} has {} field(s); the table needs {}.", first + n, r.len(), cols.len()),
                ));
            }
            rows.push(
                r.iter()
                    .zip(&cols)
                    .map(|(v, (_, has_default))| {
                        if v.is_empty() {
                            if *has_default && !keep_nulls {
                                Expr::Default
                            } else {
                                Expr::Lit(Value::Null)
                            }
                        } else {
                            Expr::Lit(Value::Str(v.clone()))
                        }
                    })
                    .collect(),
            );
        }
        let n = rows.len();
        let ins = Insert { ctes: vec![], target: Target::Table(b.table.clone()), cols: Some(cols.iter().map(|c| c.0.clone()).collect()), output: None, src: InsertSrc::Values(rows), top: None };
        let key = format!("{:?}", loc);
        let had = self.ses.identity_insert.contains(&key);
        if keep_identity {
            self.ses.identity_insert.insert(key.clone());
        }
        let r = if n == 0 { Ok(()) } else { self.exec_insert(&ins) };
        if keep_identity && !had {
            self.ses.identity_insert.remove(&key);
        }
        r?;
        self.ses.rowcount = n as i64;
        Ok(())
    }

    fn dml_target_for_bulk(&mut self, target: &Target) -> Result<(Loc, Vec<String>)> {
        match target {
            Target::Table(name) => {
                let loc = self.locate(name);
                if self.table_ref(&loc).is_none() {
                    return Err(self.invalid_object(name));
                }
                Ok((loc, name.clone()))
            }
            Target::Var(v) => Err(SqlError::new(1087, format!("Must declare the table variable \"{}\".", v))),
        }
    }
}
