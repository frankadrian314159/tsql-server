//! A long literal IN list against the same condition as a join to a VALUES table, on one engine.
use std::time::Instant;
use tsql_server::Server;

fn main() {
    let server = Server::new();
    let mut c = server.connect();
    assert!(c.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, v INT)").first_error().is_none());
    for chunk in (0..150_000).collect::<Vec<i32>>().chunks(1000) {
        let rows = chunk.iter().map(|i| format!("({}, {}, {})", i, (i * 7919) % 20_000, i % 13)).collect::<Vec<_>>().join(",");
        assert!(c.execute(&format!("INSERT t VALUES {}", rows)).first_error().is_none());
    }
    let time = |c: &mut tsql_server::Connection, sql: &str| {
        let t = Instant::now();
        let r = c.execute(sql);
        assert!(r.first_error().is_none(), "{:?}", r.first_error());
        (t.elapsed().as_secs_f64(), r.resultsets()[0].rows[0][0].to_string())
    };
    println!("{:>6} {:>12} {:>12} {:>12} {:>12} {:>12}", "values", "IN list", "JOIN VALUES", "EXISTS VALUES", "NOT IN list", "LEFT JOIN anti");
    for n in [10usize, 25, 100, 500, 2000, 8000] {
        let list = (0..n).map(|i| (i * 7).to_string()).collect::<Vec<_>>();
        let a = time(&mut c, &format!("SELECT COUNT(*) FROM t WHERE k IN ({})", list.join(",")));
        let rows = list.iter().map(|x| format!("({})", x)).collect::<Vec<_>>().join(",");
        let b = time(&mut c, &format!("SELECT COUNT(*) FROM t JOIN (VALUES {}) AS l(x) ON t.k = l.x", rows));
        let e = time(&mut c, &format!("SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM (VALUES {}) AS l(x) WHERE l.x = t.k)", rows));
        let d = time(&mut c, &format!("SELECT COUNT(*) FROM t WHERE k NOT IN ({})", list.join(",")));
        let l = time(&mut c, &format!("SELECT COUNT(*) FROM t LEFT JOIN (VALUES {}) AS l(x) ON t.k = l.x WHERE l.x IS NULL", rows));
        assert_eq!(d.1, l.1);
        assert_eq!(a.1, b.1);
        assert_eq!(a.1, e.1);
        println!("{:>6} {:>10.3}s {:>10.3}s {:>10.3}s {:>10.3}s {:>10.3}s  (matches {})", n, a.0, b.0, e.0, d.0, l.0, a.1);
    }
}
