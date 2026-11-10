//! A reduced TPC-E: the brokerage-house schema (customers, accounts, brokers, securities, trades, holdings, watch
//! lists), nine of its transactions, the benchmark's read-heavy mix, and invariants that tie the tables together.
//!
//! This is not the full benchmark: the real schema has 33 tables, the transactions have many more steps and frames,
//! and the driver emulates a market exchange and customers with think times. What is kept is the shape of the
//! workload: mostly reads with joins, aggregates and top-N queries, plus Trade-Order / Trade-Result writes that must
//! leave holdings, account balances, broker totals and trade history consistent under concurrency.
use crate::util::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tsql_server::Connection;

pub struct Cfg {
    pub customers: i64,
    pub securities: i64,
    pub accounts_per_customer: i64,
}

impl Cfg {
    fn accounts(&self) -> i64 {
        self.customers * self.accounts_per_customer
    }
    fn brokers(&self) -> i64 {
        (self.customers / 20).max(5)
    }
}

pub const NAMES: [&str; 9] = ["Broker-Volume", "Customer-Position", "Market-Feed", "Market-Watch", "Security-Detail", "Trade-Lookup", "Trade-Order", "Trade-Result", "Trade-Status"];
pub const TRADE_RESULT: usize = 7;
const BROKER_VOLUME: usize = 0;
const CUSTOMER_POSITION: usize = 1;
const MARKET_FEED: usize = 2;
const MARKET_WATCH: usize = 3;
const SECURITY_DETAIL: usize = 4;
const TRADE_LOOKUP: usize = 5;
const TRADE_ORDER: usize = 6;
const TRADE_STATUS: usize = 8;

const SCHEMA: &str = "
CREATE TABLE customer (c_id INT PRIMARY KEY, c_name VARCHAR(30), c_tier INT);
CREATE TABLE broker (b_id INT PRIMARY KEY, b_name VARCHAR(30), b_num_trades INT, b_comm_total DECIMAL(14,2));
CREATE TABLE customer_account (ca_id INT PRIMARY KEY, ca_c_id INT, ca_b_id INT, ca_bal DECIMAL(14,2));
CREATE INDEX ix_ca_c ON customer_account (ca_c_id);
CREATE TABLE security (s_symb VARCHAR(8) PRIMARY KEY, s_name VARCHAR(30), s_ex VARCHAR(4));
CREATE TABLE last_trade (lt_s_symb VARCHAR(8) PRIMARY KEY, lt_price DECIMAL(10,2), lt_open_price DECIMAL(10,2), lt_vol BIGINT);
CREATE TABLE trade (t_id BIGINT PRIMARY KEY, t_dts DATETIME, t_st_id CHAR(4), t_tt_id CHAR(3), t_s_symb VARCHAR(8), t_qty INT, t_bid_price DECIMAL(10,2), t_ca_id INT, t_trade_price DECIMAL(10,2) NULL, t_chrg DECIMAL(10,2), t_comm DECIMAL(10,2));
CREATE INDEX ix_t_ca ON trade (t_ca_id, t_dts);
CREATE INDEX ix_t_s ON trade (t_s_symb);
CREATE TABLE trade_history (th_t_id BIGINT, th_dts DATETIME, th_st_id CHAR(4));
CREATE INDEX ix_th ON trade_history (th_t_id);
CREATE TABLE holding (h_t_id BIGINT PRIMARY KEY, h_ca_id INT, h_s_symb VARCHAR(8), h_qty INT, h_price DECIMAL(10,2));
CREATE INDEX ix_h ON holding (h_ca_id, h_s_symb);
CREATE TABLE holding_summary (hs_ca_id INT, hs_s_symb VARCHAR(8), hs_qty INT, PRIMARY KEY (hs_ca_id, hs_s_symb));
CREATE TABLE watch_list (wl_id INT PRIMARY KEY, wl_c_id INT);
CREATE TABLE watch_item (wi_wl_id INT, wi_s_symb VARCHAR(8), PRIMARY KEY (wi_wl_id, wi_s_symb));
CREATE SEQUENCE trade_seq AS BIGINT START WITH 10000000 INCREMENT BY 1;
";

const PROCS: [&str; 9] = [
    "CREATE PROCEDURE trade_order @ca_id INT, @symb VARCHAR(8), @qty INT, @is_buy BIT AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @price DECIMAL(10,2), @t_id BIGINT;
  SELECT @price = lt_price FROM last_trade WHERE lt_s_symb = @symb;
  SET @t_id = NEXT VALUE FOR trade_seq;
  BEGIN TRAN;
  INSERT trade (t_id, t_dts, t_st_id, t_tt_id, t_s_symb, t_qty, t_bid_price, t_ca_id, t_trade_price, t_chrg, t_comm) VALUES (@t_id, GETDATE(), 'PNDG', CASE WHEN @is_buy = 1 THEN 'TMB' ELSE 'TMS' END, @symb, @qty, @price, @ca_id, NULL, 1.00, 0);
  INSERT trade_history (th_t_id, th_dts, th_st_id) VALUES (@t_id, GETDATE(), 'PNDG');
  COMMIT TRAN;
  SELECT @t_id AS t_id;
END",
    "CREATE PROCEDURE trade_result @t_id BIGINT, @price DECIMAL(10,2) AS
BEGIN
  SET NOCOUNT ON;
  DECLARE @ca_id INT, @symb VARCHAR(8), @qty INT, @tt CHAR(3), @st CHAR(4), @amt DECIMAL(14,2), @comm DECIMAL(10,2), @b_id INT, @left INT, @h_id BIGINT, @h_qty INT, @have INT, @sign INT;
  BEGIN TRAN;
  SET @st = NULL;
  SELECT @st = t_st_id, @ca_id = t_ca_id, @symb = t_s_symb, @qty = t_qty, @tt = t_tt_id FROM trade WITH (UPDLOCK) WHERE t_id = @t_id;
  IF @st IS NULL OR @st <> 'PNDG' BEGIN ROLLBACK TRAN; SELECT 0 AS completed; RETURN; END
  SET @amt = @qty * @price;
  SET @comm = @amt * 0.001;
  IF @tt = 'TMS'
  BEGIN
    SELECT @have = ISNULL(SUM(h_qty), 0) FROM holding WITH (UPDLOCK) WHERE h_ca_id = @ca_id AND h_s_symb = @symb;
    IF @have < @qty
    BEGIN
      UPDATE trade SET t_st_id = 'CNCL' WHERE t_id = @t_id;
      INSERT trade_history (th_t_id, th_dts, th_st_id) VALUES (@t_id, GETDATE(), 'CNCL');
      COMMIT TRAN;
      SELECT 0 AS completed;
      RETURN;
    END
    SET @left = @qty;
    WHILE @left > 0
    BEGIN
      SELECT TOP 1 @h_id = h_t_id, @h_qty = h_qty FROM holding WITH (UPDLOCK) WHERE h_ca_id = @ca_id AND h_s_symb = @symb ORDER BY h_t_id;
      IF @h_qty <= @left BEGIN DELETE FROM holding WHERE h_t_id = @h_id; SET @left = @left - @h_qty; END
      ELSE BEGIN UPDATE holding SET h_qty = h_qty - @left WHERE h_t_id = @h_id; SET @left = 0; END
    END
    UPDATE holding_summary SET hs_qty = hs_qty - @qty WHERE hs_ca_id = @ca_id AND hs_s_symb = @symb;
    SET @sign = 1;
  END
  ELSE
  BEGIN
    INSERT holding (h_t_id, h_ca_id, h_s_symb, h_qty, h_price) VALUES (@t_id, @ca_id, @symb, @qty, @price);
    UPDATE holding_summary SET hs_qty = hs_qty + @qty WHERE hs_ca_id = @ca_id AND hs_s_symb = @symb;
    IF @@ROWCOUNT = 0 INSERT holding_summary (hs_ca_id, hs_s_symb, hs_qty) VALUES (@ca_id, @symb, @qty);
    SET @sign = -1;
  END
  UPDATE trade SET t_st_id = 'CMPT', t_trade_price = @price, t_comm = @comm WHERE t_id = @t_id;
  INSERT trade_history (th_t_id, th_dts, th_st_id) VALUES (@t_id, GETDATE(), 'CMPT');
  UPDATE customer_account SET ca_bal = ca_bal + @sign * @amt - @comm WHERE ca_id = @ca_id;
  SELECT @b_id = ca_b_id FROM customer_account WHERE ca_id = @ca_id;
  UPDATE broker SET b_num_trades = b_num_trades + 1, b_comm_total = b_comm_total + @comm WHERE b_id = @b_id;
  COMMIT TRAN;
  SELECT 1 AS completed;
END",
    "CREATE PROCEDURE market_feed @symb VARCHAR(8), @price DECIMAL(10,2) AS
BEGIN
  SET NOCOUNT ON;
  UPDATE last_trade SET lt_price = @price, lt_vol = lt_vol + 100 WHERE lt_s_symb = @symb;
END",
    "CREATE PROCEDURE trade_lookup @ca_id INT AS
BEGIN
  SET NOCOUNT ON;
  SELECT TOP 20 t.t_id, t.t_s_symb, t.t_qty, t.t_trade_price, t.t_st_id, h.th_st_id, h.th_dts FROM trade t JOIN trade_history h ON h.th_t_id = t.t_id WHERE t.t_ca_id = @ca_id ORDER BY t.t_dts DESC, t.t_id DESC, h.th_dts;
END",
    "CREATE PROCEDURE trade_status @ca_id INT AS
BEGIN
  SET NOCOUNT ON;
  SELECT TOP 50 t.t_id, t.t_dts, t.t_st_id, t.t_tt_id, t.t_s_symb, t.t_qty, s.s_name FROM trade t JOIN security s ON s.s_symb = t.t_s_symb WHERE t.t_ca_id = @ca_id ORDER BY t.t_dts DESC, t.t_id DESC;
END",
    "CREATE PROCEDURE customer_position @c_id INT AS
BEGIN
  SET NOCOUNT ON;
  SELECT a.ca_id, a.ca_bal, ISNULL(SUM(hs.hs_qty * lt.lt_price), 0) AS assets FROM customer_account a LEFT JOIN holding_summary hs ON hs.hs_ca_id = a.ca_id LEFT JOIN last_trade lt ON lt.lt_s_symb = hs.hs_s_symb WHERE a.ca_c_id = @c_id GROUP BY a.ca_id, a.ca_bal;
END",
    "CREATE PROCEDURE market_watch @wl_id INT AS
BEGIN
  SET NOCOUNT ON;
  SELECT 100.0 * SUM(lt.lt_price) / NULLIF(SUM(lt.lt_open_price), 0) - 100.0 AS pct_change FROM watch_item w JOIN last_trade lt ON lt.lt_s_symb = w.wi_s_symb WHERE w.wi_wl_id = @wl_id;
END",
    "CREATE PROCEDURE security_detail @symb VARCHAR(8) AS
BEGIN
  SET NOCOUNT ON;
  SELECT s.s_name, s.s_ex, lt.lt_price, lt.lt_open_price, lt.lt_vol FROM security s JOIN last_trade lt ON lt.lt_s_symb = s.s_symb WHERE s.s_symb = @symb;
  SELECT TOP 5 t_id, t_dts, t_qty, t_trade_price FROM trade WHERE t_s_symb = @symb AND t_st_id = 'CMPT' ORDER BY t_dts DESC, t_id DESC;
END",
    "CREATE PROCEDURE broker_volume @min_qty INT AS
BEGIN
  SET NOCOUNT ON;
  SELECT b.b_name, SUM(t.t_qty * t.t_bid_price) AS volume FROM trade t JOIN customer_account a ON a.ca_id = t.t_ca_id JOIN broker b ON b.b_id = a.ca_b_id WHERE t.t_st_id = 'PNDG' AND t.t_qty >= @min_qty GROUP BY b.b_name ORDER BY volume DESC, b.b_name;
END",
];

fn symb(i: i64) -> String {
    format!("S{:04}", i)
}

pub fn load(c: &mut Connection, cfg: &Cfg) {
    must(c, SCHEMA);
    for p in PROCS {
        must(c, p);
    }
    let mut rng = Rng::new(424_242);
    insert_rows(c, "customer", &(1..=cfg.customers).map(|i| format!("({}, 'customer {}', {})", i, i, rng.int(1, 3))).collect::<Vec<_>>());
    insert_rows(c, "security", &(1..=cfg.securities).map(|i| format!("('{}', 'security {}', '{}')", symb(i), i, if i % 2 == 0 { "NYSE" } else { "NASD" })).collect::<Vec<_>>());
    let prices: Vec<i64> = (0..cfg.securities).map(|_| rng.int(1000, 10_000)).collect(); // cents
    insert_rows(c, "last_trade", &(1..=cfg.securities).map(|i| format!("('{}', {:.2}, {:.2}, 0)", symb(i), prices[(i - 1) as usize] as f64 / 100.0, prices[(i - 1) as usize] as f64 / 100.0)).collect::<Vec<_>>());

    // accounts, each with a few initial completed purchases (so sells can happen); balances and broker totals follow
    let n_acc = cfg.accounts();
    let n_brk = cfg.brokers();
    let mut bal = vec![10_000_000i64; (n_acc + 1) as usize]; // cents
    let mut broker_trades = vec![0i64; (n_brk + 1) as usize];
    let mut broker_comm = vec![0i64; (n_brk + 1) as usize];
    let (mut trades, mut hist, mut holds, mut summ) = (vec![], vec![], vec![], vec![]);
    let mut tid = 1i64;
    for a in 1..=n_acc {
        let b = (a % n_brk) + 1;
        for k in 0..3 {
            let s = (a * 7 + k * 13) % cfg.securities + 1;
            let qty = rng.int(100, 500);
            let price = prices[(s - 1) as usize];
            let amt = qty * price;
            let comm = amt / 1000; // 0.1%, in cents (matches the procedure's rounding for these magnitudes)
            let comm = comm.max(0);
            bal[a as usize] -= amt + comm;
            broker_trades[b as usize] += 1;
            broker_comm[b as usize] += comm;
            trades.push(format!("({}, GETDATE(), 'CMPT', 'TMB', '{}', {}, {:.2}, {}, {:.2}, 1.00, {:.2})", tid, symb(s), qty, price as f64 / 100.0, a, price as f64 / 100.0, comm as f64 / 100.0));
            hist.push(format!("({}, GETDATE(), 'PNDG')", tid));
            hist.push(format!("({}, GETDATE(), 'CMPT')", tid));
            holds.push(format!("({}, {}, '{}', {}, {:.2})", tid, a, symb(s), qty, price as f64 / 100.0));
            summ.push((a, s, qty));
            tid += 1;
        }
    }
    // merge the summary rows of the same (account, security)
    let mut merged: std::collections::BTreeMap<(i64, i64), i64> = Default::default();
    for (a, s, q) in summ {
        *merged.entry((a, s)).or_default() += q;
    }
    insert_rows(c, "broker", &(1..=n_brk).map(|b| format!("({}, 'broker {}', {}, {:.2})", b, b, broker_trades[b as usize], broker_comm[b as usize] as f64 / 100.0)).collect::<Vec<_>>());
    insert_rows(c, "customer_account", &(1..=n_acc).map(|a| format!("({}, {}, {}, {:.2})", a, (a - 1) / cfg.accounts_per_customer + 1, (a % n_brk) + 1, bal[a as usize] as f64 / 100.0)).collect::<Vec<_>>());
    insert_rows(c, "trade", &trades);
    insert_rows(c, "trade_history", &hist);
    insert_rows(c, "holding", &holds);
    insert_rows(c, "holding_summary", &merged.iter().map(|((a, s), q)| format!("({}, '{}', {})", a, symb(*s), q)).collect::<Vec<_>>());
    insert_rows(c, "watch_list", &(1..=cfg.customers).map(|i| format!("({}, {})", i, i)).collect::<Vec<_>>());
    let mut items = vec![];
    for w in 1..=cfg.customers {
        let mut seen = vec![];
        for _ in 0..rng.int(5, 15) {
            let s = rng.int(1, cfg.securities);
            if !seen.contains(&s) {
                seen.push(s);
                items.push(format!("({}, '{}')", w, symb(s)));
            }
        }
    }
    insert_rows(c, "watch_item", &items);
}

/// Invariants tying the tables together, each a query that counts violations.
pub fn consistency() -> Vec<(&'static str, &'static str)> {
    vec![
        ("holding summary = sum of holdings", "SELECT COUNT(*) FROM holding_summary hs WHERE hs_qty <> ISNULL((SELECT SUM(h_qty) FROM holding h WHERE h.h_ca_id = hs.hs_ca_id AND h.h_s_symb = hs.hs_s_symb), 0)"),
        ("every holding has a summary", "SELECT COUNT(*) FROM holding h WHERE NOT EXISTS (SELECT 1 FROM holding_summary hs WHERE hs.hs_ca_id = h.h_ca_id AND hs.hs_s_symb = h.h_s_symb)"),
        ("no negative holdings", "SELECT COUNT(*) FROM (SELECT h_qty q FROM holding UNION ALL SELECT hs_qty FROM holding_summary) x WHERE q < 0 OR q IS NULL"),
        ("broker trade counts = completed trades", "SELECT COUNT(*) FROM broker b WHERE b_num_trades <> (SELECT COUNT(*) FROM trade t JOIN customer_account a ON a.ca_id = t.t_ca_id WHERE a.ca_b_id = b.b_id AND t.t_st_id = 'CMPT')"),
        ("broker commission = sum of trade commissions", "SELECT COUNT(*) FROM broker b WHERE b_comm_total <> ISNULL((SELECT SUM(t_comm) FROM trade t JOIN customer_account a ON a.ca_id = t.t_ca_id WHERE a.ca_b_id = b.b_id AND t.t_st_id = 'CMPT'), 0)"),
        ("account balance = 100000 + sells - buys - commissions", "SELECT COUNT(*) FROM customer_account a WHERE ca_bal <> 100000 - ISNULL((SELECT SUM(CASE WHEN t_tt_id = 'TMB' THEN t_qty * t_trade_price ELSE -t_qty * t_trade_price END + t_comm) FROM trade t WHERE t.t_ca_id = a.ca_id AND t.t_st_id = 'CMPT'), 0)"),
        ("every trade has its pending history row", "SELECT COUNT(*) FROM trade t WHERE (SELECT COUNT(*) FROM trade_history h WHERE h.th_t_id = t.t_id AND h.th_st_id = 'PNDG') <> 1"),
        ("completed and cancelled trades have exactly one final history row", "SELECT COUNT(*) FROM trade t WHERE t.t_st_id <> 'PNDG' AND (SELECT COUNT(*) FROM trade_history h WHERE h.th_t_id = t.t_id AND h.th_st_id = t.t_st_id) <> 1"),
        ("holdings come from completed buys", "SELECT COUNT(*) FROM holding h WHERE NOT EXISTS (SELECT 1 FROM trade t WHERE t.t_id = h.h_t_id AND t.t_st_id = 'CMPT' AND t.t_tt_id = 'TMB')"),
    ]
}

pub struct Tally {
    pub done: [u64; 9],
    pub failed: [u64; 9],
    pub retries: u64,
    pub lat: [Vec<f64>; 9],
    pub errors: Vec<String>,
}

impl Tally {
    fn new() -> Tally {
        Tally { done: [0; 9], failed: [0; 9], retries: 0, lat: Default::default(), errors: vec![] }
    }
}

/// Run the mix on every terminal for `secs` seconds. Trade-Order queues the trades it creates for Trade-Result,
/// which stands in for the market exchange emulator completing them.
pub fn drive(server: &tsql_server::Server, cfg: Arc<Cfg>, terminals: usize, secs: f64) -> Tally {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let queue: Arc<Mutex<VecDeque<(i64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut hs = vec![];
    for t in 0..terminals {
        let mut conn = server.connect();
        let (stop, queue, cfg) = (stop.clone(), queue.clone(), cfg.clone());
        hs.push(std::thread::spawn(move || {
            let mut rng = Rng::new(900 + t as u64);
            let mut tally = Tally::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // the mix of the specification, with Trade-Update's 2% given to Trade-Status
                let mut kind = match rng.int(1, 100) {
                    1..=5 => BROKER_VOLUME,
                    6..=18 => CUSTOMER_POSITION,
                    19 => MARKET_FEED,
                    20..=37 => MARKET_WATCH,
                    38..=51 => SECURITY_DETAIL,
                    52..=59 => TRADE_LOOKUP,
                    60..=69 => TRADE_ORDER,
                    70..=79 => TRADE_RESULT,
                    _ => TRADE_STATUS,
                };
                let mut completes: Option<(i64, i64)> = None;
                if kind == TRADE_RESULT {
                    completes = queue.lock().unwrap().pop_front();
                    if completes.is_none() {
                        kind = TRADE_ORDER;
                    }
                }
                let acct = rng.int(1, cfg.accounts());
                let s = symb(rng.int(1, cfg.securities));
                let sql = match kind {
                    BROKER_VOLUME => format!("EXEC broker_volume {}", rng.int(1, 300)),
                    CUSTOMER_POSITION => format!("EXEC customer_position {}", rng.int(1, cfg.customers)),
                    MARKET_FEED => format!("EXEC market_feed '{}', {}.{:02}", s, rng.int(10, 99), rng.int(0, 99)),
                    MARKET_WATCH => format!("EXEC market_watch {}", rng.int(1, cfg.customers)),
                    SECURITY_DETAIL => format!("EXEC security_detail '{}'", s),
                    TRADE_LOOKUP => format!("EXEC trade_lookup {}", acct),
                    TRADE_ORDER => format!("EXEC trade_order {}, '{}', {}, {}", acct, s, rng.int(1, 200), rng.int(0, 1)),
                    TRADE_RESULT => {
                        let (id, _) = completes.unwrap();
                        format!("EXEC trade_result {}, {}.{:02}", id, rng.int(10, 99), rng.int(0, 99))
                    }
                    _ => format!("EXEC trade_status {}", acct),
                };
                let t0 = Instant::now();
                let mut retries = 0;
                loop {
                    let r = conn.execute(&sql);
                    match r.first_error() {
                        None => {
                            if kind == TRADE_ORDER {
                                if let Some(rs) = r.resultsets().first() {
                                    if let Some(id) = rs.rows.first().and_then(|row| row[0].to_string().parse::<i64>().ok()) {
                                        queue.lock().unwrap().push_back((id, acct));
                                    }
                                }
                            }
                            tally.done[kind] += 1;
                            tally.retries += retries;
                            tally.lat[kind].push(t0.elapsed().as_secs_f64());
                            break;
                        }
                        Some(e) if e.number == 1205 && retries < 20 => {
                            retries += 1;
                        }
                        Some(e) => {
                            tally.failed[kind] += 1;
                            if tally.errors.len() < 10 {
                                tally.errors.push(format!("{}: {} [{}]", NAMES[kind], e, sql.chars().take(100).collect::<String>()));
                            }
                            // a trade that could not complete goes back so it is not lost to the invariants
                            if kind == TRADE_RESULT {
                                if let Some(c) = completes {
                                    queue.lock().unwrap().push_back(c);
                                }
                            }
                            break;
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
        let t = h.join().unwrap();
        for i in 0..9 {
            all.done[i] += t.done[i];
            all.failed[i] += t.failed[i];
            all.lat[i].extend(t.lat[i].iter());
        }
        all.retries += t.retries;
        for e in t.errors {
            if all.errors.len() < 10 {
                all.errors.push(e);
            }
        }
    }
    all
}
