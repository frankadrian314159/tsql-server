//! Dynamic data masking.
mod common;
use common::*;
use tsql_server::{Connection, Server};

fn setup() -> (Server, Connection) {
    let s = server();
    let mut sa = s.connect();
    script(
        &mut sa,
        "CREATE TABLE person (
            id INT PRIMARY KEY,
            name VARCHAR(30) MASKED WITH (FUNCTION = 'default()'),
            email VARCHAR(40) MASKED WITH (FUNCTION = 'email()'),
            phone VARCHAR(20) MASKED WITH (FUNCTION = 'partial(2, \"XXX-XXX\", 2)'),
            salary INT MASKED WITH (FUNCTION = 'default()'),
            score INT MASKED WITH (FUNCTION = 'random(1, 5)'),
            born DATE MASKED WITH (FUNCTION = 'default()'),
            note VARCHAR(30))
         INSERT person VALUES (1, 'Alice Jones', 'alice@example.com', '5551234567', 5000, 90, '1990-04-05', 'plain')
         INSERT person VALUES (2, 'Bob Smith', NULL, '12', 7000, 80, '1985-01-01', NULL)
         CREATE LOGIN reader WITH PASSWORD = 'pw'
         CREATE USER reader FOR LOGIN reader
         GRANT SELECT ON person TO reader",
    );
    (s, sa)
}

fn reader(s: &Server) -> Connection {
    let mut c = s.connect_as("reader", "pw").unwrap();
    exec(&mut c, "USE master");
    c
}

#[test]
fn masked_columns_are_disguised_for_users_without_unmask() {
    let (s, mut sa) = setup();
    let mut r = reader(&s);
    let row = &rows(&mut r, "SELECT name, email, phone, salary, born, note FROM person WHERE id = 1")[0];
    assert_eq!(row, &["XXXX", "aXXX@XXXX.com", "55XXX-XXX67", "0", "1900-01-01", "plain"]);
    let row2 = &rows(&mut r, "SELECT email, phone FROM person WHERE id = 2")[0];
    assert_eq!(row2, &["NULL", "XXX-XXX"], "NULL stays NULL; a value shorter than prefix+suffix shows only the padding");
    let score: i32 = scalar(&mut r, "SELECT score FROM person WHERE id = 1").parse().unwrap();
    assert!((1..=5).contains(&score));
    // sysadmin sees the real data
    assert_eq!(rows(&mut sa, "SELECT name, salary FROM person WHERE id = 1"), [["Alice Jones", "5000"]]);
    // SELECT * and column aliases are masked too
    let star = &rows(&mut r, "SELECT * FROM person WHERE id = 1")[0];
    assert_eq!(star[1], "XXXX");
    assert_eq!(scalar(&mut r, "SELECT name AS who FROM person WHERE id = 1"), "XXXX");
}

#[test]
fn unmask_permission_and_roles() {
    let (s, mut sa) = setup();
    let mut r = reader(&s);
    exec(&mut sa, "GRANT UNMASK TO reader");
    assert_eq!(scalar(&mut r, "SELECT name FROM person WHERE id = 1"), "Alice Jones");
    exec(&mut sa, "REVOKE UNMASK FROM reader");
    assert_eq!(scalar(&mut r, "SELECT name FROM person WHERE id = 1"), "XXXX");
    exec(&mut sa, "ALTER ROLE db_owner ADD MEMBER reader");
    assert_eq!(scalar(&mut r, "SELECT name FROM person WHERE id = 1"), "Alice Jones", "db_owner implies UNMASK");
}

#[test]
fn masking_is_applied_to_output_not_to_stored_data_or_predicates() {
    let (s, mut sa) = setup();
    let mut r = reader(&s);
    // a filter still sees the real value (as in SQL Server) while the output is masked
    assert_eq!(rows(&mut r, "SELECT id, salary FROM person WHERE salary > 6000"), [["2", "0"]]);
    // views and derived tables return the masked values to the caller
    exec(&mut sa, "CREATE VIEW pv AS SELECT id, name FROM person; GRANT SELECT ON pv TO reader");
    assert_eq!(scalar(&mut r, "SELECT name FROM pv WHERE id = 1"), "XXXX");
    assert_eq!(scalar(&mut sa, "SELECT name FROM pv WHERE id = 1"), "Alice Jones");
    // data copied by a masked reader is the masked form (SELECT INTO into a scratch table)
    exec(&mut sa, "GRANT CREATE TABLE TO reader; GRANT ALTER ON SCHEMA::dbo TO reader");
    exec(&mut r, "SELECT id, name INTO copy1 FROM person");
    assert_eq!(scalar(&mut sa, "SELECT name FROM copy1 WHERE id = 1"), "XXXX");
    // the stored data is untouched
    assert_eq!(scalar(&mut sa, "SELECT name FROM person WHERE id = 1"), "Alice Jones");
}

#[test]
fn masks_are_managed_with_alter_table_and_listed_in_the_catalog() {
    let (s, mut sa) = setup();
    let mut r = reader(&s);
    assert_eq!(scalar(&mut r, "SELECT note FROM person WHERE id = 1"), "plain");
    exec(&mut sa, "ALTER TABLE person ALTER COLUMN note ADD MASKED WITH (FUNCTION = 'partial(1, \"***\", 0)')");
    assert_eq!(scalar(&mut r, "SELECT note FROM person WHERE id = 1"), "p***");
    exec(&mut sa, "ALTER TABLE person ALTER COLUMN name DROP MASKED");
    assert_eq!(scalar(&mut r, "SELECT name FROM person WHERE id = 1"), "Alice Jones");
    let masked = rows(&mut sa, "SELECT name, masking_function FROM sys.masked_columns ORDER BY column_id");
    assert!(masked.iter().any(|m| m[0] == "email" && m[1] == "email()"));
    assert!(!masked.iter().any(|m| m[0] == "name"));
    // masks must fit the type, and be valid
    assert_eq!(err(&mut sa, "ALTER TABLE person ALTER COLUMN salary ADD MASKED WITH (FUNCTION = 'email()')").number, 16003);
    assert_eq!(err(&mut sa, "ALTER TABLE person ALTER COLUMN name ADD MASKED WITH (FUNCTION = 'random(1,2)')").number, 16003);
    assert_eq!(err(&mut sa, "ALTER TABLE person ALTER COLUMN name ADD MASKED WITH (FUNCTION = 'nonsense()')").number, 16000);
    assert_eq!(err(&mut sa, "CREATE TABLE bad (x INT MASKED WITH (FUNCTION = 'partial(1, \"x\", 1)'))").number, 16003);
}

#[test]
fn masks_survive_a_restart_and_apply_in_the_streaming_executor() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path().join("d")).unwrap();
        let mut sa = s.connect();
        script(&mut sa, "CREATE TABLE m (id INT PRIMARY KEY, secret VARCHAR(20) MASKED WITH (FUNCTION = 'default()')); INSERT m VALUES (1, 'top'), (2, 'bottom'); CREATE LOGIN u WITH PASSWORD = 'p'; CREATE USER u FOR LOGIN u; GRANT SELECT ON m TO u;");
    }
    let s = Server::open(dir.path().join("d")).unwrap();
    s.set_work_mem(2048, 0, None); // every query below runs in the streaming executor
    let mut u = s.connect_as("u", "p").unwrap();
    assert_eq!(rows(&mut u, "SELECT secret FROM m ORDER BY secret DESC"), [["XXXX"], ["XXXX"]]);
    assert!(u.last_plan().join(" ").contains("streaming"), "{:?}", u.last_plan());
    assert_eq!(rows(&mut u, "SELECT DISTINCT secret FROM m"), [["XXXX"]]);
    let mut sa = s.connect();
    assert_eq!(rows(&mut sa, "SELECT secret FROM m ORDER BY id"), [["top"], ["bottom"]]);
}
