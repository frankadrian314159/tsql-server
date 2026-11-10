//! A SQLsmith-style fuzzer: it generates random, valid-looking T-SQL queries over a small random schema and checks
//! the engine three ways.
//!
//! 1. Crashes and hangs: a panic in the engine, a statement that runs far too long, or an error that names an
//!    internal failure (as opposed to an ordinary SQL error such as a conversion failure) is a bug.
//! 2. Logic oracles, which need no reference implementation: *ternary logic partitioning* (the rows of `F` are
//!    exactly the rows where `p` is true, plus those where it is false, plus those where it is NULL) and *NoREC*
//!    (`SELECT COUNT(*) FROM F WHERE p` must equal summing `p` evaluated per row, which cannot use indexes or
//!    pushdown).
//! 3. Differential execution: the same query on the default executor, with the optimizer off, and on a server with
//!    a tiny work-memory budget (streaming and spilling executors) must return the same rows.
//!
//! `cargo run --release --example sqlsmith [iterations] [--seed N] [--verbose]`
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::panic::{catch_unwind, AssertUnwindSafe};
use tsql_server::{Connection, Server};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Str,
    Dec,
    Date,
}

struct Col {
    name: &'static str,
    ty: Ty,
}

struct Tbl {
    name: &'static str,
    cols: Vec<Col>,
}

/// A column that expressions may refer to: its qualifier and name.
#[derive(Clone)]
struct Avail {
    alias: String,
    name: String,
    ty: Ty,
}

struct Gen {
    rng: StdRng,
    tables: Vec<Tbl>,
    counter: usize,
}

fn tables() -> Vec<Tbl> {
    let c = |name, ty| Col { name, ty };
    vec![
        Tbl { name: "t1", cols: vec![c("id", Ty::Int), c("a", Ty::Int), c("b", Ty::Int), c("c", Ty::Str), c("d", Ty::Dec), c("e", Ty::Date)] },
        Tbl { name: "t2", cols: vec![c("id", Ty::Int), c("a", Ty::Int), c("b", Ty::Int), c("c", Ty::Str), c("d", Ty::Dec), c("e", Ty::Date)] },
        Tbl { name: "t3", cols: vec![c("id", Ty::Int), c("a", Ty::Int), c("b", Ty::Int), c("c", Ty::Str), c("d", Ty::Dec), c("e", Ty::Date)] },
    ]
}

const STRS: [&str; 8] = ["x", "y", "ab", "abc", "ba", "", "zz", "a b"];

fn load(c: &mut Connection, seed: u64) {
    let mut rng = StdRng::seed_from_u64(seed);
    for t in ["t1", "t2", "t3"] {
        let r = c.execute(&format!("CREATE TABLE {} (id INT PRIMARY KEY, a INT, b INT, c VARCHAR(10), d DECIMAL(8,2), e DATE)", t));
        assert!(r.first_error().is_none(), "{:?}", r.first_error());
    }
    for sql in ["CREATE INDEX i1a ON t1 (a)", "CREATE INDEX i1c ON t1 (c)", "CREATE INDEX i2ab ON t2 (a, b)", "CREATE INDEX i2c ON t2 (c)", "CREATE INDEX i3a ON t3 (a)"] {
        let r = c.execute(sql);
        assert!(r.first_error().is_none());
    }
    for (t, n) in [("t1", 40), ("t2", 35), ("t3", 25)] {
        let mut rows = vec![];
        for id in 1..=n {
            let int = |rng: &mut StdRng, hi: i32| if rng.gen_bool(0.15) { "NULL".to_string() } else { rng.gen_range(0..hi).to_string() };
            let s = if rng.gen_bool(0.15) { "NULL".to_string() } else { format!("'{}'", STRS[rng.gen_range(0..STRS.len())]) };
            let d = if rng.gen_bool(0.15) { "NULL".to_string() } else { format!("{}.{:02}", rng.gen_range(-5..40), rng.gen_range(0..100)) };
            let e = if rng.gen_bool(0.15) { "NULL".to_string() } else { format!("'2020-{:02}-{:02}'", rng.gen_range(1..13), rng.gen_range(1..28)) };
            rows.push(format!("({}, {}, {}, {}, {}, {})", id, int(&mut rng, 8), int(&mut rng, 5), s, d, e));
        }
        let r = c.execute(&format!("INSERT {} VALUES {}", t, rows.join(",")));
        assert!(r.first_error().is_none(), "{:?}", r.first_error());
    }
}

impl Gen {
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.rng.gen_range(0..v.len())]
    }
    fn alias(&mut self, p: &str) -> String {
        self.counter += 1;
        format!("{}{}", p, self.counter)
    }
    fn cols_of(&self, scope: &[Avail], ty: Ty) -> Vec<Avail> {
        scope.iter().filter(|c| c.ty == ty).cloned().collect()
    }

    // ------------------------------------------------------------- expressions
    fn int_expr(&mut self, scope: &[Avail], d: u32) -> String {
        let cols = self.cols_of(scope, Ty::Int);
        if d == 0 || self.rng.gen_bool(0.3) {
            if !cols.is_empty() && self.rng.gen_bool(0.75) {
                let c = self.pick(&cols).clone();
                return format!("{}.{}", c.alias, c.name);
            }
            return self.rng.gen_range(-3..10).to_string();
        }
        match self.rng.gen_range(0..11) {
            0 => format!("({} + {})", self.int_expr(scope, d - 1), self.int_expr(scope, d - 1)),
            1 => format!("({} - {})", self.int_expr(scope, d - 1), self.int_expr(scope, d - 1)),
            2 => format!("({} * {})", self.int_expr(scope, d - 1), self.rng.gen_range(-2..4)),
            3 => format!("ABS({})", self.int_expr(scope, d - 1)),
            4 => format!("COALESCE({}, {})", self.int_expr(scope, d - 1), self.rng.gen_range(0..5)),
            5 => format!("(CASE WHEN {} THEN {} ELSE {} END)", self.pred(scope, d - 1), self.int_expr(scope, d - 1), self.int_expr(scope, d - 1)),
            6 => format!("LEN({})", self.str_expr(scope, d - 1)),
            7 => format!("({} % {})", self.int_expr(scope, d - 1), self.rng.gen_range(1..7)),
            8 => format!("NULLIF({}, {})", self.int_expr(scope, d - 1), self.rng.gen_range(0..5)),
            9 => format!("DATEPART(month, {})", self.date_expr(scope, d - 1)),
            _ => {
                if d >= 2 {
                    self.scalar_subquery(scope, d - 1)
                } else {
                    self.rng.gen_range(0..9).to_string()
                }
            }
        }
    }

    fn str_expr(&mut self, scope: &[Avail], d: u32) -> String {
        let cols = self.cols_of(scope, Ty::Str);
        if d == 0 || self.rng.gen_bool(0.35) {
            if !cols.is_empty() && self.rng.gen_bool(0.75) {
                let c = self.pick(&cols).clone();
                return format!("{}.{}", c.alias, c.name);
            }
            return format!("'{}'", STRS[self.rng.gen_range(0..STRS.len())]);
        }
        match self.rng.gen_range(0..9) {
            0 => format!("UPPER({})", self.str_expr(scope, d - 1)),
            1 => format!("LOWER({})", self.str_expr(scope, d - 1)),
            2 => format!("LEFT({}, {})", self.str_expr(scope, d - 1), self.rng.gen_range(0..3)),
            3 => format!("SUBSTRING({}, {}, {})", self.str_expr(scope, d - 1), self.rng.gen_range(1..3), self.rng.gen_range(1..4)),
            4 => format!("CONCAT({}, {})", self.str_expr(scope, d - 1), self.str_expr(scope, d - 1)),
            5 => format!("COALESCE({}, 'n')", self.str_expr(scope, d - 1)),
            6 => format!("CAST({} AS VARCHAR(12))", self.int_expr(scope, d - 1)),
            7 => format!("REPLACE({}, 'a', 'q')", self.str_expr(scope, d - 1)),
            _ => format!("({} + {})", self.str_expr(scope, d - 1), self.str_expr(scope, d - 1)),
        }
    }

    fn dec_expr(&mut self, scope: &[Avail], d: u32) -> String {
        let cols = self.cols_of(scope, Ty::Dec);
        if d == 0 || self.rng.gen_bool(0.35) {
            if !cols.is_empty() && self.rng.gen_bool(0.75) {
                let c = self.pick(&cols).clone();
                return format!("{}.{}", c.alias, c.name);
            }
            return format!("{}.{}", self.rng.gen_range(-2..20), self.rng.gen_range(0..100));
        }
        match self.rng.gen_range(0..5) {
            0 => format!("({} + {})", self.dec_expr(scope, d - 1), self.dec_expr(scope, d - 1)),
            1 => format!("({} - {})", self.dec_expr(scope, d - 1), self.dec_expr(scope, d - 1)),
            2 => format!("ROUND({}, 1)", self.dec_expr(scope, d - 1)),
            3 => format!("CAST({} AS DECIMAL(10,2))", self.int_expr(scope, d - 1)),
            _ => format!("({} * 2)", self.dec_expr(scope, d - 1)),
        }
    }

    fn date_expr(&mut self, scope: &[Avail], d: u32) -> String {
        let cols = self.cols_of(scope, Ty::Date);
        if d == 0 || self.rng.gen_bool(0.4) {
            if !cols.is_empty() && self.rng.gen_bool(0.75) {
                let c = self.pick(&cols).clone();
                return format!("{}.{}", c.alias, c.name);
            }
            return format!("'2020-{:02}-{:02}'", self.rng.gen_range(1..13), self.rng.gen_range(1..28));
        }
        format!("DATEADD(day, {}, {})", self.rng.gen_range(-40..40), self.date_expr(scope, d - 1))
    }

    fn pred(&mut self, scope: &[Avail], d: u32) -> String {
        let cmp = ["=", "<>", "<", "<=", ">", ">="];
        if d == 0 {
            return format!("{} {} {}", self.int_expr(scope, 0), self.pick(&cmp), self.rng.gen_range(0..8));
        }
        match self.rng.gen_range(0..16) {
            0 | 1 | 2 => format!("{} {} {}", self.int_expr(scope, d - 1), self.pick(&cmp), self.int_expr(scope, d - 1)),
            3 => format!("{} {} {}", self.str_expr(scope, d - 1), self.pick(&["=", "<>", "<", ">"]), self.str_expr(scope, d - 1)),
            4 => format!("{} {} {}", self.dec_expr(scope, d - 1), self.pick(&cmp), self.dec_expr(scope, d - 1)),
            5 => format!("{} {} {}", self.date_expr(scope, d - 1), self.pick(&cmp), self.date_expr(scope, d - 1)),
            6 => format!("{} IS {}NULL", self.int_expr(scope, d - 1), if self.rng.gen_bool(0.5) { "NOT " } else { "" }),
            7 => format!("{} BETWEEN {} AND {}", self.int_expr(scope, d - 1), self.rng.gen_range(0..3), self.rng.gen_range(3..8)),
            8 => format!("{} {}IN ({}, {}, {})", self.int_expr(scope, d - 1), if self.rng.gen_bool(0.3) { "NOT " } else { "" }, self.rng.gen_range(0..8), self.rng.gen_range(0..8), self.rng.gen_range(0..8)),
            9 => format!("{} {}LIKE '{}'", self.str_expr(scope, d - 1), if self.rng.gen_bool(0.3) { "NOT " } else { "" }, self.pick(&["a%", "%b", "_b%", "%", "x", "%a%"])),
            10 => format!("({} AND {})", self.pred(scope, d - 1), self.pred(scope, d - 1)),
            11 => format!("({} OR {})", self.pred(scope, d - 1), self.pred(scope, d - 1)),
            12 => format!("NOT ({})", self.pred(scope, d - 1)),
            13 => self.exists(scope, d - 1),
            14 => self.in_subquery(scope, d - 1),
            _ => format!("{} {} {}", self.int_expr(scope, d - 1), self.pick(&cmp), self.scalar_subquery(scope, d - 1)),
        }
    }

    // ------------------------------------------------------------- subqueries
    /// A single-table query whose WHERE may refer to the enclosing scope (a correlated subquery).
    fn sub_from(&mut self, scope: &[Avail]) -> (String, Vec<Avail>, String) {
        let ti = self.rng.gen_range(0..self.tables.len());
        let a = self.alias("s");
        let avail: Vec<Avail> = self.tables[ti].cols.iter().map(|c| Avail { alias: a.clone(), name: c.name.to_string(), ty: c.ty }).collect();
        let name = self.tables[ti].name;
        let mut cond = String::new();
        if self.rng.gen_bool(0.6) && !scope.is_empty() {
            let outer = self.cols_of(scope, Ty::Int);
            if !outer.is_empty() {
                let o = self.pick(&outer).clone();
                let inner = self.cols_of(&avail, Ty::Int);
                let i = self.pick(&inner).clone();
                cond = format!("{}.{} = {}.{}", a, i.name, o.alias, o.name);
            }
        }
        if self.rng.gen_bool(0.5) {
            let p = self.pred(&avail, 1);
            cond = if cond.is_empty() { p } else { format!("{} AND {}", cond, p) };
        }
        (format!("{} {}", name, a), avail, cond)
    }

    fn exists(&mut self, scope: &[Avail], _d: u32) -> String {
        let (from, _, cond) = self.sub_from(scope);
        let neg = if self.rng.gen_bool(0.3) { "NOT " } else { "" };
        format!("{}EXISTS (SELECT 1 FROM {}{})", neg, from, if cond.is_empty() { String::new() } else { format!(" WHERE {}", cond) })
    }

    fn in_subquery(&mut self, scope: &[Avail], _d: u32) -> String {
        let (from, avail, cond) = self.sub_from(scope);
        let ints = self.cols_of(&avail, Ty::Int);
        let col = self.pick(&ints).clone();
        let lhs = self.int_expr(scope, 1);
        let neg = if self.rng.gen_bool(0.3) { "NOT " } else { "" };
        format!("{} {}IN (SELECT {}.{} FROM {}{})", lhs, neg, col.alias, col.name, from, if cond.is_empty() { String::new() } else { format!(" WHERE {}", cond) })
    }

    fn scalar_subquery(&mut self, scope: &[Avail], _d: u32) -> String {
        let (from, avail, cond) = self.sub_from(scope);
        let ints = self.cols_of(&avail, Ty::Int);
        let col = self.pick(&ints).clone();
        let agg = *self.pick(&["MAX", "MIN", "COUNT", "SUM"]);
        format!("(SELECT {}({}.{}) FROM {}{})", agg, col.alias, col.name, from, if cond.is_empty() { String::new() } else { format!(" WHERE {}", cond) })
    }

    // ------------------------------------------------------------- FROM clauses
    /// A FROM clause over one to three tables (or derived tables) and the columns it provides.
    fn from_clause(&mut self) -> (String, Vec<Avail>) {
        let n = self.rng.gen_range(1..4);
        let mut scope: Vec<Avail> = vec![];
        // the tables an ON clause may refer to: those of its own join tree (a comma starts a new one)
        let mut tree: Vec<Avail> = vec![];
        let mut from = String::new();
        for i in 0..n {
            let ti = self.rng.gen_range(0..self.tables.len());
            let a = self.alias("q");
            let name = self.tables[ti].name;
            let cols: Vec<Avail> = self.tables[ti].cols.iter().map(|c| Avail { alias: a.clone(), name: c.name.to_string(), ty: c.ty }).collect();
            let item = if self.rng.gen_bool(0.2) {
                // a derived table over the same columns
                let inner = self.alias("w");
                let icols: Vec<Avail> = self.tables[ti].cols.iter().map(|c| Avail { alias: inner.clone(), name: c.name.to_string(), ty: c.ty }).collect();
                let p = self.pred(&icols, 1);
                format!("(SELECT {} FROM {} {} WHERE {}) {}", icols.iter().map(|c| format!("{}.{}", inner, c.name)).collect::<Vec<_>>().join(", "), name, inner, p, a)
            } else {
                format!("{} {}", name, a)
            };
            if i == 0 {
                from = item;
            } else {
                let kind = *self.pick(&["INNER JOIN", "INNER JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN", ","]);
                if kind == "CROSS JOIN" || kind == "," {
                    from = format!("{} {} {}", from, kind, item);
                    if kind == "," {
                        tree.clear();
                    }
                } else {
                    let mut all = tree.clone();
                    all.extend(cols.iter().cloned());
                    let l = self.cols_of(&tree, Ty::Int);
                    let r = self.cols_of(&cols, Ty::Int);
                    let (lc, rc) = (self.pick(&l).clone(), self.pick(&r).clone());
                    let mut on = format!("{}.{} = {}.{}", lc.alias, lc.name, rc.alias, rc.name);
                    if self.rng.gen_bool(0.4) {
                        on = format!("{} AND {}", on, self.pred(&all, 1));
                    }
                    from = format!("{} {} {} ON {}", from, kind, item, on);
                }
            }
            tree.extend(cols.iter().cloned());
            scope.extend(cols);
        }
        (from, scope)
    }

    /// A random query and whether its row order is irrelevant (always true here: results are compared as sets).
    fn query(&mut self) -> String {
        let (from, scope) = self.from_clause();
        let where_ = if self.rng.gen_bool(0.7) { format!(" WHERE {}", self.pred(&scope, 2)) } else { String::new() };
        match self.rng.gen_range(0..12) {
            6 => {
                // a CTE used twice (once as a scalar subquery source)
                let (v1, v2) = (self.int_expr(&scope, 2), self.int_expr(&scope, 2));
                format!("WITH c AS (SELECT {v1} AS v1, {v2} AS v2 FROM {from}{where_}) SELECT c.v1, COUNT(*) AS n, MAX(c.v2) AS m FROM c WHERE c.v2 IS NULL OR c.v2 >= (SELECT MIN(v2) FROM c) GROUP BY c.v1", v1 = v1, v2 = v2, from = from, where_ = where_)
            }
            7 => {
                // CROSS / OUTER APPLY of a correlated derived table
                let ti = self.rng.gen_range(0..3);
                let (t1, t2) = (self.tables[ti].name, self.tables[self.rng.gen_range(0..3)].name);
                let col = *self.pick(&["a", "b"]);
                let inner = self.pick(&[
                    format!("SELECT TOP 1 s.a AS v FROM {} s WHERE s.{} = q.{} ORDER BY s.id", t2, col, col),
                    format!("SELECT COUNT(*) AS v FROM {} s WHERE s.{} = q.{}", t2, col, col),
                    format!("SELECT MAX(s.b) AS v FROM {} s WHERE s.{} < q.{}", t2, col, col),
                ])
                .clone();
                let kind = *self.pick(&["CROSS APPLY", "OUTER APPLY"]);
                format!("SELECT q.id, x.v FROM {} q {} ({}) x", t1, kind, inner)
            }
            8 => {
                let t = self.tables[self.rng.gen_range(0..3)].name;
                let kind = *self.pick(&["ROLLUP", "CUBE"]);
                format!("SELECT q.a, q.b, GROUPING(q.a) AS ga, GROUPING(q.b) AS gb, COUNT(*) AS n, SUM(q.id) AS s FROM {} q GROUP BY {} (q.a, q.b)", t, kind)
            }
            9 => {
                // windows with explicit frames, ordered by the unique key so the answer is deterministic
                let t = self.tables[self.rng.gen_range(0..3)].name;
                let frame = *self.pick(&["ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING", "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW", "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING", "ROWS BETWEEN 2 PRECEDING AND CURRENT ROW"]);
                format!("SELECT q.id, ROW_NUMBER() OVER (PARTITION BY q.a ORDER BY q.id) AS rn, SUM(q.b) OVER (PARTITION BY q.a ORDER BY q.id {f}) AS s, MIN(q.b) OVER (ORDER BY q.id {f}) AS mn, LAG(q.b) OVER (ORDER BY q.id) AS lg, LEAD(q.c, 2, 'z') OVER (PARTITION BY q.a ORDER BY q.id) AS ld, FIRST_VALUE(q.c) OVER (PARTITION BY q.a ORDER BY q.id) AS fv, COUNT(q.b) OVER (PARTITION BY q.a) AS cb FROM {t} q", f = frame, t = t)
            }
            10 => {
                let ti = self.rng.gen_range(0..3);
                let t = self.tables[ti].name;
                let avail: Vec<Avail> = self.tables[ti].cols.iter().map(|c| Avail { alias: "q".to_string(), name: c.name.to_string(), ty: c.ty }).collect();
                let p = self.pred(&avail, 2);
                let x = self.int_expr(&avail, 2);
                format!("SELECT TOP {} q.id, {} AS x FROM {} q WHERE {} ORDER BY q.id", self.rng.gen_range(1..15), x, t, p)
            }
            11 => {
                let t = self.tables[self.rng.gen_range(0..3)].name;
                format!("SELECT q.a, STRING_AGG(q.c, ',') WITHIN GROUP (ORDER BY q.id) AS cs, COUNT(DISTINCT q.b) AS nb FROM {} q GROUP BY q.a", t)
            }
            0 | 1 => {
                // aggregate with GROUP BY
                let g1 = self.int_expr(&scope, 1);
                let agg = *self.pick(&["COUNT(*)", "SUM(1)"]);
                let x = self.int_expr(&scope, 1);
                let having = if self.rng.gen_bool(0.4) { format!(" HAVING COUNT(*) >= {}", self.rng.gen_range(0..3)) } else { String::new() };
                format!("SELECT {g} AS g, {agg} AS n, MIN({x}) AS mn, MAX({x}) AS mx, SUM({x}) AS sm FROM {from}{where_} GROUP BY {g}{having}", g = g1, agg = agg, x = x, from = from, where_ = where_, having = having)
            }
            2 => {
                let items: Vec<String> = (0..self.rng.gen_range(1..4)).map(|i| format!("{} AS c{}", self.int_expr(&scope, 2), i)).collect();
                format!("SELECT DISTINCT {} FROM {}{}", items.join(", "), from, where_)
            }
            3 => {
                // a window over a partition (deterministic: no ROW_NUMBER over ties)
                let p = self.int_expr(&scope, 1);
                let x = self.int_expr(&scope, 1);
                format!("SELECT {x} AS x, SUM({x}) OVER (PARTITION BY {p}) AS s, COUNT(*) OVER (PARTITION BY {p}) AS n, RANK() OVER (ORDER BY {x}) AS r FROM {from}{where_}", x = x, p = p, from = from, where_ = where_)
            }
            4 => {
                // a set operation between two int queries
                let (from2, scope2) = self.from_clause();
                let op = *self.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
                let (a, b) = (self.int_expr(&scope, 2), self.int_expr(&scope2, 2));
                let w2 = if self.rng.gen_bool(0.5) { format!(" WHERE {}", self.pred(&scope2, 1)) } else { String::new() };
                format!("SELECT {} AS v FROM {}{} {} SELECT {} FROM {}{}", a, from, where_, op, b, from2, w2)
            }
            _ => {
                let items: Vec<String> = (0..self.rng.gen_range(1..5))
                    .map(|i| match self.rng.gen_range(0..4) {
                        0 => format!("{} AS c{}", self.str_expr(&scope, 2), i),
                        1 => format!("{} AS c{}", self.dec_expr(&scope, 2), i),
                        _ => format!("{} AS c{}", self.int_expr(&scope, 2), i),
                    })
                    .collect();
                format!("SELECT {} FROM {}{}", items.join(", "), from, where_)
            }
        }
    }
}

// ------------------------------------------------------------------ execution
type Rows = Result<Vec<Vec<String>>, (i32, String)>;

fn run(c: &mut Connection, sql: &str) -> Result<Rows, String> {
    let r = catch_unwind(AssertUnwindSafe(|| c.execute(sql)));
    let r = match r {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            return Err(format!("PANIC: {}", msg));
        }
    };
    if let Some(e) = r.first_error() {
        return Ok(Err((e.number, format!("{}", e))));
    }
    Ok(Ok(r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect()).unwrap_or_default()))
}

fn close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 0.011 + 1e-9 * x.abs().max(y.abs()),
        _ => false,
    }
}

fn same(a: &[Vec<String>], b: &[Vec<String>]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} rows against {}", a.len(), b.len()));
    }
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort();
    b.sort();
    for (x, y) in a.iter().zip(b.iter()) {
        if x.len() != y.len() || x.iter().zip(y.iter()).any(|(u, v)| u != v && !close(u, v)) {
            return Some(format!("{:?} against {:?}", x, y));
        }
    }
    None
}

/// Error numbers a generated query may legitimately provoke. Anything else, or text that names an internal failure,
/// is reported.
fn suspicious(num: i32, msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("internal") || m.contains("panic") || m.contains("unexpected") || m.contains("not implemented") || m.contains("unsupported") || num == 0
}

fn scalar_i64(r: &Rows) -> Option<i64> {
    match r {
        Ok(rows) if rows.len() == 1 && rows[0].len() == 1 => {
            if rows[0][0] == "NULL" {
                Some(0)
            } else {
                rows[0][0].parse::<f64>().ok().map(|f| f as i64)
            }
        }
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let iterations: u64 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(1500);
    let seed: u64 = args.iter().position(|a| a == "--seed").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(1);
    let verbose = args.iter().any(|a| a == "--verbose");

    // three servers holding the same data: the default executors, a tiny work-memory budget (streaming and spilling
    // executors everywhere), and a session with the optimizer off
    let (sa, sb, sc) = (Server::new(), Server::new(), Server::new());
    sb.set_work_mem(64 << 10, 0, None);
    let (mut conn_a, mut conn_b, mut off) = (sa.connect(), sb.connect(), sc.connect());
    for c in [&mut conn_a, &mut conn_b, &mut off] {
        load(c, 99);
    }
    assert!(off.execute("SET OPTIMIZER OFF").first_error().is_none());

    // a watchdog: a statement that does not finish in a minute is a hang
    let current = std::sync::Arc::new(std::sync::Mutex::new((std::time::Instant::now(), String::new())));
    {
        let current = current.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let g = current.lock().unwrap();
            if g.0.elapsed().as_secs() > 60 && !g.1.is_empty() {
                eprintln!("HANG (over 60s): {}", g.1);
                std::process::exit(2);
            }
        });
    }
    let watch = |sql: &str| {
        let mut g = current.lock().unwrap();
        *g = (std::time::Instant::now(), sql.to_string());
    };

    let mut g = Gen { rng: StdRng::seed_from_u64(seed), tables: tables(), counter: 0 };
    let (mut ok, mut errors, mut problems) = (0u64, 0u64, 0u64);
    let mut error_kinds: std::collections::BTreeMap<i32, (u64, String)> = Default::default();
    let mut report = |what: &str, sql: &str, detail: &str, problems: &mut u64| {
        *problems += 1;
        if *problems <= 25 {
            println!("PROBLEM ({}): {}\n   {}\n   {}", what, detail, sql, "");
        }
    };
    for it in 0..iterations {
        let sql = g.query();
        watch(&sql);
        let ra = match run(&mut conn_a, &sql) {
            Ok(r) => r,
            Err(p) => {
                report("crash", &sql, &p, &mut problems);
                continue;
            }
        };
        match &ra {
            Ok(_) => ok += 1,
            Err((n, m)) => {
                errors += 1;
                error_kinds.entry(*n).or_insert((0, m.clone())).0 += 1;
                if suspicious(*n, m) {
                    report("suspicious error", &sql, m, &mut problems);
                }
            }
        }
        if verbose && it % 100 == 0 {
            println!("[{}] {}", it, sql);
        }
        // differential: optimizer off, and the spilling server
        for (label, conn) in [("optimizer off", &mut off), ("spilling executors", &mut conn_b)] {
            watch(&sql);
            match run(conn, &sql) {
                Err(p) => report("crash", &sql, &format!("{}: {}", label, p), &mut problems),
                Ok(rb) => match (&ra, &rb) {
                    (Ok(x), Ok(y)) => {
                        if let Some(d) = same(x, y) {
                            report("different answers", &sql, &format!("default vs {}: {}", label, d), &mut problems);
                        }
                    }
                    (Err((n1, _)), Err((n2, _))) if n1 == n2 => {}
                    (Ok(_), Err((n, m))) => report("different outcome", &sql, &format!("default succeeded, {} failed with {}: {}", label, n, m), &mut problems),
                    (Err((n, m)), Ok(_)) => report("different outcome", &sql, &format!("default failed with {}: {}, {} succeeded", n, m, label), &mut problems),
                    (Err((n1, m1)), Err((n2, m2))) => report("different errors", &sql, &format!("{}: {} against {}: {}", n1, m1, n2, m2), &mut problems),
                },
            }
        }
        // data changes, applied to all three servers; the tables must stay identical
        if it % 20 == 7 {
            let ti = g.rng.gen_range(0..3);
            let t = g.tables[ti].name;
            let avail: Vec<Avail> = g.tables[ti].cols.iter().map(|c| Avail { alias: "q".to_string(), name: c.name.to_string(), ty: c.ty }).collect();
            let p = g.pred(&avail, 2);
            let dml = match g.rng.gen_range(0..4) {
                0 => format!("UPDATE q SET a = {} FROM {} q WHERE {}", g.int_expr(&avail, 2), t, p),
                1 => format!("UPDATE q SET c = {}, d = {} FROM {} q WHERE {}", g.str_expr(&avail, 1), g.dec_expr(&avail, 1), t, p),
                2 => format!("DELETE q FROM {} q WHERE {}", t, p),
                _ => {
                    let src = g.tables[g.rng.gen_range(0..3)].name;
                    format!("INSERT INTO {t} (id, a, b, c, d, e) SELECT (SELECT ISNULL(MAX(id), 0) FROM {t}) + ROW_NUMBER() OVER (ORDER BY s.id), s.a, s.b, s.c, s.d, s.e FROM {src} s WHERE s.id <= {n}", t = t, src = src, n = g.rng.gen_range(1..8))
                }
            };
            watch(&dml);
            let outcomes: Vec<Result<Rows, String>> = [&mut conn_a, &mut conn_b, &mut off].into_iter().map(|c| run(c, &dml)).collect();
            let kinds: Vec<String> = outcomes
                .iter()
                .map(|o| match o {
                    Err(p) => p.clone(),
                    Ok(Err((n, _))) => format!("error {}", n),
                    Ok(Ok(_)) => "ok".to_string(),
                })
                .collect();
            if kinds.iter().any(|k| k != &kinds[0]) || kinds[0].starts_with("PANIC") {
                report("different outcome of a data change", &dml, &format!("{:?}", kinds), &mut problems);
            }
            if let Ok(Err((n, m))) = &outcomes[0] {
                if suspicious(*n, m) {
                    report("suspicious error", &dml, m, &mut problems);
                }
            }
            for tn in ["t1", "t2", "t3"] {
                let q = format!("SELECT * FROM {} ORDER BY id", tn);
                let states: Vec<Rows> = [&mut conn_a, &mut conn_b, &mut off].into_iter().map(|c| run(c, &q).unwrap_or(Err((0, "panic".into())))).collect();
                for (label, st) in [("spilling server", &states[1]), ("optimizer-off session", &states[2])] {
                    if let (Ok(x), Ok(y)) = (&states[0], st) {
                        if let Some(d) = same(x, y) {
                            report("tables diverged after a data change", &dml, &format!("{} differs from the default in {}: {}", label, tn, d), &mut problems);
                        }
                    }
                }
            }
        }
        // every so often start again from the initial data (the changes above would otherwise empty the tables)
        if it % 100 == 99 {
            for c in [&mut conn_a, &mut conn_b, &mut off] {
                for t in ["t1", "t2", "t3"] {
                    let r = c.execute(&format!("DROP TABLE {}", t));
                    assert!(r.first_error().is_none(), "{:?}", r.first_error());
                }
                load(c, 99);
            }
        }
        // logic oracles on a predicate over a fresh FROM clause
        if it % 2 == 0 {
            let (from, scope) = g.from_clause();
            let p = g.pred(&scope, 3);
            let q = |w: &str| format!("SELECT COUNT(*) FROM {}{}", from, w);
            let base = q("");
            let (t, f, n) = (q(&format!(" WHERE {}", p)), q(&format!(" WHERE NOT ({})", p)), q(&format!(" WHERE (CASE WHEN {p} THEN 0 WHEN NOT ({p}) THEN 0 ELSE 1 END) = 1", p = p)));
            let norec = format!("SELECT SUM(CASE WHEN {} THEN 1 ELSE 0 END) FROM {}", p, from);
            watch(&t);
            let rs: Vec<Result<Rows, String>> = [&base, &t, &f, &n, &norec].iter().map(|s| run(&mut conn_a, s)).collect();
            let mut vals = vec![];
            let mut failed = false;
            for (i, r) in rs.into_iter().enumerate() {
                match r {
                    Err(pn) => {
                        report("crash", [&base, &t, &f, &n, &norec][i], &pn, &mut problems);
                        failed = true;
                    }
                    Ok(Err((num, m))) => {
                        if suspicious(num, &m) {
                            report("suspicious error", [&base, &t, &f, &n, &norec][i], &m, &mut problems);
                        }
                        failed = true;
                    }
                    Ok(r) => vals.push(scalar_i64(&Ok(match r {
                        Ok(rows) => rows,
                        Err(_) => unreachable!(),
                    }))),
                }
            }
            if !failed && vals.len() == 5 {
                if let (Some(b), Some(t), Some(f), Some(n), Some(nr)) = (vals[0], vals[1], vals[2], vals[3], vals[4]) {
                    if t + f + n != b {
                        report("ternary logic partitioning", &format!("FROM {} WHERE {}", from, p), &format!("true {} + false {} + null {} <> all {}", t, f, n, b), &mut problems);
                    }
                    if t != nr {
                        report("NoREC", &format!("FROM {} WHERE {}", from, p), &format!("filtered count {} <> per-row count {}", t, nr), &mut problems);
                    }
                }
            }
        }
    }
    println!("{} queries: {} ran, {} raised SQL errors; {} problem(s)", iterations, ok, errors, problems);
    println!("errors raised by generated queries (number: count, first message):");
    for (n, (c, m)) in &error_kinds {
        println!("   {}: {} x  {}", n, c, m.chars().take(100).collect::<String>());
    }
    if problems > 0 {
        std::process::exit(1);
    }
}
