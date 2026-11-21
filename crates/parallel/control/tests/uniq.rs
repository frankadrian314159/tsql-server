//! Unique keys that do not contain the shard key: the control node checks them across the shards.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    _shards: Vec<RunningShard>,
    cluster: Arc<Cluster>,
}

fn start(n: usize) -> Harness {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    Harness { _rt: rt, _shards: shards, cluster }
}

fn ok(r: &BatchResult, what: &str) {
    assert!(r.first_error().is_none(), "{}: {:?}", what, r.first_error());
}

fn run(s: &mut ClusterSession, sql: &str) {
    ok(&s.execute(sql), sql);
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    let r = s.execute(sql);
    ok(&r, sql);
    r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

fn one(s: &mut ClusterSession, sql: &str) -> String {
    rows(s, sql).first().and_then(|r| r.first()).cloned().unwrap_or_default()
}

fn error(s: &mut ClusterSession, sql: &str) -> i32 {
    s.execute(sql).first_error().map(|e| e.number).unwrap_or(0)
}

/// ids 1..=8 land on different shards; the e-mail is unique but is not the shard key.
fn people(s: &mut ClusterSession) {
    run(s, "CREATE TABLE people (id INT PRIMARY KEY, email VARCHAR(40), grp INT, CONSTRAINT uq_email UNIQUE (email))");
    run(s, "INSERT people VALUES (1,'a@x',1),(2,'b@x',1),(3,'c@x',2),(4,'d@x',2),(5,'e@x',3),(6,'f@x',3),(7,'g@x',4),(8,'h@x',4)");
}

#[test]
fn an_insert_of_a_value_another_shard_holds_is_refused() {
    let h = start(3);
    let mut s = h.cluster.session();
    people(&mut s);
    // 'a@x' is on the shard of id 1; these ids hash to all the shards
    for id in 100..110 {
        assert_eq!(error(&mut s, &format!("INSERT people VALUES ({}, 'a@x', 9)", id)), 2627, "id {}", id);
    }
    // the same value, case apart and with trailing spaces, is the same value
    assert_eq!(error(&mut s, "INSERT people VALUES (120, 'A@X  ', 9)"), 2627);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM people"), "8");
    run(&mut s, "INSERT people VALUES (121, 'new@x', 9)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM people"), "9");
}

#[test]
fn rows_of_one_statement_that_repeat_a_value_are_refused() {
    let h = start(3);
    let mut s = h.cluster.session();
    people(&mut s);
    assert_eq!(error(&mut s, "INSERT people VALUES (50,'z@x',1),(51,'z@x',1)"), 2627);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM people"), "8");
    // NULL is one value too: the first is fine, a second is a duplicate
    run(&mut s, "INSERT people VALUES (60, NULL, 1)");
    assert_eq!(error(&mut s, "INSERT people VALUES (61, NULL, 1)"), 2627);
}

#[test]
fn an_update_may_not_take_a_value_another_row_has_but_may_swap_its_own() {
    let h = start(3);
    let mut s = h.cluster.session();
    people(&mut s);
    assert_eq!(error(&mut s, "UPDATE people SET email = 'h@x' WHERE id = 1"), 2627);
    assert_eq!(one(&mut s, "SELECT email FROM people WHERE id = 1"), "a@x");
    // two rows given the same value
    assert_eq!(error(&mut s, "UPDATE people SET email = 'same' WHERE grp = 1"), 2627);
    // a row keeps its own value, and the rows of a statement may shuffle values among themselves
    run(&mut s, "UPDATE people SET email = email WHERE id = 3");
    run(&mut s, "UPDATE people SET email = CASE id WHEN 1 THEN 'b@x' ELSE 'a@x' END WHERE id IN (1, 2)");
    assert_eq!(rows(&mut s, "SELECT id, email FROM people WHERE id IN (1,2) ORDER BY id"), vec![vec!["1".to_string(), "b@x".into()], vec!["2".into(), "a@x".into()]]);
    // a column that is not part of the key is not looked at
    run(&mut s, "UPDATE people SET grp = 7");
    // a free value is taken
    run(&mut s, "UPDATE people SET email = 'free@x' WHERE id = 4");
    assert_eq!(one(&mut s, "SELECT email FROM people WHERE id = 4"), "free@x");
    // ... and the value it left is free
    run(&mut s, "INSERT people VALUES (70, 'd@x', 1)");
}

#[test]
fn a_delete_frees_the_value_and_a_rolled_back_insert_leaves_nothing() {
    let h = start(3);
    let mut s = h.cluster.session();
    people(&mut s);
    run(&mut s, "DELETE people WHERE id = 5");
    run(&mut s, "INSERT people VALUES (80, 'e@x', 1)");
    run(&mut s, "BEGIN TRAN");
    run(&mut s, "INSERT people VALUES (81, 'tx@x', 1)");
    // inside the transaction the value is taken
    assert_eq!(error(&mut s, "INSERT people VALUES (82, 'tx@x', 1)"), 2627);
    run(&mut s, "ROLLBACK");
    run(&mut s, "INSERT people VALUES (83, 'tx@x', 1)");
}

#[test]
fn a_composite_key_and_a_unique_index_made_later() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, a INT, b VARCHAR(10))");
    run(&mut s, "INSERT t VALUES (1,1,'x'),(2,1,'y'),(3,2,'x'),(4,2,NULL)");
    // the rows there are looked at when the index is made
    run(&mut s, "INSERT t VALUES (5,1,'x')");
    assert_eq!(error(&mut s, "CREATE UNIQUE INDEX ux_ab ON t (a, b)"), 1505);
    run(&mut s, "DELETE t WHERE id = 5");
    run(&mut s, "CREATE UNIQUE INDEX ux_ab ON t (a, b)");
    // the pair is the key
    run(&mut s, "INSERT t VALUES (6,3,'x')");
    assert_eq!(error(&mut s, "INSERT t VALUES (7,1,'x')"), 2601);
    assert_eq!(error(&mut s, "INSERT t VALUES (8,2,NULL)"), 2601);
    // dropping it frees the pairs
    run(&mut s, "DROP INDEX ux_ab ON t");
    run(&mut s, "INSERT t VALUES (9,1,'x')");
    // ADD CONSTRAINT checks the rows too
    assert_eq!(error(&mut s, "ALTER TABLE t ADD CONSTRAINT uq_a UNIQUE (a)"), 1505);
    run(&mut s, "CREATE TABLE u (id INT PRIMARY KEY, c INT)");
    run(&mut s, "INSERT u VALUES (1, 10), (2, 20)");
    run(&mut s, "ALTER TABLE u ADD CONSTRAINT uq_c UNIQUE (c)");
    assert_eq!(error(&mut s, "INSERT u VALUES (3, 10)"), 2627);
    run(&mut s, "ALTER TABLE u DROP CONSTRAINT uq_c");
    run(&mut s, "INSERT u VALUES (3, 10)");
    // a filtered unique index without the shard key is refused
    assert_eq!(error(&mut s, "CREATE UNIQUE INDEX uf ON u (c) WHERE c > 5"), 50100);
}

#[test]
fn a_key_with_the_shard_key_is_the_shards_and_a_moved_key_is_checked_as_an_insert() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE k (id INT, v INT, CONSTRAINT uq_idv UNIQUE (id, v))");
    run(&mut s, "INSERT k VALUES (1, 1)");
    assert_eq!(error(&mut s, "INSERT k VALUES (1, 1)"), 2627);
    // the shard key moves rows between shards: the check is made on the rows as the delete leaves them
    people(&mut s);
    run(&mut s, "UPDATE people SET id = id + 1000 WHERE id < 4");
    assert_eq!(error(&mut s, "UPDATE people SET id = 2001, email = 'h@x' WHERE id = 1001"), 2627);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM people"), "8");
}

#[test]
fn two_sessions_cannot_both_take_the_same_free_value() {
    let h = start(3);
    let mut s1 = h.cluster.session();
    people(&mut s1);
    let mut handles = vec![];
    for w in 0..6 {
        let c = h.cluster.clone();
        handles.push(std::thread::spawn(move || {
            let mut s = c.session();
            let mut wins = 0;
            for i in 0..8 {
                // every worker tries the same eight values, with ids of its own (so that they land on different shards)
                let r = s.execute(&format!("INSERT people VALUES ({}, 'race{}@x', 1)", 1000 + w * 100 + i, i));
                if r.first_error().is_none() {
                    wins += 1;
                }
            }
            wins
        }));
    }
    let total: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 8, "each value is taken once");
    assert_eq!(one(&mut s1, "SELECT COUNT(DISTINCT email) FROM people WHERE email LIKE 'race%'"), "8");
    assert_eq!(one(&mut s1, "SELECT COUNT(*) FROM people WHERE email LIKE 'race%'"), "8");
}

#[test]
fn a_primary_key_added_later_is_kept_with_or_without_the_shard_key() {
    let h = start(3);
    let mut s = h.cluster.session();
    // (what Reporting Services does: make the table, then add its key)
    run(&mut s, "CREATE TABLE cache (a INT NOT NULL, b VARCHAR(10) NOT NULL, v INT NULL)");
    run(&mut s, "INSERT cache VALUES (1,'x',1),(2,'y',2),(3,'z',3),(4,'w',4)");
    // a key without the shard key (the first column) is checked across the shards
    run(&mut s, "ALTER TABLE cache ADD CONSTRAINT pk_cache PRIMARY KEY NONCLUSTERED (b)");
    assert_eq!(error(&mut s, "INSERT cache VALUES (9,'x',9)"), 2627);
    run(&mut s, "INSERT cache VALUES (9,'q',9)");
    // a key that has duplicates among the rows is refused when it is added
    run(&mut s, "CREATE TABLE dup (a INT NOT NULL, b INT NOT NULL)");
    run(&mut s, "INSERT dup VALUES (1,1),(2,1),(3,2)");
    assert_eq!(error(&mut s, "ALTER TABLE dup ADD CONSTRAINT pk_dup PRIMARY KEY (b)"), 1505);
    // a key with the shard key is the shards' to keep
    run(&mut s, "ALTER TABLE dup ADD CONSTRAINT pk_dup2 PRIMARY KEY (a)");
    assert_eq!(error(&mut s, "INSERT dup VALUES (1, 5)"), 2627);
}
