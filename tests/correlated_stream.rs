//! A correlated subquery over a table big enough for the streaming executor (20,000 slots) is still run per outer row as an index seek.
//! It used to scan the whole table for each outer row: TPC-C's consistency condition 6 (3,000 orders over 30,000 order lines) did not
//! finish in minutes.
mod common;
use common::*;
use std::time::Instant;

#[test]
fn a_correlated_count_over_a_big_table_seeks_its_index_for_each_outer_row() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE orders (o_w_id INT, o_d_id INT, o_id INT, o_ol_cnt INT, PRIMARY KEY (o_w_id, o_d_id, o_id))");
    exec(&mut c, "CREATE TABLE order_line (ol_w_id INT, ol_d_id INT, ol_o_id INT, ol_number INT, ol_amount INT, PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number))");
    let (mut o, mut l) = (vec![], vec![]);
    for i in 0..3000usize {
        let (d, id) = (i % 10 + 1, i / 10 + 1);
        // (the order of 777 says it has 11 lines and has 10)
        o.push(format!("(1, {}, {}, {})", d, id, if i == 777 { 11 } else { 10 }));
        for k in 1..=10 {
            l.push(format!("(1, {}, {}, {}, 5)", d, id, k));
        }
    }
    for ch in o.chunks(500) {
        exec(&mut c, &format!("INSERT INTO orders VALUES {}", ch.join(",")));
    }
    for ch in l.chunks(500) {
        exec(&mut c, &format!("INSERT INTO order_line VALUES {}", ch.join(",")));
    }
    let t = Instant::now();
    let r = scalar(&mut c, "SELECT COUNT(*) FROM orders o WHERE o_ol_cnt <> (SELECT COUNT(*) FROM order_line WHERE ol_w_id = o.o_w_id AND ol_d_id = o.o_d_id AND ol_o_id = o.o_id)");
    assert_eq!(r, "1");
    // (a tenth of a second here; the scan of the whole table for each of the 3,000 rows took more than half a minute)
    assert!(t.elapsed().as_secs_f64() < 8.0, "took {:?}", t.elapsed());
}
