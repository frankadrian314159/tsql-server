//! TPC-C: nine tables, the five transactions as stored procedures (New-Order, Payment, Order-Status, Delivery,
//! Stock-Level), the specification's transaction mix, and its consistency conditions checked after the run.
//!
//! Scaled down from the specification (fewer customers per district and items), without think times or keying times,
//! and run in process, so the numbers measure the engine under concurrent transactions rather than a certified tpmC.
use crate::util::*;
use std::time::Instant;
use tsql_server::Connection;

pub struct Cfg {
    pub warehouses: i64,
    pub customers: i64, // per district
    pub items: i64,
}

const SCHEMA: &str = "
CREATE TABLE warehouse (w_id INT PRIMARY KEY, w_name VARCHAR(10), w_tax DECIMAL(4,4), w_ytd DECIMAL(12,2));
CREATE TABLE district (d_w_id INT, d_id INT, d_name VARCHAR(10), d_tax DECIMAL(4,4), d_ytd DECIMAL(12,2), d_next_o_id INT, PRIMARY KEY (d_w_id, d_id));
CREATE TABLE customer (c_w_id INT, c_d_id INT, c_id INT, c_first VARCHAR(16), c_middle CHAR(2), c_last VARCHAR(16), c_credit CHAR(2), c_discount DECIMAL(4,4), c_balance DECIMAL(12,2), c_ytd_payment DECIMAL(12,2), c_payment_cnt INT, c_delivery_cnt INT, c_data VARCHAR(100), PRIMARY KEY (c_w_id, c_d_id, c_id));
CREATE INDEX ix_customer_name ON customer (c_w_id, c_d_id, c_last, c_first);
CREATE TABLE history (h_c_id INT, h_c_d_id INT, h_c_w_id INT, h_d_id INT, h_w_id INT, h_date DATETIME, h_amount DECIMAL(6,2), h_data VARCHAR(24));
CREATE INDEX ix_history_c ON history (h_c_w_id, h_c_d_id, h_c_id);
CREATE TABLE orders (o_w_id INT, o_d_id INT, o_id INT, o_c_id INT, o_entry_d DATETIME, o_carrier_id INT NULL, o_ol_cnt INT, o_all_local INT, PRIMARY KEY (o_w_id, o_d_id, o_id));
CREATE INDEX ix_orders_c ON orders (o_w_id, o_d_id, o_c_id, o_id);
CREATE TABLE new_order (no_w_id INT, no_d_id INT, no_o_id INT, PRIMARY KEY (no_w_id, no_d_id, no_o_id));
CREATE TABLE item (i_id INT PRIMARY KEY, i_name VARCHAR(24), i_price DECIMAL(5,2), i_data VARCHAR(50));
CREATE TABLE stock (s_w_id INT, s_i_id INT, s_quantity INT, s_ytd INT, s_order_cnt INT, s_remote_cnt INT, s_data VARCHAR(50), PRIMARY KEY (s_w_id, s_i_id));
CREATE TABLE order_line (ol_w_id INT, ol_d_id INT, ol_o_id INT, ol_number INT, ol_i_id INT, ol_supply_w_id INT, ol_delivery_d DATETIME NULL, ol_quantity INT, ol_amount DECIMAL(9,2), ol_dist_info CHAR(24), PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number));
";

const PROCS: [&str; 5] = [
    "CREATE PROCEDURE neword @w_id INT, @d_id INT, @c_id INT, @lines VARCHAR(400), @all_local INT AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @o_id INT, @w_tax DECIMAL(4,4), @d_tax DECIMAL(4,4), @c_disc DECIMAL(4,4), @total DECIMAL(12,2), @ol_cnt INT;
  DECLARE @tok VARCHAR(40), @sep INT, @p1 INT, @p2 INT, @i_id INT, @s_w INT, @qty INT, @price DECIMAL(5,2), @s_q INT, @amt DECIMAL(9,2), @n INT, @remote INT;
  SET @total = 0; SET @n = 0;
  BEGIN TRAN;
  SELECT @w_tax = w_tax FROM warehouse WHERE w_id = @w_id;
  SELECT @o_id = d_next_o_id, @d_tax = d_tax FROM district WITH (UPDLOCK) WHERE d_w_id = @w_id AND d_id = @d_id;
  UPDATE district SET d_next_o_id = @o_id + 1 WHERE d_w_id = @w_id AND d_id = @d_id;
  SELECT @c_disc = c_discount FROM customer WHERE c_w_id = @w_id AND c_d_id = @d_id AND c_id = @c_id;
  SET @ol_cnt = LEN(@lines) - LEN(REPLACE(@lines, ';', '')) + 1;
  INSERT orders (o_w_id, o_d_id, o_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) VALUES (@w_id, @d_id, @o_id, @c_id, GETDATE(), NULL, @ol_cnt, @all_local);
  INSERT new_order (no_w_id, no_d_id, no_o_id) VALUES (@w_id, @d_id, @o_id);
  WHILE LEN(@lines) > 0
  BEGIN
    SET @sep = CHARINDEX(';', @lines);
    IF @sep = 0 BEGIN SET @tok = @lines; SET @lines = ''; END
    ELSE BEGIN SET @tok = LEFT(@lines, @sep - 1); SET @lines = SUBSTRING(@lines, @sep + 1, 400); END
    SET @p1 = CHARINDEX(':', @tok);
    SET @i_id = CAST(LEFT(@tok, @p1 - 1) AS INT);
    SET @tok = SUBSTRING(@tok, @p1 + 1, 40);
    SET @p2 = CHARINDEX(':', @tok);
    SET @s_w = CAST(LEFT(@tok, @p2 - 1) AS INT);
    SET @qty = CAST(SUBSTRING(@tok, @p2 + 1, 10) AS INT);
    SET @price = NULL;
    SELECT @price = i_price FROM item WHERE i_id = @i_id;
    IF @price IS NULL BEGIN ROLLBACK TRAN; RETURN 1; END
    SELECT @s_q = s_quantity FROM stock WITH (UPDLOCK) WHERE s_i_id = @i_id AND s_w_id = @s_w;
    SET @remote = CASE WHEN @s_w <> @w_id THEN 1 ELSE 0 END;
    UPDATE stock SET s_quantity = CASE WHEN @s_q - @qty >= 10 THEN @s_q - @qty ELSE @s_q - @qty + 91 END, s_ytd = s_ytd + @qty, s_order_cnt = s_order_cnt + 1, s_remote_cnt = s_remote_cnt + @remote WHERE s_i_id = @i_id AND s_w_id = @s_w;
    SET @n = @n + 1;
    SET @amt = @qty * @price;
    SET @total = @total + @amt;
    INSERT order_line (ol_w_id, ol_d_id, ol_o_id, ol_number, ol_i_id, ol_supply_w_id, ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) VALUES (@w_id, @d_id, @o_id, @n, @i_id, @s_w, NULL, @qty, @amt, 'dist');
  END
  SET @total = @total * (1 - @c_disc) * (1 + @w_tax + @d_tax);
  COMMIT TRAN;
  SELECT @o_id AS o_id, @total AS total;
  RETURN 0;
END",
    "CREATE PROCEDURE payment @w_id INT, @d_id INT, @c_w_id INT, @c_d_id INT, @c_id INT, @c_last VARCHAR(16), @amount DECIMAL(6,2) AS
BEGIN
  SET NOCOUNT ON;
  BEGIN TRAN;
  UPDATE warehouse SET w_ytd = w_ytd + @amount WHERE w_id = @w_id;
  UPDATE district SET d_ytd = d_ytd + @amount WHERE d_w_id = @w_id AND d_id = @d_id;
  IF @c_id = 0
    SELECT @c_id = c_id FROM (SELECT c_id, ROW_NUMBER() OVER (ORDER BY c_first, c_id) rn, COUNT(*) OVER () cnt FROM customer WHERE c_w_id = @c_w_id AND c_d_id = @c_d_id AND c_last = @c_last) x WHERE rn = (cnt + 1) / 2;
  UPDATE customer SET c_balance = c_balance - @amount, c_ytd_payment = c_ytd_payment + @amount, c_payment_cnt = c_payment_cnt + 1 WHERE c_w_id = @c_w_id AND c_d_id = @c_d_id AND c_id = @c_id;
  INSERT history (h_c_id, h_c_d_id, h_c_w_id, h_d_id, h_w_id, h_date, h_amount, h_data) VALUES (@c_id, @c_d_id, @c_w_id, @d_id, @w_id, GETDATE(), @amount, 'pay');
  COMMIT TRAN;
  SELECT @c_id AS c_id;
END",
    "CREATE PROCEDURE ordstat @w_id INT, @d_id INT, @c_id INT, @c_last VARCHAR(16) AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @o_id INT;
  IF @c_id = 0
    SELECT @c_id = c_id FROM (SELECT c_id, ROW_NUMBER() OVER (ORDER BY c_first, c_id) rn, COUNT(*) OVER () cnt FROM customer WHERE c_w_id = @w_id AND c_d_id = @d_id AND c_last = @c_last) x WHERE rn = (cnt + 1) / 2;
  SELECT c_balance, c_first, c_middle, c_last FROM customer WHERE c_w_id = @w_id AND c_d_id = @d_id AND c_id = @c_id;
  SELECT @o_id = MAX(o_id) FROM orders WHERE o_w_id = @w_id AND o_d_id = @d_id AND o_c_id = @c_id;
  SELECT o_id, o_entry_d, o_carrier_id FROM orders WHERE o_w_id = @w_id AND o_d_id = @d_id AND o_id = @o_id;
  SELECT ol_i_id, ol_supply_w_id, ol_quantity, ol_amount, ol_delivery_d FROM order_line WHERE ol_w_id = @w_id AND ol_d_id = @d_id AND ol_o_id = @o_id;
END",
    "CREATE PROCEDURE delivery @w_id INT, @carrier INT AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @d INT, @o_id INT, @c_id INT, @amt DECIMAL(12,2), @done INT;
  SET @d = 1; SET @done = 0;
  BEGIN TRAN;
  WHILE @d <= 10
  BEGIN
    SET @o_id = NULL;
    SELECT @o_id = MIN(no_o_id) FROM new_order WITH (UPDLOCK) WHERE no_w_id = @w_id AND no_d_id = @d;
    IF @o_id IS NOT NULL
    BEGIN
      DELETE FROM new_order WHERE no_w_id = @w_id AND no_d_id = @d AND no_o_id = @o_id;
      SELECT @c_id = o_c_id FROM orders WHERE o_w_id = @w_id AND o_d_id = @d AND o_id = @o_id;
      UPDATE orders SET o_carrier_id = @carrier WHERE o_w_id = @w_id AND o_d_id = @d AND o_id = @o_id;
      UPDATE order_line SET ol_delivery_d = GETDATE() WHERE ol_w_id = @w_id AND ol_d_id = @d AND ol_o_id = @o_id;
      SELECT @amt = SUM(ol_amount) FROM order_line WHERE ol_w_id = @w_id AND ol_d_id = @d AND ol_o_id = @o_id;
      UPDATE customer SET c_balance = c_balance + @amt, c_delivery_cnt = c_delivery_cnt + 1 WHERE c_w_id = @w_id AND c_d_id = @d AND c_id = @c_id;
      SET @done = @done + 1;
    END
    SET @d = @d + 1;
  END
  COMMIT TRAN;
  SELECT @done AS delivered;
END",
    "CREATE PROCEDURE slev @w_id INT, @d_id INT, @threshold INT AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @next INT;
  SELECT @next = d_next_o_id FROM district WHERE d_w_id = @w_id AND d_id = @d_id;
  SELECT COUNT(DISTINCT s_i_id) AS low_stock FROM order_line, stock WHERE ol_w_id = @w_id AND ol_d_id = @d_id AND ol_o_id < @next AND ol_o_id >= @next - 20 AND s_w_id = @w_id AND s_i_id = ol_i_id AND s_quantity < @threshold;
END",
];

const SYLLABLES: [&str; 10] = ["BAR", "OUGHT", "ABLE", "PRI", "PRES", "ESE", "ANTI", "CALLY", "ATION", "EING"];

pub fn last_name(n: i64) -> String {
    format!("{}{}{}", SYLLABLES[((n / 100) % 10) as usize], SYLLABLES[((n / 10) % 10) as usize], SYLLABLES[(n % 10) as usize])
}

pub fn load(c: &mut Connection, cfg: &Cfg) {
    must(c, SCHEMA);
    for p in PROCS {
        must(c, p);
    }
    let mut rng = Rng::new(1_234_567);
    insert_rows(c, "item", &(1..=cfg.items).map(|i| format!("({}, 'item {}', {}.{:02}, '{}')", i, i, rng.int(1, 100), rng.int(0, 99), rng.text(3))).collect::<Vec<_>>());
    let n = cfg.customers;
    for w in 1..=cfg.warehouses {
        insert_rows(c, "warehouse", &[format!("({}, 'wh{}', 0.{:04}, {}.00)", w, w, rng.int(0, 2000), 10 * n * 10)]);
        insert_rows(c, "stock", &(1..=cfg.items).map(|i| format!("({}, {}, {}, 0, 0, 0, '{}')", w, i, rng.int(10, 100), rng.text(2))).collect::<Vec<_>>());
        let mut dist = vec![];
        let (mut cust, mut hist, mut ord, mut nord, mut line) = (vec![], vec![], vec![], vec![], vec![]);
        for d in 1..=10 {
            dist.push(format!("({}, {}, 'dist{}', 0.{:04}, {}.00, {})", w, d, d, rng.int(0, 2000), n * 10, n + 1));
            for ci in 1..=n {
                let name = last_name((ci - 1) % (n / 3).max(1));
                cust.push(format!("({}, {}, {}, 'first{}', 'OE', '{}', '{}', 0.{:04}, -10.00, 10.00, 1, 0, '{}')", w, d, ci, rng.int(1, 9999), name, if rng.chance(0.1) { "BC" } else { "GC" }, rng.int(0, 5000), rng.text(3)));
                hist.push(format!("({}, {}, {}, {}, {}, GETDATE(), 10.00, 'init')", ci, d, w, d, w));
            }
            // one order per customer (in a random order of customers); the last 30% are not delivered yet
            let mut perm: Vec<i64> = (1..=n).collect();
            for i in (1..perm.len()).rev() {
                perm.swap(i, rng.int(0, i as i64) as usize);
            }
            let delivered = n * 7 / 10;
            for o in 1..=n {
                let cnt = rng.int(5, 15);
                let is_new = o > delivered;
                ord.push(format!("({}, {}, {}, {}, GETDATE(), {}, {}, 1)", w, d, o, perm[(o - 1) as usize], if is_new { "NULL".to_string() } else { rng.int(1, 10).to_string() }, cnt));
                if is_new {
                    nord.push(format!("({}, {}, {})", w, d, o));
                }
                for l in 1..=cnt {
                    line.push(format!("({}, {}, {}, {}, {}, {}, {}, 5, {}, 'dist')", w, d, o, l, rng.int(1, cfg.items), w, if is_new { "NULL" } else { "GETDATE()" }, if is_new { format!("{}.{:02}", rng.int(1, 9999), rng.int(0, 99)) } else { "0.00".to_string() }));
                }
            }
        }
        insert_rows(c, "district", &dist);
        insert_rows(c, "customer", &cust);
        insert_rows(c, "history", &hist);
        insert_rows(c, "orders", &ord);
        insert_rows(c, "new_order", &nord);
        insert_rows(c, "order_line", &line);
    }
}

/// The consistency conditions of clause 3.3 of the specification (numbers as there), each a query that counts the
/// violations.
pub fn consistency() -> Vec<(&'static str, &'static str)> {
    vec![
        ("1: W_YTD = sum(D_YTD)", "SELECT COUNT(*) FROM warehouse w WHERE w_ytd <> (SELECT SUM(d_ytd) FROM district WHERE d_w_id = w.w_id)"),
        ("2: D_NEXT_O_ID - 1 = max(O_ID) = max(NO_O_ID)", "SELECT COUNT(*) FROM district d WHERE d_next_o_id - 1 <> (SELECT MAX(o_id) FROM orders WHERE o_w_id = d.d_w_id AND o_d_id = d.d_id) OR (EXISTS (SELECT 1 FROM new_order WHERE no_w_id = d.d_w_id AND no_d_id = d.d_id) AND d_next_o_id - 1 <> (SELECT MAX(no_o_id) FROM new_order WHERE no_w_id = d.d_w_id AND no_d_id = d.d_id))"),
        ("3: new orders are contiguous", "SELECT COUNT(*) FROM (SELECT no_w_id, no_d_id FROM new_order GROUP BY no_w_id, no_d_id HAVING MAX(no_o_id) - MIN(no_o_id) + 1 <> COUNT(*)) x"),
        ("4: sum(O_OL_CNT) = number of order lines", "SELECT COUNT(*) FROM district d WHERE (SELECT SUM(o_ol_cnt) FROM orders WHERE o_w_id = d.d_w_id AND o_d_id = d.d_id) <> (SELECT COUNT(*) FROM order_line WHERE ol_w_id = d.d_w_id AND ol_d_id = d.d_id)"),
        ("5: undelivered orders are the new orders", "SELECT COUNT(*) FROM orders o WHERE (o_carrier_id IS NULL AND NOT EXISTS (SELECT 1 FROM new_order n WHERE n.no_w_id = o.o_w_id AND n.no_d_id = o.o_d_id AND n.no_o_id = o.o_id)) OR (o_carrier_id IS NOT NULL AND EXISTS (SELECT 1 FROM new_order n WHERE n.no_w_id = o.o_w_id AND n.no_d_id = o.o_d_id AND n.no_o_id = o.o_id))"),
        ("6: O_OL_CNT = lines of the order", "SELECT COUNT(*) FROM orders o WHERE o_ol_cnt <> (SELECT COUNT(*) FROM order_line WHERE ol_w_id = o.o_w_id AND ol_d_id = o.o_d_id AND ol_o_id = o.o_id)"),
        ("8: W_YTD = sum(H_AMOUNT)", "SELECT COUNT(*) FROM warehouse w WHERE w_ytd <> (SELECT SUM(h_amount) FROM history WHERE h_w_id = w.w_id)"),
        ("9: D_YTD = sum(H_AMOUNT)", "SELECT COUNT(*) FROM district d WHERE d_ytd <> (SELECT SUM(h_amount) FROM history WHERE h_w_id = d.d_w_id AND h_d_id = d.d_id)"),
        ("10: C_BALANCE = delivered amounts - payments", "SELECT COUNT(*) FROM customer c WHERE c_balance <> ISNULL((SELECT SUM(ol_amount) FROM orders o JOIN order_line l ON l.ol_w_id = o.o_w_id AND l.ol_d_id = o.o_d_id AND l.ol_o_id = o.o_id WHERE o.o_w_id = c.c_w_id AND o.o_d_id = c.c_d_id AND o.o_c_id = c.c_id AND o.o_carrier_id IS NOT NULL), 0) - (SELECT SUM(h_amount) FROM history h WHERE h.h_c_w_id = c.c_w_id AND h.h_c_d_id = c.c_d_id AND h.h_c_id = c.c_id)"),
        ("12: C_BALANCE + C_YTD_PAYMENT = delivered amounts", "SELECT COUNT(*) FROM customer c WHERE c_balance + c_ytd_payment <> ISNULL((SELECT SUM(ol_amount) FROM orders o JOIN order_line l ON l.ol_w_id = o.o_w_id AND l.ol_d_id = o.o_d_id AND l.ol_o_id = o.o_id WHERE o.o_w_id = c.c_w_id AND o.o_d_id = c.c_d_id AND o.o_c_id = c.c_id AND o.o_carrier_id IS NOT NULL), 0)"),
    ]
}

/// One terminal: a connection bound to a home warehouse that runs the transaction mix.
pub struct Terminal {
    conn: Connection,
    rng: Rng,
    cfg: std::sync::Arc<Cfg>,
    home: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    NewOrder,
    Payment,
    OrderStatus,
    Delivery,
    StockLevel,
}

pub const KINDS: [Kind; 5] = [Kind::NewOrder, Kind::Payment, Kind::OrderStatus, Kind::Delivery, Kind::StockLevel];

impl Terminal {
    pub fn new(conn: Connection, seed: u64, cfg: std::sync::Arc<Cfg>, home: i64) -> Terminal {
        Terminal { conn, rng: Rng::new(seed), cfg, home }
    }

    /// The mix of clause 5.2.3: 45% New-Order, 43% Payment, 4% each of the others.
    pub fn pick(&mut self) -> Kind {
        match self.rng.int(1, 100) {
            1..=45 => Kind::NewOrder,
            46..=88 => Kind::Payment,
            89..=92 => Kind::OrderStatus,
            93..=96 => Kind::Delivery,
            _ => Kind::StockLevel,
        }
    }

    fn customer(&mut self) -> i64 {
        // a skewed choice (NURand-like): some customers are hot
        let n = self.cfg.customers;
        ((self.rng.int(0, 1023) | self.rng.int(1, n)) % n) + 1
    }

    fn by_name(&mut self) -> (i64, String) {
        if self.rng.chance(0.6) {
            (0, last_name(self.rng.int(0, (self.cfg.customers / 3).max(1) - 1)))
        } else {
            (self.customer(), String::new())
        }
    }

    /// Run one transaction, retrying when it is chosen as a deadlock victim. Returns (retries, committed).
    pub fn run(&mut self, kind: Kind) -> Result<(u32, bool), String> {
        let w = self.home;
        let d = self.rng.int(1, 10);
        let sql = match kind {
            Kind::NewOrder => {
                let c = self.customer();
                let n = self.rng.int(5, 15);
                let rollback = self.rng.chance(0.01);
                let mut all_local = 1;
                let mut lines = vec![];
                for l in 0..n {
                    let item = if rollback && l == n - 1 { self.cfg.items + 1 } else { self.rng.int(1, self.cfg.items) };
                    let supply = if self.cfg.warehouses > 1 && self.rng.chance(0.01) {
                        all_local = 0;
                        (w % self.cfg.warehouses) + 1
                    } else {
                        w
                    };
                    lines.push(format!("{}:{}:{}", item, supply, self.rng.int(1, 10)));
                }
                format!("EXEC neword {}, {}, {}, '{}', {}", w, d, c, lines.join(";"), all_local)
            }
            Kind::Payment => {
                let (cw, cd) = if self.cfg.warehouses > 1 && self.rng.chance(0.15) { ((w % self.cfg.warehouses) + 1, self.rng.int(1, 10)) } else { (w, d) };
                let (c, name) = self.by_name();
                format!("EXEC payment {}, {}, {}, {}, {}, '{}', {}.{:02}", w, d, cw, cd, c, name, self.rng.int(1, 5000), self.rng.int(0, 99))
            }
            Kind::OrderStatus => {
                let (c, name) = self.by_name();
                format!("EXEC ordstat {}, {}, {}, '{}'", w, d, c, name)
            }
            Kind::Delivery => format!("EXEC delivery {}, {}", w, self.rng.int(1, 10)),
            Kind::StockLevel => format!("EXEC slev {}, {}, {}", w, d, self.rng.int(10, 20)),
        };
        let mut retries = 0;
        loop {
            let r = self.conn.execute(&sql);
            match r.first_error() {
                None => {
                    // New-Order returns 1 (rolled back on purpose) for the invalid item; that is a success of the test
                    return Ok((retries, true));
                }
                Some(e) if e.number == 1205 && retries < 20 => {
                    retries += 1;
                }
                Some(e) => return Err(format!("{:?}: {} [{}]", kind, e, sql.chars().take(120).collect::<String>())),
            }
        }
    }
}

pub struct Tally {
    pub done: [u64; 5],
    pub failed: [u64; 5],
    pub retries: u64,
    pub lat: [Vec<f64>; 5],
    pub errors: Vec<String>,
}

impl Tally {
    pub fn new() -> Tally {
        Tally { done: [0; 5], failed: [0; 5], retries: 0, lat: Default::default(), errors: vec![] }
    }
    pub fn merge(&mut self, o: Tally) {
        for i in 0..5 {
            self.done[i] += o.done[i];
            self.failed[i] += o.failed[i];
            self.lat[i].extend(o.lat[i].iter());
        }
        self.retries += o.retries;
        for e in o.errors {
            if self.errors.len() < 10 {
                self.errors.push(e);
            }
        }
    }
}

/// Run the mix on every terminal for `secs` seconds.
pub fn drive(server: &tsql_server::Server, cfg: std::sync::Arc<Cfg>, terminals: usize, secs: f64) -> Tally {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut hs = vec![];
    for t in 0..terminals {
        let mut term = Terminal::new(server.connect(), 77 + t as u64, cfg.clone(), (t as i64 % cfg.warehouses) + 1);
        let stop = stop.clone();
        hs.push(std::thread::spawn(move || {
            let mut tally = Tally::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let kind = term.pick();
                let i = KINDS.iter().position(|k| *k == kind).unwrap();
                let t0 = Instant::now();
                match term.run(kind) {
                    Ok((retries, _)) => {
                        tally.done[i] += 1;
                        tally.retries += retries as u64;
                        tally.lat[i].push(t0.elapsed().as_secs_f64());
                    }
                    Err(e) => {
                        tally.failed[i] += 1;
                        if tally.errors.len() < 10 {
                            tally.errors.push(e);
                        }
                    }
                }
            }
            tally
        }));
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut all = Tally::new();
    for h in hs {
        all.merge(h.join().unwrap());
    }
    all
}
