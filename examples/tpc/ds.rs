//! TPC-DS: the core retail snowflake (three sales channels, returns, inventory, and the dimensions they share), a
//! generator for it, and a set of queries written in the style of the benchmark's (star joins, correlated
//! subqueries, windows over aggregates, ROLLUP, INTERSECT/EXCEPT, year-over-year CTEs).
//!
//! This is a subset: the full benchmark has 24 tables and 99 queries from templates that are not reproduced here, and
//! the generator is not `dsdgen`. A few answers are recomputed in Rust from the generated rows.
use crate::util::*;
use chrono::{Datelike, Duration, NaiveDate};
use std::collections::HashMap;
use tsql_server::Connection;

const SCHEMA: &str = "
CREATE TABLE date_dim (d_date_sk INT PRIMARY KEY, d_date DATE, d_month_seq INT, d_week_seq INT, d_year INT, d_dow INT, d_moy INT, d_dom INT, d_qoy INT, d_day_name VARCHAR(9));
CREATE TABLE time_dim (t_time_sk INT PRIMARY KEY, t_time INT, t_hour INT, t_minute INT);
CREATE TABLE item (i_item_sk INT PRIMARY KEY, i_item_id CHAR(16), i_item_desc VARCHAR(200), i_current_price DECIMAL(7,2), i_class VARCHAR(50), i_category_id INT, i_category CHAR(50), i_brand_id INT, i_brand CHAR(50), i_manufact_id INT, i_manager_id INT, i_color CHAR(20));
CREATE TABLE customer_demographics (cd_demo_sk INT PRIMARY KEY, cd_gender CHAR(1), cd_marital_status CHAR(1), cd_education_status CHAR(20), cd_dep_count INT);
CREATE TABLE household_demographics (hd_demo_sk INT PRIMARY KEY, hd_income_band_sk INT, hd_buy_potential CHAR(15), hd_dep_count INT, hd_vehicle_count INT);
CREATE TABLE customer_address (ca_address_sk INT PRIMARY KEY, ca_state CHAR(2), ca_zip CHAR(10), ca_country VARCHAR(20), ca_city VARCHAR(60));
CREATE TABLE customer (c_customer_sk INT PRIMARY KEY, c_customer_id CHAR(16), c_current_cdemo_sk INT, c_current_hdemo_sk INT, c_current_addr_sk INT, c_first_name CHAR(20), c_last_name CHAR(30), c_birth_year INT);
CREATE TABLE store (s_store_sk INT PRIMARY KEY, s_store_id CHAR(16), s_store_name VARCHAR(50), s_state CHAR(2), s_zip CHAR(10), s_number_employees INT);
CREATE TABLE promotion (p_promo_sk INT PRIMARY KEY, p_channel_email CHAR(1), p_channel_event CHAR(1), p_channel_tv CHAR(1));
CREATE TABLE warehouse (w_warehouse_sk INT PRIMARY KEY, w_warehouse_name VARCHAR(20), w_state CHAR(2));
CREATE TABLE store_sales (ss_sold_date_sk INT, ss_sold_time_sk INT, ss_item_sk INT, ss_customer_sk INT, ss_cdemo_sk INT, ss_hdemo_sk INT, ss_addr_sk INT, ss_store_sk INT, ss_promo_sk INT, ss_ticket_number INT, ss_quantity INT, ss_list_price DECIMAL(7,2), ss_sales_price DECIMAL(7,2), ss_ext_sales_price DECIMAL(7,2), ss_coupon_amt DECIMAL(7,2), ss_net_profit DECIMAL(7,2), PRIMARY KEY (ss_item_sk, ss_ticket_number));
CREATE TABLE store_returns (sr_returned_date_sk INT, sr_item_sk INT, sr_customer_sk INT, sr_store_sk INT, sr_ticket_number INT, sr_return_quantity INT, sr_return_amt DECIMAL(7,2), PRIMARY KEY (sr_item_sk, sr_ticket_number));
CREATE TABLE catalog_sales (cs_sold_date_sk INT, cs_item_sk INT, cs_bill_customer_sk INT, cs_bill_cdemo_sk INT, cs_promo_sk INT, cs_order_number INT, cs_quantity INT, cs_list_price DECIMAL(7,2), cs_sales_price DECIMAL(7,2), cs_ext_sales_price DECIMAL(7,2), cs_coupon_amt DECIMAL(7,2), cs_net_profit DECIMAL(7,2), PRIMARY KEY (cs_item_sk, cs_order_number));
CREATE TABLE web_sales (ws_sold_date_sk INT, ws_item_sk INT, ws_bill_customer_sk INT, ws_order_number INT, ws_quantity INT, ws_sales_price DECIMAL(7,2), ws_ext_sales_price DECIMAL(7,2), ws_net_profit DECIMAL(7,2), PRIMARY KEY (ws_item_sk, ws_order_number));
CREATE TABLE inventory (inv_date_sk INT, inv_item_sk INT, inv_warehouse_sk INT, inv_quantity_on_hand INT, PRIMARY KEY (inv_date_sk, inv_item_sk, inv_warehouse_sk));
";

const INDEXES: &str = "
CREATE INDEX ix_ss_date ON store_sales (ss_sold_date_sk);
CREATE INDEX ix_ss_cust ON store_sales (ss_customer_sk);
CREATE INDEX ix_ss_store ON store_sales (ss_store_sk);
CREATE INDEX ix_cs_date ON catalog_sales (cs_sold_date_sk);
CREATE INDEX ix_cs_cust ON catalog_sales (cs_bill_customer_sk);
CREATE INDEX ix_ws_date ON web_sales (ws_sold_date_sk);
CREATE INDEX ix_ws_cust ON web_sales (ws_bill_customer_sk);
CREATE INDEX ix_sr_cust ON store_returns (sr_customer_sk);
CREATE INDEX ix_c_addr ON customer (c_current_addr_sk);
CREATE INDEX ix_i_manager ON item (i_manager_id);
CREATE INDEX ix_i_manufact ON item (i_manufact_id);
";

const CATEGORIES: [&str; 10] = ["Women", "Men", "Children", "Shoes", "Music", "Jewelry", "Home", "Sports", "Books", "Electronics"];
const STATES: [&str; 12] = ["TN", "SD", "AL", "GA", "KY", "NM", "CA", "WA", "TX", "OH", "VA", "NY"];
const EDUCATION: [&str; 7] = ["Primary", "Secondary", "College", "2 yr Degree", "4 yr Degree", "Advanced Degree", "Unknown"];
const COLORS: [&str; 8] = ["red", "blue", "green", "white", "black", "yellow", "brown", "pink"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const FIRST: [&str; 10] = ["James", "Mary", "John", "Linda", "Robert", "Susan", "Michael", "Karen", "David", "Nancy"];
const LAST: [&str; 10] = ["Smith", "Jones", "Brown", "Davis", "Miller", "Wilson", "Moore", "Taylor", "Clark", "Hall"];

const BASE: NaiveDate = match NaiveDate::from_ymd_opt(1998, 1, 1) {
    Some(d) => d,
    None => panic!(),
};
const DATE_SK0: i64 = 2_450_815;
const DAYS_SPAN: i64 = 1826; // 1998-01-01 .. 2002-12-31

struct Sale {
    date: i64,
    time: i64,
    item: i64,
    hdemo: i64,
    store: i64,
    qty: i64,
    ext: i64, // cents
}

struct ItemInfo {
    manufact: i64,
    manager: i64,
    brand_id: i64,
    brand: String,
    cat_id: i64,
    cat: String,
}

pub struct Data {
    sales: Vec<Sale>,
    items: HashMap<i64, ItemInfo>,
    dates: HashMap<i64, (i32, u32)>, // sk -> (year, month)
    hd_dep: HashMap<i64, i64>,
    store_names: HashMap<i64, String>,
}

fn money(c: i64) -> String {
    format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.abs() / 100, c.abs() % 100)
}

fn opt(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "NULL".to_string())
}

pub fn load(c: &mut Connection, sf: f64) -> Data {
    must(c, SCHEMA);
    let mut rng = Rng::new(20_000_101);
    let scale = (sf / 0.01).max(0.1);
    let n_item = ((300.0 * scale) as i64).max(300);
    let n_cust = ((1000.0 * scale) as i64).max(300);
    let n_addr = ((500.0 * scale) as i64).max(300);
    let n_store = 12i64;

    // dates and times
    let mut rows = vec![];
    let mut dates = HashMap::new();
    for off in 0..DAYS_SPAN {
        let dt = BASE + Duration::days(off);
        let sk = DATE_SK0 + off;
        let (y, m) = (dt.year(), dt.month());
        dates.insert(sk, (y, m));
        rows.push(format!("({}, '{}', {}, {}, {}, {}, {}, {}, {}, '{}')", sk, dt, (y - 1900) * 12 + m as i32 - 1, off / 7 + 1, y, dt.weekday().num_days_from_sunday(), m, dt.day(), (m + 2) / 3, DAYS[dt.weekday().num_days_from_sunday() as usize]));
    }
    insert_rows(c, "date_dim", &rows);
    insert_rows(c, "time_dim", &(0..1440).map(|m| format!("({}, {}, {}, {})", m, m * 60, m / 60, m % 60)).collect::<Vec<_>>());

    // items
    let mut items = HashMap::new();
    let mut rows = vec![];
    for i in 1..=n_item {
        let cat_id = rng.int(1, 10);
        let cat = CATEGORIES[(cat_id - 1) as usize];
        let manufact = if i % 150 == 1 { 128 } else { rng.int(1, 200) };
        let manager = rng.int(1, 100);
        let brand_id = cat_id * 1_000_000 + rng.int(1, 20) * 1000 + rng.int(1, 9);
        let brand = format!("brand #{}", brand_id % 997);
        let price = rng.int(100, 20_000);
        rows.push(format!("({}, 'AAAAAAAA{:08}', '{} item {}', {}, '{} class {}', {}, '{}', {}, '{}', {}, {}, '{}')", i, i, cat, rng.text(3), money(price), cat, rng.int(1, 5), cat_id, cat, brand_id, brand, manufact, manager, rng.pick(&COLORS)));
        items.insert(i, ItemInfo { manufact, manager, brand_id, brand, cat_id, cat: cat.to_string() });
    }
    insert_rows(c, "item", &rows);

    // demographics
    let mut rows = vec![];
    let mut sk = 0;
    for g in ["M", "F"] {
        for m in ["M", "S", "D", "W", "U"] {
            for e in EDUCATION {
                for dep in 0..4 {
                    sk += 1;
                    rows.push(format!("({}, '{}', '{}', '{}', {})", sk, g, m, e, dep));
                }
            }
        }
    }
    let n_cd = sk;
    insert_rows(c, "customer_demographics", &rows);
    let mut rows = vec![];
    let mut hd_dep = HashMap::new();
    let n_hd = 600i64;
    for h in 1..=n_hd {
        let dep = rng.int(0, 9);
        hd_dep.insert(h, dep);
        rows.push(format!("({}, {}, '{}', {}, {})", h, rng.int(1, 20), rng.pick(&[">10000", "5001-10000", "1001-5000", "501-1000", "0-500", "Unknown"]), dep, rng.int(0, 4)));
    }
    insert_rows(c, "household_demographics", &rows);
    insert_rows(c, "customer_address", &(1..=n_addr).map(|a| format!("({}, '{}', '{}', 'United States', 'City {}')", a, rng.pick(&STATES), if a % 9 == 0 { "85669".to_string() } else { rng.digits(5) }, rng.int(1, 60))).collect::<Vec<_>>());
    insert_rows(c, "customer", &(1..=n_cust).map(|k| format!("({}, 'AAAAAAAA{:08}', {}, {}, {}, '{}', '{}', {})", k, k, rng.int(1, n_cd), rng.int(1, n_hd), rng.int(1, n_addr), rng.pick(&FIRST), rng.pick(&LAST), rng.int(1930, 1992))).collect::<Vec<_>>());
    let store_names = ["ese", "ought", "able", "bar", "eing", "ation", "anti", "cally", "n st", "pri", "ese2", "ought2"];
    insert_rows(c, "store", &(1..=n_store).map(|s| format!("({}, 'AAAAAAAA{:08}', '{}', '{}', '{}', {})", s, s, store_names[(s - 1) as usize], rng.pick(&STATES), rng.digits(5), rng.int(200, 300))).collect::<Vec<_>>());
    insert_rows(c, "promotion", &(1..=30).map(|p| format!("({}, '{}', '{}', '{}')", p, rng.pick(&["Y", "N"]), rng.pick(&["Y", "N"]), rng.pick(&["Y", "N"]))).collect::<Vec<_>>());
    insert_rows(c, "warehouse", &(1..=5).map(|w| format!("({}, 'Warehouse {}', '{}')", w, w, rng.pick(&STATES))).collect::<Vec<_>>());

    // store sales (tickets of several lines) and returns
    let n_tickets = ((4000.0 * scale) as i64).max(2000);
    let (mut ss, mut sr, mut sales) = (vec![], vec![], vec![]);
    for t in 1..=n_tickets {
        let date = DATE_SK0 + rng.int(0, DAYS_SPAN - 1);
        let time = rng.int(8 * 60, 21 * 60 + 59);
        let cust = if rng.chance(0.02) { None } else { Some(rng.int(1, n_cust)) };
        let (cd, hd, addr, store, promo) = (rng.int(1, n_cd), rng.int(1, n_hd), rng.int(1, n_addr), rng.int(1, n_store), rng.int(1, 30));
        let mut used = vec![];
        for _ in 0..rng.int(1, 8) {
            let item = rng.int(1, n_item);
            if used.contains(&item) {
                continue;
            }
            used.push(item);
            let qty = rng.int(1, 100);
            let list = rng.int(100, 20_000);
            let price = list * rng.int(50, 100) / 100;
            let ext = price * qty;
            ss.push(format!("({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})", date, time, item, opt(cust), cd, hd, addr, store, promo, t, qty, money(list), money(price), money(ext), money(rng.int(0, ext / 10)), money(rng.int(-ext / 4, ext / 3))));
            sales.push(Sale { date, time, item, hdemo: hd, store, qty, ext });
            if rng.chance(0.1) {
                let rdate = (date + rng.int(1, 90)).min(DATE_SK0 + DAYS_SPAN - 1);
                let rq = rng.int(1, qty);
                sr.push(format!("({}, {}, {}, {}, {}, {}, {})", rdate, item, opt(cust), store, t, rq, money(price * rq)));
            }
        }
    }
    insert_rows(c, "store_sales", &ss);
    insert_rows(c, "store_returns", &sr);

    let (mut cs, mut ws) = (vec![], vec![]);
    for o in 1..=(n_tickets / 2) {
        let date = DATE_SK0 + rng.int(0, DAYS_SPAN - 1);
        let cust = rng.int(1, n_cust);
        let mut used = vec![];
        for _ in 0..rng.int(1, 5) {
            let item = rng.int(1, n_item);
            if used.contains(&item) {
                continue;
            }
            used.push(item);
            let qty = rng.int(1, 100);
            let list = rng.int(100, 20_000);
            let price = list * rng.int(50, 100) / 100;
            let ext = price * qty;
            cs.push(format!("({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})", date, item, cust, rng.int(1, n_cd), rng.int(1, 30), o, qty, money(list), money(price), money(ext), money(rng.int(0, ext / 10)), money(rng.int(-ext / 4, ext / 3))));
        }
        let mut used = vec![];
        for _ in 0..rng.int(0, 4) {
            let item = rng.int(1, n_item);
            if used.contains(&item) {
                continue;
            }
            used.push(item);
            let qty = rng.int(1, 100);
            let price = rng.int(100, 15_000);
            ws.push(format!("({}, {}, {}, {}, {}, {}, {}, {})", DATE_SK0 + rng.int(0, DAYS_SPAN - 1), item, rng.int(1, n_cust), o, qty, money(price), money(price * qty), money(rng.int(-price * qty / 4, price * qty / 3))));
        }
    }
    insert_rows(c, "catalog_sales", &cs);
    insert_rows(c, "web_sales", &ws);

    // inventory snapshots on eight dates
    let mut inv = vec![];
    for k in 0..8 {
        let date = DATE_SK0 + 700 + k * 28; // within 1999-2000, includes month_seq 1200..1211
        for i in 1..=n_item {
            for w in 1..=5 {
                inv.push(format!("({}, {}, {}, {})", date, i, w, rng.int(0, 1000)));
            }
        }
    }
    insert_rows(c, "inventory", &inv);
    must(c, INDEXES);
    let store_names = store_names.iter().enumerate().map(|(i, n)| (i as i64 + 1, n.to_string())).collect();
    Data { sales, items, dates, hd_dep, store_names }
}

pub fn queries() -> Vec<(String, String)> {
    let v: Vec<(&str, &str)> = vec![
        ("Q1 customers returning more than average", "WITH customer_total_return AS (SELECT sr_customer_sk ctr_customer_sk, sr_store_sk ctr_store_sk, SUM(sr_return_amt) ctr_total_return FROM store_returns, date_dim WHERE sr_returned_date_sk = d_date_sk AND d_year = 2000 GROUP BY sr_customer_sk, sr_store_sk) SELECT TOP 100 c_customer_id FROM customer_total_return ctr1, store, customer WHERE ctr1.ctr_total_return > (SELECT AVG(ctr_total_return) * 1.2 FROM customer_total_return ctr2 WHERE ctr1.ctr_store_sk = ctr2.ctr_store_sk) AND s_store_sk = ctr1.ctr_store_sk AND s_state = 'TN' AND ctr1.ctr_customer_sk = c_customer_sk ORDER BY c_customer_id"),
        ("Q3 brand sales for a manufacturer", "SELECT TOP 100 dt.d_year, item.i_brand_id brand_id, item.i_brand brand, SUM(ss_ext_sales_price) sum_agg FROM date_dim dt, store_sales, item WHERE dt.d_date_sk = store_sales.ss_sold_date_sk AND store_sales.ss_item_sk = item.i_item_sk AND item.i_manufact_id = 128 AND dt.d_moy = 11 GROUP BY dt.d_year, item.i_brand, item.i_brand_id ORDER BY dt.d_year, sum_agg DESC, brand_id"),
        ("Q6 states with expensive items", "SELECT TOP 100 a.ca_state state, COUNT(*) cnt FROM customer_address a, customer c, store_sales s, date_dim d, item i WHERE a.ca_address_sk = c.c_current_addr_sk AND c.c_customer_sk = s.ss_customer_sk AND s.ss_sold_date_sk = d.d_date_sk AND s.ss_item_sk = i.i_item_sk AND d.d_month_seq = (SELECT DISTINCT d_month_seq FROM date_dim WHERE d_year = 2001 AND d_moy = 1) AND i.i_current_price > 1.2 * (SELECT AVG(j.i_current_price) FROM item j WHERE j.i_category = i.i_category) GROUP BY a.ca_state HAVING COUNT(*) >= 1 ORDER BY cnt, a.ca_state"),
        ("Q7 promotion averages", "SELECT TOP 100 i_item_id, AVG(ss_quantity) agg1, AVG(ss_list_price) agg2, AVG(ss_coupon_amt) agg3, AVG(ss_sales_price) agg4 FROM store_sales, customer_demographics, date_dim, item, promotion WHERE ss_sold_date_sk = d_date_sk AND ss_item_sk = i_item_sk AND ss_cdemo_sk = cd_demo_sk AND ss_promo_sk = p_promo_sk AND cd_gender = 'M' AND cd_marital_status = 'S' AND cd_education_status = 'College' AND (p_channel_email = 'N' OR p_channel_event = 'N') AND d_year = 2000 GROUP BY i_item_id ORDER BY i_item_id"),
        ("Q12 web revenue ratio per class", "SELECT TOP 100 i_item_id, i_item_desc, i_category, i_class, i_current_price, SUM(ws_ext_sales_price) AS itemrevenue, SUM(ws_ext_sales_price) * 100.0 / SUM(SUM(ws_ext_sales_price)) OVER (PARTITION BY i_class) AS revenueratio FROM web_sales, item, date_dim WHERE ws_item_sk = i_item_sk AND i_category IN ('Sports', 'Books', 'Home') AND ws_sold_date_sk = d_date_sk AND d_date BETWEEN '1999-02-22' AND '1999-03-24' GROUP BY i_item_id, i_item_desc, i_category, i_class, i_current_price ORDER BY i_category, i_class, i_item_id, i_item_desc, revenueratio"),
        ("Q15 catalog sales by zip", "SELECT TOP 100 ca_zip, SUM(cs_sales_price) FROM catalog_sales, customer, customer_address, date_dim WHERE cs_bill_customer_sk = c_customer_sk AND c_current_addr_sk = ca_address_sk AND (SUBSTRING(ca_zip, 1, 5) IN ('85669', '86197', '88274', '83405', '86475', '85392', '85460', '80348', '81792') OR ca_state IN ('CA', 'WA', 'GA') OR cs_sales_price > 500) AND cs_sold_date_sk = d_date_sk AND d_qoy = 2 AND d_year = 2001 GROUP BY ca_zip ORDER BY ca_zip"),
        ("Q19 brand sales outside the zip", "SELECT TOP 100 i_brand_id brand_id, i_brand brand, i_manufact_id, SUM(ss_ext_sales_price) ext_price FROM date_dim, store_sales, item, customer, customer_address, store WHERE d_date_sk = ss_sold_date_sk AND ss_item_sk = i_item_sk AND i_manager_id = 8 AND d_moy = 11 AND d_year = 1998 AND ss_customer_sk = c_customer_sk AND c_current_addr_sk = ca_address_sk AND SUBSTRING(ca_zip, 1, 5) <> SUBSTRING(s_zip, 1, 5) AND ss_store_sk = s_store_sk GROUP BY i_brand, i_brand_id, i_manufact_id ORDER BY ext_price DESC, i_brand, i_brand_id, i_manufact_id"),
        ("Q20 catalog revenue ratio per class", "SELECT TOP 100 i_item_id, i_category, i_class, i_current_price, SUM(cs_ext_sales_price) AS itemrevenue, SUM(cs_ext_sales_price) * 100.0 / SUM(SUM(cs_ext_sales_price)) OVER (PARTITION BY i_class) AS revenueratio FROM catalog_sales, item, date_dim WHERE cs_item_sk = i_item_sk AND i_category IN ('Sports', 'Books', 'Home') AND cs_sold_date_sk = d_date_sk AND d_date BETWEEN '1999-02-22' AND '1999-03-24' GROUP BY i_item_id, i_category, i_class, i_current_price ORDER BY i_category, i_class, i_item_id, revenueratio"),
        ("Q22 inventory rollup", "SELECT TOP 100 i_item_id, i_category, AVG(inv_quantity_on_hand) qoh FROM inventory, date_dim, item WHERE inv_date_sk = d_date_sk AND inv_item_sk = i_item_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11 GROUP BY ROLLUP (i_category, i_item_id) ORDER BY qoh, i_category, i_item_id"),
        ("Q26 catalog promotion averages", "SELECT TOP 100 i_item_id, AVG(cs_quantity) agg1, AVG(cs_list_price) agg2, AVG(cs_coupon_amt) agg3, AVG(cs_sales_price) agg4 FROM catalog_sales, customer_demographics, date_dim, item, promotion WHERE cs_sold_date_sk = d_date_sk AND cs_item_sk = i_item_sk AND cs_bill_cdemo_sk = cd_demo_sk AND cs_promo_sk = p_promo_sk AND cd_gender = 'M' AND cd_marital_status = 'S' AND cd_education_status = 'College' AND (p_channel_email = 'N' OR p_channel_event = 'N') AND d_year = 2000 GROUP BY i_item_id ORDER BY i_item_id"),
        ("Q27 state rollup with grouping", "SELECT TOP 100 i_item_id, s_state, GROUPING(s_state) g_state, AVG(ss_quantity) agg1, AVG(ss_list_price) agg2, AVG(ss_coupon_amt) agg3, AVG(ss_sales_price) agg4 FROM store_sales, customer_demographics, date_dim, store, item WHERE ss_sold_date_sk = d_date_sk AND ss_item_sk = i_item_sk AND ss_store_sk = s_store_sk AND ss_cdemo_sk = cd_demo_sk AND cd_gender = 'M' AND cd_marital_status = 'S' AND cd_education_status = 'College' AND d_year = 2002 AND s_state IN ('TN', 'SD', 'AL', 'GA') GROUP BY ROLLUP (i_item_id, s_state) ORDER BY i_item_id, s_state"),
        ("Q36 gross margin rank within parent", "SELECT TOP 100 SUM(ss_net_profit) / SUM(ss_ext_sales_price) AS gross_margin, i_category, i_class, GROUPING(i_category) + GROUPING(i_class) AS lochierarchy, RANK() OVER (PARTITION BY GROUPING(i_category) + GROUPING(i_class), CASE WHEN GROUPING(i_class) = 0 THEN i_category END ORDER BY SUM(ss_net_profit) / SUM(ss_ext_sales_price) ASC) AS rank_within_parent FROM store_sales, date_dim d1, item, store WHERE d1.d_year = 2001 AND d1.d_date_sk = ss_sold_date_sk AND i_item_sk = ss_item_sk AND s_store_sk = ss_store_sk AND s_state IN ('TN', 'SD', 'AL', 'GA') GROUP BY ROLLUP (i_category, i_class) ORDER BY lochierarchy DESC, CASE WHEN GROUPING(i_category) + GROUPING(i_class) = 0 THEN i_category END, rank_within_parent"),
        ("Q38 customers buying on all channels", "SELECT COUNT(*) FROM (SELECT DISTINCT c_last_name, c_first_name, d_date FROM store_sales, date_dim, customer WHERE ss_sold_date_sk = d_date_sk AND ss_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11 INTERSECT SELECT DISTINCT c_last_name, c_first_name, d_date FROM catalog_sales, date_dim, customer WHERE cs_sold_date_sk = d_date_sk AND cs_bill_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11 INTERSECT SELECT DISTINCT c_last_name, c_first_name, d_date FROM web_sales, date_dim, customer WHERE ws_sold_date_sk = d_date_sk AND ws_bill_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11) hot_cust"),
        ("Q42 category sales for a manager", "SELECT TOP 100 dt.d_year, item.i_category_id, item.i_category, SUM(ss_ext_sales_price) FROM date_dim dt, store_sales, item WHERE dt.d_date_sk = store_sales.ss_sold_date_sk AND store_sales.ss_item_sk = item.i_item_sk AND item.i_manager_id = 1 AND dt.d_moy = 11 AND dt.d_year = 2000 GROUP BY dt.d_year, item.i_category_id, item.i_category ORDER BY SUM(ss_ext_sales_price) DESC, dt.d_year, item.i_category_id, item.i_category"),
        ("Q43 store sales by weekday", "SELECT TOP 100 s_store_name, s_store_id, SUM(CASE WHEN d_day_name = 'Sunday' THEN ss_sales_price ELSE NULL END) sun_sales, SUM(CASE WHEN d_day_name = 'Monday' THEN ss_sales_price ELSE NULL END) mon_sales, SUM(CASE WHEN d_day_name = 'Tuesday' THEN ss_sales_price ELSE NULL END) tue_sales, SUM(CASE WHEN d_day_name = 'Wednesday' THEN ss_sales_price ELSE NULL END) wed_sales, SUM(CASE WHEN d_day_name = 'Thursday' THEN ss_sales_price ELSE NULL END) thu_sales, SUM(CASE WHEN d_day_name = 'Friday' THEN ss_sales_price ELSE NULL END) fri_sales, SUM(CASE WHEN d_day_name = 'Saturday' THEN ss_sales_price ELSE NULL END) sat_sales FROM date_dim, store_sales, store WHERE d_date_sk = ss_sold_date_sk AND s_store_sk = ss_store_sk AND d_year = 2000 GROUP BY s_store_name, s_store_id ORDER BY s_store_name, s_store_id, sun_sales, mon_sales"),
        ("Q50 return latency buckets", "SELECT TOP 100 s_store_name, s_state, SUM(CASE WHEN sr_returned_date_sk - ss_sold_date_sk <= 30 THEN 1 ELSE 0 END) AS d30, SUM(CASE WHEN sr_returned_date_sk - ss_sold_date_sk > 30 AND sr_returned_date_sk - ss_sold_date_sk <= 60 THEN 1 ELSE 0 END) AS d31_60, SUM(CASE WHEN sr_returned_date_sk - ss_sold_date_sk > 60 THEN 1 ELSE 0 END) AS d60_plus FROM store_sales, store_returns, store, date_dim d1, date_dim d2 WHERE d2.d_year = 2001 AND ss_ticket_number = sr_ticket_number AND ss_item_sk = sr_item_sk AND ss_sold_date_sk = d1.d_date_sk AND sr_returned_date_sk = d2.d_date_sk AND ss_customer_sk = sr_customer_sk AND ss_store_sk = s_store_sk GROUP BY s_store_name, s_state ORDER BY s_store_name, s_state"),
        ("Q52 brand sales for a manager", "SELECT TOP 100 dt.d_year, item.i_brand_id brand_id, item.i_brand brand, SUM(ss_ext_sales_price) ext_price FROM date_dim dt, store_sales, item WHERE dt.d_date_sk = store_sales.ss_sold_date_sk AND store_sales.ss_item_sk = item.i_item_sk AND item.i_manager_id = 1 AND dt.d_moy = 11 AND dt.d_year = 2000 GROUP BY dt.d_year, item.i_brand, item.i_brand_id ORDER BY dt.d_year, ext_price DESC, brand_id"),
        ("Q55 brand sales for manager 28", "SELECT TOP 100 i_brand_id brand_id, i_brand brand, SUM(ss_ext_sales_price) ext_price FROM date_dim, store_sales, item WHERE d_date_sk = ss_sold_date_sk AND ss_item_sk = i_item_sk AND i_manager_id = 28 AND d_moy = 11 AND d_year = 1999 GROUP BY i_brand, i_brand_id ORDER BY ext_price DESC, i_brand_id"),
        ("Q69 store-only customers", "SELECT TOP 100 cd_gender, cd_marital_status, cd_education_status, COUNT(*) cnt1 FROM customer c, customer_address ca, customer_demographics WHERE c.c_current_addr_sk = ca.ca_address_sk AND ca_state IN ('KY', 'GA', 'NM', 'TN', 'AL') AND cd_demo_sk = c.c_current_cdemo_sk AND EXISTS (SELECT * FROM store_sales, date_dim WHERE c.c_customer_sk = ss_customer_sk AND ss_sold_date_sk = d_date_sk AND d_year = 2001 AND d_moy BETWEEN 4 AND 6) AND NOT EXISTS (SELECT * FROM web_sales, date_dim WHERE c.c_customer_sk = ws_bill_customer_sk AND ws_sold_date_sk = d_date_sk AND d_year = 2001 AND d_moy BETWEEN 4 AND 6) AND NOT EXISTS (SELECT * FROM catalog_sales, date_dim WHERE c.c_customer_sk = cs_bill_customer_sk AND cs_sold_date_sk = d_date_sk AND d_year = 2001 AND d_moy BETWEEN 4 AND 6) GROUP BY cd_gender, cd_marital_status, cd_education_status ORDER BY cd_gender, cd_marital_status, cd_education_status"),
        ("Q87 store-only buyers (EXCEPT)", "SELECT COUNT(*) FROM ((SELECT DISTINCT c_last_name, c_first_name, d_date FROM store_sales, date_dim, customer WHERE ss_sold_date_sk = d_date_sk AND ss_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11) EXCEPT (SELECT DISTINCT c_last_name, c_first_name, d_date FROM catalog_sales, date_dim, customer WHERE cs_sold_date_sk = d_date_sk AND cs_bill_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11) EXCEPT (SELECT DISTINCT c_last_name, c_first_name, d_date FROM web_sales, date_dim, customer WHERE ws_sold_date_sk = d_date_sk AND ws_bill_customer_sk = c_customer_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11)) cool_cust"),
        ("Q96 evening shoppers with 7 dependents", "SELECT COUNT(*) FROM store_sales, household_demographics, time_dim, store WHERE ss_sold_time_sk = t_time_sk AND ss_hdemo_sk = hd_demo_sk AND ss_store_sk = s_store_sk AND t_hour = 20 AND t_minute >= 30 AND hd_dep_count = 7 AND s_store_name = 'ese'"),
        ("Q98 store revenue ratio per class", "SELECT i_item_id, i_category, i_class, i_current_price, SUM(ss_ext_sales_price) AS itemrevenue, SUM(ss_ext_sales_price) * 100.0 / SUM(SUM(ss_ext_sales_price)) OVER (PARTITION BY i_class) AS revenueratio FROM store_sales, item, date_dim WHERE ss_item_sk = i_item_sk AND i_category IN ('Sports', 'Books', 'Home') AND ss_sold_date_sk = d_date_sk AND d_date BETWEEN '1999-02-22' AND '1999-03-24' GROUP BY i_item_id, i_category, i_class, i_current_price ORDER BY i_category, i_class, i_item_id, revenueratio"),
        ("Q11 year-over-year growth by channel", "WITH year_total AS (SELECT c_customer_id customer_id, d_year dyear, SUM(ss_ext_sales_price) year_total, 's' sale_type FROM customer, store_sales, date_dim WHERE c_customer_sk = ss_customer_sk AND ss_sold_date_sk = d_date_sk GROUP BY c_customer_id, d_year UNION ALL SELECT c_customer_id, d_year, SUM(ws_ext_sales_price), 'w' FROM customer, web_sales, date_dim WHERE c_customer_sk = ws_bill_customer_sk AND ws_sold_date_sk = d_date_sk GROUP BY c_customer_id, d_year) SELECT TOP 100 t_s_secyear.customer_id FROM year_total t_s_firstyear, year_total t_s_secyear, year_total t_w_firstyear, year_total t_w_secyear WHERE t_s_secyear.customer_id = t_s_firstyear.customer_id AND t_s_firstyear.customer_id = t_w_secyear.customer_id AND t_s_firstyear.customer_id = t_w_firstyear.customer_id AND t_s_firstyear.sale_type = 's' AND t_w_firstyear.sale_type = 'w' AND t_s_secyear.sale_type = 's' AND t_w_secyear.sale_type = 'w' AND t_s_firstyear.dyear = 2001 AND t_s_secyear.dyear = 2002 AND t_w_firstyear.dyear = 2001 AND t_w_secyear.dyear = 2002 AND t_s_firstyear.year_total > 0 AND t_w_firstyear.year_total > 0 AND t_w_secyear.year_total / t_w_firstyear.year_total > t_s_secyear.year_total / t_s_firstyear.year_total ORDER BY t_s_secyear.customer_id"),
        ("Q51 cumulative sales window", "SELECT TOP 100 item_sk, d_date, daily, SUM(daily) OVER (PARTITION BY item_sk ORDER BY d_date ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) cume_sales FROM (SELECT ss_item_sk item_sk, d_date, SUM(ss_sales_price) daily FROM store_sales, date_dim WHERE ss_sold_date_sk = d_date_sk AND d_month_seq BETWEEN 1200 AND 1200 + 11 AND ss_item_sk <= 20 GROUP BY ss_item_sk, d_date) x ORDER BY item_sk, d_date"),
    ];
    v.into_iter().map(|(n, s)| (n.to_string(), s.to_string())).collect()
}

/// Recompute a few answers from the generated rows.
pub fn check(data: &Data, name: &str, rows: &[Vec<String>]) -> Option<Result<(), String>> {
    match name.split(' ').next().unwrap() {
        "Q3" => {
            let mut g: HashMap<(i32, i64, String), i64> = HashMap::new();
            for s in &data.sales {
                let it = &data.items[&s.item];
                let (y, m) = data.dates[&s.date];
                if it.manufact == 128 && m == 11 {
                    *g.entry((y, it.brand_id, it.brand.clone())).or_default() += s.ext;
                }
            }
            let shown = rows.len().min(100);
            if shown != g.len().min(100) {
                return Some(Err(format!("{} rows, expected {}", rows.len(), g.len().min(100))));
            }
            for r in rows {
                let key = (num(&r[0]) as i32, num(&r[1]) as i64, r[2].trim().to_string());
                match g.get(&key) {
                    Some(v) if close(num(&r[3]), *v as f64 / 100.0) => {}
                    other => return Some(Err(format!("row {:?}: expected {:?}", r, other))),
                }
            }
            Some(Ok(()))
        }
        "Q42" => {
            let mut g: HashMap<(i32, i64, String), i64> = HashMap::new();
            for s in &data.sales {
                let it = &data.items[&s.item];
                let (y, m) = data.dates[&s.date];
                if it.manager == 1 && m == 11 && y == 2000 {
                    *g.entry((y, it.cat_id, it.cat.clone())).or_default() += s.ext;
                }
            }
            if rows.len() != g.len().min(100) {
                return Some(Err(format!("{} rows, expected {}", rows.len(), g.len().min(100))));
            }
            for r in rows {
                let key = (num(&r[0]) as i32, num(&r[1]) as i64, r[2].trim().to_string());
                match g.get(&key) {
                    Some(v) if close(num(&r[3]), *v as f64 / 100.0) => {}
                    other => return Some(Err(format!("row {:?}: expected {:?}", r, other))),
                }
            }
            Some(Ok(()))
        }
        "Q96" => {
            let n = data.sales.iter().filter(|s| s.time / 60 == 20 && s.time % 60 >= 30 && data.hd_dep[&s.hdemo] == 7 && data.store_names[&s.store] == "ese").count();
            if rows.len() == 1 && num(&rows[0][0]) as usize == n {
                Some(Ok(()))
            } else {
                Some(Err(format!("count {:?} expected {}", rows, n)))
            }
        }
        _ => {
            let _ = |s: &Sale| s.qty;
            None
        }
    }
}
