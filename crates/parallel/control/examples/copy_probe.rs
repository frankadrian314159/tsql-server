//! Check 10 of TPC-C against a copy of order_line made with SELECT INTO (a heap), with and without an index on the copy.
use std::time::Instant;
use tsql_server::Server;

fn run(c: &mut tsql_server::Connection, sql: &str) -> String {
    let r = c.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
    r.resultsets().first().and_then(|rs| rs.rows.first()).map(|row| row[0].to_string()).unwrap_or_default()
}

fn main() {
    let s = Server::new();
    let mut c = s.connect();
    for ddl in [
        "CREATE TABLE customer (c_w_id INT, c_d_id INT, c_id INT, c_balance DECIMAL(12,2), PRIMARY KEY (c_w_id, c_d_id, c_id))",
        "CREATE TABLE orders (o_w_id INT, o_d_id INT, o_id INT, o_c_id INT, o_carrier_id INT NULL, PRIMARY KEY (o_w_id, o_d_id, o_id))",
        "CREATE TABLE order_line (ol_w_id INT, ol_d_id INT, ol_o_id INT, ol_number INT, ol_amount DECIMAL(8,2), PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number))",
    ] {
        run(&mut c, ddl);
    }
    // 2 warehouses x 10 districts x 300 customers; one order per customer, 10 lines each
    let mut cust = vec![];
    let mut ord = vec![];
    let mut ol = vec![];
    for w in 1..=2 {
        for d in 1..=10 {
            for i in 1..=300 {
                cust.push(format!("({},{},{},0)", w, d, i));
                ord.push(format!("({},{},{},{},{})", w, d, i, i, if i % 3 == 0 { "NULL" } else { "1" }));
                for n in 1..=10 {
                    ol.push(format!("({},{},{},{},1.5)", w, d, i, n));
                }
            }
        }
    }
    for (t, rows) in [("customer", &cust), ("orders", &ord), ("order_line", &ol)] {
        for ch in rows.chunks(500) {
            run(&mut c, &format!("INSERT {} VALUES {}", t, ch.join(",")));
        }
    }
    let q = "SELECT COUNT(*) FROM customer c WHERE c_id <= 2 AND c_balance <> ISNULL((SELECT SUM(ol_amount) FROM orders o JOIN {} l ON l.ol_w_id = o.o_w_id AND l.ol_d_id = o.o_d_id AND l.ol_o_id = o.o_id WHERE o.o_w_id = c.c_w_id AND o.o_d_id = c.c_d_id AND o.o_c_id = c.c_id AND o.o_carrier_id IS NOT NULL), 0)";
    let t = Instant::now();
    println!("original table: {} in {:.2}s", run(&mut c, &q.replace("{}", "order_line")), t.elapsed().as_secs_f64());
    run(&mut c, "SELECT ol_w_id, ol_d_id, ol_o_id, ol_amount INTO #cp FROM order_line");
    let t = Instant::now();
    println!("temp heap copy, no index: {} in {:.2}s", run(&mut c, &q.replace("{}", "#cp")), t.elapsed().as_secs_f64());
    run(&mut c, "CREATE INDEX ix_cp ON #cp (ol_o_id, ol_d_id, ol_w_id)");
    let t = Instant::now();
    println!("temp heap copy, indexed: {} in {:.2}s", run(&mut c, &q.replace("{}", "#cp")), t.elapsed().as_secs_f64());
    run(&mut c, "SELECT ol_w_id, ol_d_id, ol_o_id, ol_amount INTO rc FROM order_line");
    run(&mut c, "CREATE INDEX ix_rc ON rc (ol_o_id, ol_d_id, ol_w_id)");
    let t = Instant::now();
    println!("real table copy, indexed: {} in {:.2}s", run(&mut c, &q.replace("{}", "rc")), t.elapsed().as_secs_f64());
    run(&mut c, "CREATE INDEX ix_rc2 ON rc (ol_w_id, ol_d_id, ol_o_id)");
    let t = Instant::now();
    println!("real table copy, indexed w/d/o: {} in {:.2}s", run(&mut c, &q.replace("{}", "rc")), t.elapsed().as_secs_f64());
}
