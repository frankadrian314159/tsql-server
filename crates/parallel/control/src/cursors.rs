//! `DECLARE name CURSOR FOR query`, `OPEN`, `FETCH [NEXT | PRIOR | FIRST | LAST | ABSOLUTE n | RELATIVE n] FROM name INTO @v, ...`, `CLOSE` and
//! `DEALLOCATE`, and `@@FETCH_STATUS`, for the statements the control node runs (a batch, a procedure, the body of a trigger) over the
//! cluster's tables.
//!
//! A cursor is a *static snapshot*: `OPEN` runs the query (with the variables it names as they are then) and keeps the rows; `FETCH` walks
//! them. A change made to the tables after the `OPEN` is not seen, as with `STATIC` or `INSENSITIVE` cursors of a single server (a
//! cursor of another kind sees changes there; here none does). `FETCH` needs `INTO`: a `FETCH` with no `INTO` that would return the row as
//! a result set is not supported. A cursor belongs to the batch that declared it (a trigger's, to the trigger).
use super::*;

/// A declared cursor.
pub(crate) struct CursorState {
    query: Query,
    /// the rows, once opened
    rows: Option<Vec<Vec<Value>>>,
    /// the row the cursor is on: -1 before the first, the number of rows after the last
    pos: i64,
}

impl ClusterSession {
    pub(crate) async fn exec_cursor(&mut self, st: &Stmt) -> Result<(), SqlError> {
        match st {
            Stmt::DeclareCursor { name, query } => {
                let key = name.to_lowercase();
                if self.cursors.contains_key(&key) {
                    return Err(SqlError::new(16905, format!("A cursor with the name '{}' already exists.", name)));
                }
                self.cursors.insert(key, CursorState { query: (**query).clone(), rows: None, pos: -1 });
                Ok(())
            }
            Stmt::CursorOp { op, name } => {
                let key = name.to_lowercase();
                if !self.cursors.contains_key(&key) {
                    return Err(SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", name)));
                }
                if op.eq_ignore_ascii_case("OPEN") {
                    if self.cursors[&key].rows.is_some() {
                        return Err(SqlError::new(16905, format!("The cursor is already open.")));
                    }
                    let q = self.cursors[&key].query.clone();
                    // (the variables the query names are those of the moment it is opened)
                    let q = match self.bind_vars(&Stmt::Query(Box::new(q)))? {
                        Stmt::Query(q) => *q,
                        _ => return Err(unsupported("a cursor over this query")),
                    };
                    let rows = self.run_query(&q).await?.rows;
                    let c = self.cursors.get_mut(&key).expect("a cursor");
                    c.rows = Some(rows);
                    c.pos = -1;
                    self.fetch_status = 0;
                    Ok(())
                } else if op.eq_ignore_ascii_case("CLOSE") {
                    let c = self.cursors.get_mut(&key).expect("a cursor");
                    if c.rows.is_none() {
                        return Err(SqlError::new(16917, format!("Cursor is not open.")));
                    }
                    c.rows = None;
                    Ok(())
                } else if op.eq_ignore_ascii_case("DEALLOCATE") {
                    self.cursors.remove(&key);
                    Ok(())
                } else {
                    Err(unsupported(&format!("the cursor statement {}", op)))
                }
            }
            Stmt::Fetch { orient, arg, name, into } => {
                let key = name.to_lowercase();
                let Some(c) = self.cursors.get(&key) else { return Err(SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", name))) };
                let Some(rows) = &c.rows else { return Err(SqlError::new(16917, "Cursor is not open.")) };
                let len = rows.len() as i64;
                let pos = c.pos;
                let n = match arg {
                    Some(e) => {
                        let b = self.bind_expr(e)?;
                        self.eval_scalar(&b).await?.as_i64().unwrap_or(0)
                    }
                    None => 0,
                };
                let target = match orient.to_uppercase().as_str() {
                    "" | "NEXT" => pos + 1,
                    "PRIOR" => pos - 1,
                    "FIRST" => 0,
                    "LAST" => len - 1,
                    "ABSOLUTE" => {
                        if n > 0 {
                            n - 1
                        } else if n < 0 {
                            len + n
                        } else {
                            -1
                        }
                    }
                    "RELATIVE" => pos + n,
                    other => return Err(unsupported(&format!("FETCH {}", other))),
                };
                let Some(into) = into else { return Err(unsupported("FETCH without INTO")) };
                let c = self.cursors.get_mut(&key).expect("a cursor");
                if target < 0 || target >= len {
                    // (off either end: the cursor stands before the first row or after the last, and nothing is fetched)
                    c.pos = if target < 0 { -1 } else { len };
                    self.fetch_status = -1;
                    return Ok(());
                }
                c.pos = target;
                let row = c.rows.as_ref().expect("rows")[target as usize].clone();
                if into.len() > row.len() {
                    return Err(SqlError::new(16924, "Cursor fetch with too many variables."));
                }
                for (v, val) in into.iter().zip(row) {
                    self.vars.insert(v.to_lowercase(), val);
                }
                self.fetch_status = 0;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
