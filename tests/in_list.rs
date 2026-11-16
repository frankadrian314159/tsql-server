//! Long `IN` lists of integer literals are tested through a hash set; the answers (NULL rules, NOT IN, mixed types) must be those of the
//! plain comparison with each item.
use tsql_server::Server;

fn col(c: &mut tsql_server::Connection, sql: &str) -> Vec<String> {
    let r = c.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
    r.resultsets()[0].rows.iter().map(|row| row[0].to_string()).collect()
}

#[test]
fn long_integer_lists_give_the_same_answers_as_short_ones() {
    let server = Server::new();
    let mut c = server.connect();
    assert!(c.execute("CREATE TABLE t (id INT, big BIGINT NULL, s VARCHAR(10) NULL)").first_error().is_none());
    let vals: Vec<String> = (1..=60).map(|i| format!("({}, {}, '{}')", i, if i % 10 == 0 { "NULL".to_string() } else { (i as i64 * 5_000_000_000).to_string() }, i)).collect();
    assert!(c.execute(&format!("INSERT t VALUES {}", vals.join(","))).first_error().is_none());
    let even: Vec<String> = (0..40).map(|i| (i * 2).to_string()).collect();
    let list = even.join(",");
    // the long list (40 items) against the same test written as a chain of ORs
    let ors = even.iter().map(|v| format!("id = {}", v)).collect::<Vec<_>>().join(" OR ");
    let a = col(&mut c, &format!("SELECT id FROM t WHERE id IN ({}) ORDER BY id", list));
    let b = col(&mut c, &format!("SELECT id FROM t WHERE {} ORDER BY id", ors));
    assert_eq!(a, b);
    assert_eq!(a.len(), 30);
    let n = col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE id NOT IN ({})", list));
    assert_eq!(n, vec!["30"]);
    // a bigint column against a list that mixes sizes
    let big: Vec<String> = (1..=20).map(|i| (i as i64 * 5_000_000_000).to_string()).collect();
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE big IN ({})", big.join(","))), vec!["18"]);
    // NULL in the list: IN still finds, NOT IN finds nothing (unknown); a NULL probe matches nothing either way
    let with_null = format!("{},NULL", list);
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE id IN ({})", with_null)), vec!["30"]);
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE id NOT IN ({})", with_null)), vec!["0"]);
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE big NOT IN ({})", big.join(","))), vec!["36"]);
    // a string column against integers still compares as before (converted), and a decimal literal in the list is not a hash hit
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE s IN ({})", list)), vec!["30"]);
    let mixed = format!("{},2.5", list);
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t WHERE id IN ({})", mixed)), vec!["30"]);
    // the same list in a statement that runs more than once (a procedure called twice) and in a join
    assert!(c.execute(&format!("CREATE PROCEDURE p AS SELECT COUNT(*) FROM t WHERE id IN ({})", list)).first_error().is_none());
    assert_eq!(col(&mut c, "EXEC p"), vec!["30"]);
    assert_eq!(col(&mut c, "EXEC p"), vec!["30"]);
    assert_eq!(col(&mut c, &format!("SELECT COUNT(*) FROM t a JOIN t b ON a.id = b.id WHERE a.id IN ({}) AND b.id NOT IN (2,4,6)", list)), vec!["27"]);
}
