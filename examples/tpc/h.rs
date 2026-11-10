//! TPC-H: schema, a data generator in the spirit of `dbgen`, the 22 queries in T-SQL, and checks of several answers
//! against values computed independently from the generated rows.
//!
//! The generator follows the specification's cardinalities, key relationships and value domains, but it is not
//! `dbgen`: its random streams differ, so the answers are not the published validation answers. Correctness is
//! therefore checked by recomputing selected queries in Rust from the same rows.
use crate::util::*;
use chrono::{Duration, NaiveDate};
use std::collections::HashMap;
use tsql_server::Connection;

const SCHEMA: &str = "
CREATE TABLE region (r_regionkey INT PRIMARY KEY, r_name CHAR(25), r_comment VARCHAR(152));
CREATE TABLE nation (n_nationkey INT PRIMARY KEY, n_name CHAR(25), n_regionkey INT, n_comment VARCHAR(152));
CREATE TABLE supplier (s_suppkey INT PRIMARY KEY, s_name CHAR(25), s_address VARCHAR(40), s_nationkey INT, s_phone CHAR(15), s_acctbal DECIMAL(15,2), s_comment VARCHAR(101));
CREATE TABLE part (p_partkey INT PRIMARY KEY, p_name VARCHAR(55), p_mfgr CHAR(25), p_brand CHAR(10), p_type VARCHAR(25), p_size INT, p_container CHAR(10), p_retailprice DECIMAL(15,2), p_comment VARCHAR(40));
CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_availqty INT, ps_supplycost DECIMAL(15,2), ps_comment VARCHAR(199), PRIMARY KEY (ps_partkey, ps_suppkey));
CREATE TABLE customer (c_custkey INT PRIMARY KEY, c_name VARCHAR(25), c_address VARCHAR(40), c_nationkey INT, c_phone CHAR(15), c_acctbal DECIMAL(15,2), c_mktsegment CHAR(10), c_comment VARCHAR(117));
CREATE TABLE orders (o_orderkey INT PRIMARY KEY, o_custkey INT, o_orderstatus CHAR(1), o_totalprice DECIMAL(15,2), o_orderdate DATE, o_orderpriority CHAR(15), o_clerk CHAR(15), o_shippriority INT, o_comment VARCHAR(79));
CREATE TABLE lineitem (l_orderkey INT, l_partkey INT, l_suppkey INT, l_linenumber INT, l_quantity DECIMAL(15,2), l_extendedprice DECIMAL(15,2), l_discount DECIMAL(15,2), l_tax DECIMAL(15,2), l_returnflag CHAR(1), l_linestatus CHAR(1), l_shipdate DATE, l_commitdate DATE, l_receiptdate DATE, l_shipinstruct CHAR(25), l_shipmode CHAR(10), l_comment VARCHAR(44), PRIMARY KEY (l_orderkey, l_linenumber));
";

/// Indexes a DBA would add for the benchmark (foreign keys and the common filter columns).
const INDEXES: &str = "
CREATE INDEX ix_l_part ON lineitem (l_partkey, l_suppkey);
CREATE INDEX ix_l_supp ON lineitem (l_suppkey);
CREATE INDEX ix_l_ship ON lineitem (l_shipdate);
CREATE INDEX ix_o_cust ON orders (o_custkey);
CREATE INDEX ix_o_date ON orders (o_orderdate);
CREATE INDEX ix_ps_supp ON partsupp (ps_suppkey);
CREATE INDEX ix_c_nation ON customer (c_nationkey);
CREATE INDEX ix_s_nation ON supplier (s_nationkey);
";

const NATIONS: [(&str, i64); 25] = [
    ("ALGERIA", 0), ("ARGENTINA", 1), ("BRAZIL", 1), ("CANADA", 1), ("EGYPT", 4), ("ETHIOPIA", 0), ("FRANCE", 3), ("GERMANY", 3), ("INDIA", 2), ("INDONESIA", 2), ("IRAN", 4), ("IRAQ", 4), ("JAPAN", 2),
    ("JORDAN", 4), ("KENYA", 0), ("MOROCCO", 0), ("MOZAMBIQUE", 0), ("PERU", 1), ("CHINA", 2), ("ROMANIA", 3), ("SAUDI ARABIA", 4), ("VIETNAM", 2), ("RUSSIA", 3), ("UNITED KINGDOM", 3), ("UNITED STATES", 1),
];
const REGIONS: [&str; 5] = ["AFRICA", "AMERICA", "ASIA", "EUROPE", "MIDDLE EAST"];
const COLORS: [&str; 24] = ["almond", "antique", "azure", "beige", "black", "blue", "brown", "burlywood", "chartreuse", "coral", "cornsilk", "forest", "frosted", "ghost", "goldenrod", "green", "honeydew", "ivory", "khaki", "lime", "magenta", "navy", "olive", "peru"];
const TYPE1: [&str; 6] = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
const TYPE2: [&str; 5] = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
const TYPE3: [&str; 5] = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];
const CONT1: [&str; 5] = ["SM", "LG", "MED", "JUMBO", "WRAP"];
const CONT2: [&str; 8] = ["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"];
const SEGMENTS: [&str; 5] = ["AUTOMOBILE", "BUILDING", "FURNITURE", "MACHINERY", "HOUSEHOLD"];
const PRIORITIES: [&str; 5] = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
const INSTRUCT: [&str; 4] = ["DELIVER IN PERSON", "COLLECT COD", "NONE", "TAKE BACK RETURN"];
const MODES: [&str; 7] = ["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];

/// The generated rows the checks need.
pub struct Data {
    pub sf: f64,
    pub customers: usize,
    lines: Vec<Line>,
    orders: HashMap<i64, Order>,
    promo: HashMap<i64, bool>,
}

struct Order {
    priority: &'static str,
}

struct Line {
    orderkey: i64,
    partkey: i64,
    qty: i64,
    price: i64, // cents
    disc: i64,  // hundredths
    rf: char,
    ls: char,
    ship: NaiveDate,
    commit: NaiveDate,
    receipt: NaiveDate,
    mode: &'static str,
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn phone(rng: &mut Rng, nation: i64) -> String {
    format!("{}-{}-{}-{}", nation + 10, rng.digits(3), rng.digits(3), rng.digits(4))
}

pub fn load(c: &mut Connection, sf: f64) -> Data {
    must(c, SCHEMA);
    let mut rng = Rng::new(19_920_101);
    let n_supp = ((10_000.0 * sf) as i64).max(10);
    let n_part = ((200_000.0 * sf) as i64).max(40);
    let n_cust = ((150_000.0 * sf) as i64).max(30);
    let n_ord = ((1_500_000.0 * sf) as i64).max(300);

    insert_rows(c, "region", &REGIONS.iter().enumerate().map(|(i, r)| format!("({}, '{}', '{}')", i, r, rng.text(4))).collect::<Vec<_>>());
    insert_rows(c, "nation", &NATIONS.iter().enumerate().map(|(i, (n, r))| format!("({}, '{}', {}, '{}')", i, n, r, rng.text(5))).collect::<Vec<_>>());

    let mut rows = vec![];
    for s in 1..=n_supp {
        let nk = rng.int(0, 24);
        let comment = if rng.chance(0.005) { "Customer unhappy Complaints noted".to_string() } else { rng.text(5) };
        rows.push(format!("({}, 'Supplier#{:09}', 'addr {}', {}, '{}', {}.{:02}, '{}')", s, s, rng.digits(8), nk, phone(&mut rng, nk), rng.int(-999, 9999), rng.int(0, 99), comment));
    }
    insert_rows(c, "supplier", &rows);

    let mut promo = HashMap::new();
    let mut rows = vec![];
    let mut retail = HashMap::new();
    for p in 1..=n_part {
        let name = (0..5).map(|_| *rng.pick(&COLORS)).collect::<Vec<_>>().join(" ");
        let m = rng.int(1, 5);
        let ptype = format!("{} {} {}", rng.pick(&TYPE1), rng.pick(&TYPE2), rng.pick(&TYPE3));
        promo.insert(p, ptype.starts_with("PROMO"));
        let cents = 90_000 + ((p / 10) % 20_001) + 100 * (p % 1000);
        retail.insert(p, cents);
        rows.push(format!("({}, '{}', 'Manufacturer#{}', 'Brand#{}{}', '{}', {}, '{} {}', {}.{:02}, '{}')", p, name, m, m, rng.int(1, 5), ptype, rng.int(1, 50), rng.pick(&CONT1), rng.pick(&CONT2), cents / 100, cents % 100, rng.text(2)));
    }
    insert_rows(c, "part", &rows);

    let supp_of = |p: i64, i: i64| (p + i * (n_supp / 4 + (p - 1) / n_supp)) % n_supp + 1;
    let mut rows = vec![];
    for p in 1..=n_part {
        for i in 0..4 {
            rows.push(format!("({}, {}, {}, {}.{:02}, '{}')", p, supp_of(p, i), rng.int(1, 9999), rng.int(1, 1000), rng.int(0, 99), rng.text(4)));
        }
    }
    insert_rows(c, "partsupp", &rows);

    let mut rows = vec![];
    for k in 1..=n_cust {
        let nk = rng.int(0, 24);
        rows.push(format!("({}, 'Customer#{:09}', 'addr {}', {}, '{}', {}.{:02}, '{}', '{}')", k, k, rng.digits(8), nk, phone(&mut rng, nk), rng.int(-999, 9999), rng.int(0, 99), rng.pick(&SEGMENTS), rng.text(5)));
    }
    insert_rows(c, "customer", &rows);

    // orders and their lines; a third of the customers never order (queries 13 and 22 depend on that)
    let (mut orows, mut lrows) = (vec![], vec![]);
    let mut lines = vec![];
    let mut orders = HashMap::new();
    let start = d("1992-01-01");
    let span = (d("1998-08-02") - start).num_days();
    let cutoff = d("1995-06-17");
    for o in 1..=n_ord {
        let mut cust = rng.int(1, n_cust);
        while cust % 3 == 0 {
            cust = rng.int(1, n_cust);
        }
        let odate = start + Duration::days(rng.int(0, span));
        let nlines = rng.int(1, 7);
        let (mut total, mut nf, mut no) = (0f64, 0, 0);
        for ln in 1..=nlines {
            let p = rng.int(1, n_part);
            let s = supp_of(p, rng.int(0, 3));
            let qty = rng.int(1, 50);
            let price = qty * retail[&p];
            let disc = rng.int(0, 10);
            let tax = rng.int(0, 8);
            let ship = odate + Duration::days(rng.int(1, 121));
            let commit = odate + Duration::days(rng.int(30, 90));
            let receipt = ship + Duration::days(rng.int(1, 30));
            let rf = if receipt <= cutoff { if rng.chance(0.5) { 'R' } else { 'A' } } else { 'N' };
            let ls = if ship > cutoff { 'O' } else { 'F' };
            if ls == 'F' { nf += 1 } else { no += 1 }
            total += price as f64 / 100.0 * (1.0 + tax as f64 / 100.0) * (1.0 - disc as f64 / 100.0);
            let mode = *rng.pick(&MODES);
            lrows.push(format!(
                "({}, {}, {}, {}, {}, {}.{:02}, 0.{:02}, 0.{:02}, '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}')",
                o, p, s, ln, qty, price / 100, price % 100, disc, tax, rf, ls, ship, commit, receipt, rng.pick(&INSTRUCT), mode, rng.text(3)
            ));
            lines.push(Line { orderkey: o, partkey: p, qty, price, disc, rf, ls, ship, commit, receipt, mode });
        }
        let status = if no == 0 { 'F' } else if nf == 0 { 'O' } else { 'P' };
        let priority = *rng.pick(&PRIORITIES);
        let comment = if rng.chance(0.02) { "special packages and requests".to_string() } else { rng.text(5) };
        orders.insert(o, Order { priority });
        orows.push(format!("({}, {}, '{}', {:.2}, '{}', '{}', 'Clerk#{:09}', 0, '{}')", o, cust, status, total, odate, priority, rng.int(1, 1000), comment));
    }
    insert_rows(c, "orders", &orows);
    insert_rows(c, "lineitem", &lrows);
    must(c, INDEXES);
    Data { sf, customers: n_cust as usize, lines, orders, promo }
}

/// The 22 queries, with the specification's validation parameters.
pub fn queries(sf: f64) -> Vec<(String, String)> {
    let frac = 0.0001 / sf;
    let q22_codes = "('13','31','23','29','30','18','17')";
    let v: Vec<(&str, String)> = vec![
        ("Q1 pricing summary", "SELECT l_returnflag, l_linestatus, SUM(l_quantity) sum_qty, SUM(l_extendedprice) sum_base_price, SUM(l_extendedprice*(1-l_discount)) sum_disc_price, SUM(l_extendedprice*(1-l_discount)*(1+l_tax)) sum_charge, AVG(l_quantity) avg_qty, AVG(l_extendedprice) avg_price, AVG(l_discount) avg_disc, COUNT(*) count_order FROM lineitem WHERE l_shipdate <= DATEADD(day, -90, '1998-12-01') GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus".to_string()),
        ("Q2 minimum cost supplier", "SELECT TOP 100 s_acctbal, s_name, n_name, p_partkey, p_mfgr, s_address, s_phone, s_comment FROM part, supplier, partsupp, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND p_size = 15 AND p_type LIKE '%BRASS' AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE' AND ps_supplycost = (SELECT MIN(ps_supplycost) FROM partsupp, supplier, nation, region WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE') ORDER BY s_acctbal DESC, n_name, s_name, p_partkey".to_string()),
        ("Q3 shipping priority", "SELECT TOP 10 l_orderkey, SUM(l_extendedprice*(1-l_discount)) revenue, o_orderdate, o_shippriority FROM customer, orders, lineitem WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate < '1995-03-15' AND l_shipdate > '1995-03-15' GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate".to_string()),
        ("Q4 order priority checking", "SELECT o_orderpriority, COUNT(*) order_count FROM orders WHERE o_orderdate >= '1993-07-01' AND o_orderdate < DATEADD(month, 3, '1993-07-01') AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate) GROUP BY o_orderpriority ORDER BY o_orderpriority".to_string()),
        ("Q5 local supplier volume", "SELECT n_name, SUM(l_extendedprice*(1-l_discount)) revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= '1994-01-01' AND o_orderdate < '1995-01-01' GROUP BY n_name ORDER BY revenue DESC".to_string()),
        ("Q6 forecasting revenue change", "SELECT SUM(l_extendedprice*l_discount) revenue FROM lineitem WHERE l_shipdate >= '1994-01-01' AND l_shipdate < '1995-01-01' AND l_discount BETWEEN 0.06 - 0.01 AND 0.06 + 0.01 AND l_quantity < 24".to_string()),
        ("Q7 volume shipping", "SELECT supp_nation, cust_nation, l_year, SUM(volume) revenue FROM (SELECT n1.n_name supp_nation, n2.n_name cust_nation, YEAR(l_shipdate) l_year, l_extendedprice*(1-l_discount) volume FROM supplier, lineitem, orders, customer, nation n1, nation n2 WHERE s_suppkey = l_suppkey AND o_orderkey = l_orderkey AND c_custkey = o_custkey AND s_nationkey = n1.n_nationkey AND c_nationkey = n2.n_nationkey AND ((n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY') OR (n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE')) AND l_shipdate BETWEEN '1995-01-01' AND '1996-12-31') shipping GROUP BY supp_nation, cust_nation, l_year ORDER BY supp_nation, cust_nation, l_year".to_string()),
        ("Q8 national market share", "SELECT o_year, SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume) mkt_share FROM (SELECT YEAR(o_orderdate) o_year, l_extendedprice*(1-l_discount) volume, n2.n_name nation FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region WHERE p_partkey = l_partkey AND s_suppkey = l_suppkey AND l_orderkey = o_orderkey AND o_custkey = c_custkey AND c_nationkey = n1.n_nationkey AND n1.n_regionkey = r_regionkey AND r_name = 'AMERICA' AND s_nationkey = n2.n_nationkey AND o_orderdate BETWEEN '1995-01-01' AND '1996-12-31' AND p_type = 'ECONOMY ANODIZED STEEL') all_nations GROUP BY o_year ORDER BY o_year".to_string()),
        ("Q9 product type profit", "SELECT nation, o_year, SUM(amount) sum_profit FROM (SELECT n_name nation, YEAR(o_orderdate) o_year, l_extendedprice*(1-l_discount) - ps_supplycost*l_quantity amount FROM part, supplier, lineitem, partsupp, orders, nation WHERE s_suppkey = l_suppkey AND ps_suppkey = l_suppkey AND ps_partkey = l_partkey AND p_partkey = l_partkey AND o_orderkey = l_orderkey AND s_nationkey = n_nationkey AND p_name LIKE '%green%') profit GROUP BY nation, o_year ORDER BY nation, o_year DESC".to_string()),
        ("Q10 returned item reporting", "SELECT TOP 20 c_custkey, c_name, SUM(l_extendedprice*(1-l_discount)) revenue, c_acctbal, n_name, c_address, c_phone, c_comment FROM customer, orders, lineitem, nation WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate >= '1993-10-01' AND o_orderdate < DATEADD(month, 3, '1993-10-01') AND l_returnflag = 'R' AND c_nationkey = n_nationkey GROUP BY c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment ORDER BY revenue DESC".to_string()),
        ("Q11 important stock", format!("SELECT ps_partkey, SUM(ps_supplycost*ps_availqty) value FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY' GROUP BY ps_partkey HAVING SUM(ps_supplycost*ps_availqty) > (SELECT SUM(ps_supplycost*ps_availqty) * {} FROM partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY') ORDER BY value DESC", frac)),
        ("Q12 shipping modes", "SELECT l_shipmode, SUM(CASE WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH' THEN 1 ELSE 0 END) high_line_count, SUM(CASE WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH' THEN 1 ELSE 0 END) low_line_count FROM orders, lineitem WHERE o_orderkey = l_orderkey AND l_shipmode IN ('MAIL', 'SHIP') AND l_commitdate < l_receiptdate AND l_shipdate < l_commitdate AND l_receiptdate >= '1994-01-01' AND l_receiptdate < '1995-01-01' GROUP BY l_shipmode ORDER BY l_shipmode".to_string()),
        ("Q13 customer distribution", "SELECT c_count, COUNT(*) custdist FROM (SELECT c_custkey, COUNT(o_orderkey) c_count FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey AND o_comment NOT LIKE '%special%requests%' GROUP BY c_custkey) c_orders GROUP BY c_count ORDER BY custdist DESC, c_count DESC".to_string()),
        ("Q14 promotion effect", "SELECT 100.00 * SUM(CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice*(1-l_discount) ELSE 0 END) / SUM(l_extendedprice*(1-l_discount)) promo_revenue FROM lineitem, part WHERE l_partkey = p_partkey AND l_shipdate >= '1995-09-01' AND l_shipdate < DATEADD(month, 1, '1995-09-01')".to_string()),
        ("Q15 top supplier", "WITH revenue0 AS (SELECT l_suppkey supplier_no, SUM(l_extendedprice*(1-l_discount)) total_revenue FROM lineitem WHERE l_shipdate >= '1996-01-01' AND l_shipdate < DATEADD(month, 3, '1996-01-01') GROUP BY l_suppkey) SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, revenue0 WHERE s_suppkey = supplier_no AND total_revenue = (SELECT MAX(total_revenue) FROM revenue0) ORDER BY s_suppkey".to_string()),
        ("Q16 parts/supplier relationship", "SELECT p_brand, p_type, p_size, COUNT(DISTINCT ps_suppkey) supplier_cnt FROM partsupp, part WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%' AND p_size IN (49, 14, 23, 45, 19, 3, 36, 9) AND ps_suppkey NOT IN (SELECT s_suppkey FROM supplier WHERE s_comment LIKE '%Customer%Complaints%') GROUP BY p_brand, p_type, p_size ORDER BY supplier_cnt DESC, p_brand, p_type, p_size".to_string()),
        ("Q17 small-quantity-order revenue", "SELECT SUM(l_extendedprice) / 7.0 avg_yearly FROM lineitem, part WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX' AND l_quantity < (SELECT 0.2 * AVG(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)".to_string()),
        ("Q18 large volume customer", "SELECT TOP 100 c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, SUM(l_quantity) FROM customer, orders, lineitem WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300) AND c_custkey = o_custkey AND o_orderkey = l_orderkey GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice ORDER BY o_totalprice DESC, o_orderdate".to_string()),
        ("Q19 discounted revenue", "SELECT SUM(l_extendedprice*(1-l_discount)) revenue FROM lineitem, part WHERE (p_partkey = l_partkey AND p_brand = 'Brand#12' AND p_container IN ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG') AND l_quantity >= 1 AND l_quantity <= 11 AND p_size BETWEEN 1 AND 5 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON') OR (p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container IN ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK') AND l_quantity >= 10 AND l_quantity <= 20 AND p_size BETWEEN 1 AND 10 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON') OR (p_partkey = l_partkey AND p_brand = 'Brand#34' AND p_container IN ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG') AND l_quantity >= 20 AND l_quantity <= 30 AND p_size BETWEEN 1 AND 15 AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')".to_string()),
        ("Q20 potential part promotion", "SELECT s_name, s_address FROM supplier, nation WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%') AND ps_availqty > (SELECT 0.5 * SUM(l_quantity) FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey AND l_shipdate >= '1994-01-01' AND l_shipdate < '1995-01-01')) AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name".to_string()),
        ("Q21 suppliers who kept orders waiting", "SELECT TOP 100 s_name, COUNT(*) numwait FROM supplier, lineitem l1, orders, nation WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey AND o_orderstatus = 'F' AND l1.l_receiptdate > l1.l_commitdate AND EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey) AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate) AND s_nationkey = n_nationkey AND n_name = 'SAUDI ARABIA' GROUP BY s_name ORDER BY numwait DESC, s_name".to_string()),
        ("Q22 global sales opportunity", format!("SELECT cntrycode, COUNT(*) numcust, SUM(c_acctbal) totacctbal FROM (SELECT SUBSTRING(c_phone, 1, 2) cntrycode, c_acctbal FROM customer WHERE SUBSTRING(c_phone, 1, 2) IN {codes} AND c_acctbal > (SELECT AVG(c_acctbal) FROM customer WHERE c_acctbal > 0.00 AND SUBSTRING(c_phone, 1, 2) IN {codes}) AND NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)) custsale GROUP BY cntrycode ORDER BY cntrycode", codes = q22_codes)),
    ];
    v.into_iter().map(|(n, s)| (n.to_string(), s)).collect()
}

/// Recompute a few answers from the generated rows. Returns `None` for queries without a check, otherwise whether the
/// engine's rows match.
pub fn check(data: &Data, name: &str, rows: &[Vec<String>]) -> Option<Result<(), String>> {
    let money = |c: i64| c as f64 / 100.0;
    match name.split(' ').next().unwrap() {
        "Q1" => {
            let cut = d("1998-09-02");
            let mut g: std::collections::BTreeMap<(char, char), (i64, i64, f64, f64, i64, i64)> = Default::default();
            for l in data.lines.iter().filter(|l| l.ship <= cut) {
                let e = g.entry((l.rf, l.ls)).or_default();
                e.0 += l.qty;
                e.1 += l.price;
                let disc_price = money(l.price) * (1.0 - l.disc as f64 / 100.0);
                e.2 += disc_price;
                e.3 += 0.0;
                e.4 += l.disc;
                e.5 += 1;
            }
            if g.len() != rows.len() {
                return Some(Err(format!("{} groups, expected {}", rows.len(), g.len())));
            }
            for (((rf, ls), v), r) in g.iter().zip(rows) {
                let ok = r[0].trim() == rf.to_string() && r[1].trim() == ls.to_string() && close(num(&r[2]), v.0 as f64) && close(num(&r[3]), money(v.1)) && close(num(&r[4]), v.2) && num(&r[9]) as i64 == v.5 && close(num(&r[8]), v.4 as f64 / 100.0 / v.5 as f64);
                if !ok {
                    return Some(Err(format!("group {}{} differs: engine {:?}", rf, ls, r)));
                }
            }
            Some(Ok(()))
        }
        "Q18" => {
            let mut sums: HashMap<i64, i64> = HashMap::new();
            for l in &data.lines {
                *sums.entry(l.orderkey).or_default() += l.qty;
            }
            let n = sums.values().filter(|v| **v > 300).count().min(100);
            if rows.len() == n {
                Some(Ok(()))
            } else {
                Some(Err(format!("{} rows, expected {}", rows.len(), n)))
            }
        }
        "Q6" => {
            let (lo, hi) = (d("1994-01-01"), d("1995-01-01"));
            let sum: i64 = data.lines.iter().filter(|l| l.ship >= lo && l.ship < hi && (5..=7).contains(&l.disc) && l.qty < 24).map(|l| l.price * l.disc).sum();
            let got = num(&rows[0][0]);
            let want = sum as f64 / 10_000.0;
            if close(got, want) {
                Some(Ok(()))
            } else {
                Some(Err(format!("revenue {} expected {}", got, want)))
            }
        }
        "Q12" => {
            let (lo, hi) = (d("1994-01-01"), d("1995-01-01"));
            let mut g: std::collections::BTreeMap<&str, (i64, i64)> = Default::default();
            for l in data.lines.iter().filter(|l| (l.mode == "MAIL" || l.mode == "SHIP") && l.commit < l.receipt && l.ship < l.commit && l.receipt >= lo && l.receipt < hi) {
                let high = matches!(data.orders[&l.orderkey].priority, "1-URGENT" | "2-HIGH");
                let e = g.entry(l.mode).or_default();
                if high { e.0 += 1 } else { e.1 += 1 }
            }
            if g.len() != rows.len() {
                return Some(Err(format!("{} modes, expected {}", rows.len(), g.len())));
            }
            for ((m, (h, lo_)), r) in g.iter().zip(rows) {
                if r[0].trim() != *m || num(&r[1]) as i64 != *h || num(&r[2]) as i64 != *lo_ {
                    return Some(Err(format!("{} differs: engine {:?}, expected {} {} {}", m, r, m, h, lo_)));
                }
            }
            Some(Ok(()))
        }
        "Q13" => {
            let total: i64 = rows.iter().map(|r| num(&r[1]) as i64).sum();
            if total == data.customers as i64 {
                Some(Ok(()))
            } else {
                Some(Err(format!("distribution covers {} customers, expected {}", total, data.customers)))
            }
        }
        "Q14" => {
            let (lo, hi) = (d("1995-09-01"), d("1995-10-01"));
            let (mut promo, mut all) = (0f64, 0f64);
            for l in data.lines.iter().filter(|l| l.ship >= lo && l.ship < hi) {
                let v = money(l.price) * (1.0 - l.disc as f64 / 100.0);
                all += v;
                if data.promo[&l.partkey] {
                    promo += v;
                }
            }
            let want = 100.0 * promo / all;
            let got = num(&rows[0][0]);
            if (got - want).abs() < 0.01 {
                Some(Ok(()))
            } else {
                Some(Err(format!("promo revenue {} expected {}", got, want)))
            }
        }
        _ => None,
    }
}
