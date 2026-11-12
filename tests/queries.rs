mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE dept (id INT PRIMARY KEY, name VARCHAR(20))
         CREATE TABLE emp (id INT PRIMARY KEY, name VARCHAR(20), dept_id INT NULL, salary INT, boss INT NULL)
         INSERT dept VALUES (1,'Eng'),(2,'Ops'),(3,'Empty')
         INSERT emp VALUES (1,'Alice',1,100,NULL),(2,'Bob',1,80,1),(3,'Carol',2,70,1),(4,'Dan',2,70,3),(5,'Eve',NULL,50,3)",
    );
}

#[test]
fn select_basics() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE salary > 70 ORDER BY name"), vec![vec!["Alice"], vec!["Bob"]]);
    assert_eq!(rows(&mut c, "SELECT name, salary * 2 AS dbl FROM emp WHERE id = 2"), vec![vec!["Bob", "160"]]);
    assert_eq!(rows(&mut c, "SELECT DISTINCT salary FROM emp ORDER BY salary DESC"), vec![vec!["100"], vec!["80"], vec!["70"], vec!["50"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp WHERE dept_id IS NULL"), vec![vec!["1"]]);
    assert_eq!(rows(&mut c, "SELECT 1 AS a, 'x' AS b"), vec![vec!["1", "x"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE name LIKE '%a%' ORDER BY id"), vec![vec!["Alice"], vec!["Carol"], vec!["Dan"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE id IN (1, 3, 99) ORDER BY id"), vec![vec!["Alice"], vec!["Carol"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE salary BETWEEN 70 AND 80 ORDER BY id"), vec![vec!["Bob"], vec!["Carol"], vec!["Dan"]]);
}

#[test]
fn order_top_offset() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT TOP 2 name FROM emp ORDER BY salary DESC, name"), vec![vec!["Alice"], vec!["Bob"]]);
    assert_eq!(rows(&mut c, "SELECT TOP 2 WITH TIES name FROM emp ORDER BY salary DESC, name"), vec![vec!["Alice"], vec!["Bob"]]);
    assert_eq!(rows(&mut c, "SELECT TOP 3 WITH TIES salary FROM emp ORDER BY salary DESC"), vec![vec!["100"], vec!["80"], vec!["70"], vec!["70"]]);
    assert_eq!(rows(&mut c, "SELECT TOP 40 PERCENT name FROM emp ORDER BY id"), vec![vec!["Alice"], vec!["Bob"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp ORDER BY id OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY"), vec![vec!["Bob"], vec!["Carol"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp ORDER BY 1 DESC"), vec![vec!["Eve"], vec!["Dan"], vec!["Carol"], vec!["Bob"], vec!["Alice"]]);
    assert_eq!(rows(&mut c, "SELECT name AS n FROM emp ORDER BY n OFFSET 4 ROWS"), vec![vec!["Eve"]]);
    // NULLs sort first ascending
    assert_eq!(rows(&mut c, "SELECT dept_id FROM emp ORDER BY dept_id, id")[0], vec!["NULL"]);
}

#[test]
fn joins() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT e.name, d.name FROM emp e INNER JOIN dept d ON e.dept_id = d.id ORDER BY e.id").len(), 4);
    assert_eq!(rows(&mut c, "SELECT e.name, d.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id WHERE e.id = 5"), vec![vec!["Eve", "NULL"]]);
    assert_eq!(rows(&mut c, "SELECT d.name, e.name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id WHERE d.id = 3"), vec![vec!["Empty", "NULL"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp e FULL JOIN dept d ON e.dept_id = d.id"), vec![vec!["6"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp CROSS JOIN dept"), vec![vec!["15"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp, dept"), vec![vec!["15"]]);
    // self join
    assert_eq!(rows(&mut c, "SELECT e.name, b.name FROM emp e JOIN emp b ON e.boss = b.id WHERE e.id = 4"), vec![vec!["Dan", "Carol"]]);
    // non-equi + equi
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp a JOIN emp b ON a.dept_id = b.dept_id AND a.salary > b.salary"), vec![vec!["1"]]);
    // join with null keys never matches
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp a JOIN emp b ON a.dept_id = b.dept_id AND a.id < b.id"), vec![vec!["2"]]);
}

#[test]
fn apply_operators() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT d.name, x.name FROM dept d CROSS APPLY (SELECT TOP 1 name FROM emp e WHERE e.dept_id = d.id ORDER BY salary DESC) x ORDER BY d.id");
    assert_eq!(r, vec![vec!["Eng", "Alice"], vec!["Ops", "Carol"]]);
    let r = rows(&mut c, "SELECT d.name, x.name FROM dept d OUTER APPLY (SELECT TOP 1 name FROM emp e WHERE e.dept_id = d.id ORDER BY salary DESC) x ORDER BY d.id");
    assert_eq!(r.len(), 3);
    assert_eq!(r[2], vec!["Empty", "NULL"]);
}

#[test]
fn group_by_having() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT dept_id, COUNT(*) c, SUM(salary) s, MIN(salary) mn, MAX(salary) mx FROM emp WHERE dept_id IS NOT NULL GROUP BY dept_id ORDER BY dept_id"), vec![vec!["1", "2", "180", "80", "100"], vec!["2", "2", "140", "70", "70"]]);
    assert_eq!(rows(&mut c, "SELECT dept_id FROM emp GROUP BY dept_id HAVING COUNT(*) > 1 ORDER BY dept_id"), vec![vec!["1"], vec!["2"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*), COUNT(dept_id), COUNT(DISTINCT dept_id) FROM emp"), vec![vec!["5", "4", "2"]]);
    assert_eq!(rows(&mut c, "SELECT AVG(salary) FROM emp"), vec![vec!["74"]]);
    assert_eq!(rows(&mut c, "SELECT AVG(CAST(salary AS decimal(10,2))) FROM emp"), vec![vec!["74.00"]]);
    assert_eq!(rows(&mut c, "SELECT SUM(salary) FROM emp WHERE 1=0"), vec![vec!["NULL"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp WHERE 1=0"), vec![vec!["0"]]);
    assert_eq!(rows(&mut c, "SELECT dept_id, COUNT(*) FROM emp WHERE 1=0 GROUP BY dept_id"), Vec::<Vec<String>>::new());
    assert_eq!(rows(&mut c, "SELECT STDEV(salary), VAR(salary) FROM emp WHERE dept_id = 1")[0][0], "14.142135623730951");
    assert_eq!(rows(&mut c, "SELECT STRING_AGG(name, ', ') WITHIN GROUP (ORDER BY name DESC) FROM emp WHERE dept_id = 1"), vec![vec!["Bob, Alice"]]);
}

#[test]
fn rollup_cube_grouping_sets() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT dept_id, COUNT(*) FROM emp WHERE dept_id IS NOT NULL GROUP BY ROLLUP(dept_id) ORDER BY GROUPING(dept_id), dept_id");
    assert_eq!(r, vec![vec!["1", "2"], vec!["2", "2"], vec!["NULL", "4"]]);
    let r = rows(&mut c, "SELECT dept_id, boss, COUNT(*) FROM emp WHERE dept_id IS NOT NULL GROUP BY CUBE(dept_id, boss)");
    assert_eq!(r.len(), 10);
    let r = rows(&mut c, "SELECT dept_id, COUNT(*) FROM emp WHERE dept_id IS NOT NULL GROUP BY GROUPING SETS ((dept_id), ())");
    assert_eq!(r.len(), 3);
}

#[test]
fn window_functions() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT name, ROW_NUMBER() OVER (ORDER BY id) FROM emp ORDER BY id")[4], vec!["Eve", "5"]);
    assert_eq!(rows(&mut c, "SELECT name, RANK() OVER (ORDER BY salary DESC), DENSE_RANK() OVER (ORDER BY salary DESC) FROM emp ORDER BY id"), vec![
        vec!["Alice", "1", "1"], vec!["Bob", "2", "2"], vec!["Carol", "3", "3"], vec!["Dan", "3", "3"], vec!["Eve", "5", "4"]]);
    assert_eq!(rows(&mut c, "SELECT name, SUM(salary) OVER (PARTITION BY dept_id) FROM emp WHERE dept_id = 1 ORDER BY id"), vec![vec!["Alice", "180"], vec!["Bob", "180"]]);
    assert_eq!(rows(&mut c, "SELECT name, SUM(salary) OVER (ORDER BY id) FROM emp ORDER BY id")[2], vec!["Carol", "250"]);
    assert_eq!(rows(&mut c, "SELECT name, SUM(salary) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM emp ORDER BY id")[2], vec!["Carol", "150"]);
    assert_eq!(rows(&mut c, "SELECT name, LAG(name) OVER (ORDER BY id), LEAD(name, 2, 'none') OVER (ORDER BY id) FROM emp ORDER BY id"), vec![
        vec!["Alice", "NULL", "Carol"], vec!["Bob", "Alice", "Dan"], vec!["Carol", "Bob", "Eve"], vec!["Dan", "Carol", "none"], vec!["Eve", "Dan", "none"]]);
    assert_eq!(rows(&mut c, "SELECT name, NTILE(2) OVER (ORDER BY id) FROM emp ORDER BY id").iter().map(|r| r[1].clone()).collect::<Vec<_>>(), vec!["1", "1", "1", "2", "2"]);
    assert_eq!(rows(&mut c, "SELECT TOP 1 FIRST_VALUE(name) OVER (ORDER BY salary), LAST_VALUE(name) OVER (ORDER BY salary ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM emp ORDER BY id"), vec![vec!["Eve", "Alice"]]);
    assert_eq!(rows(&mut c, "SELECT TOP 1 COUNT(*) OVER () FROM emp"), vec![vec!["5"]]);
    // window over aggregates
    assert_eq!(rows(&mut c, "SELECT dept_id, SUM(salary), RANK() OVER (ORDER BY SUM(salary) DESC) FROM emp WHERE dept_id IS NOT NULL GROUP BY dept_id ORDER BY dept_id"), vec![vec!["1", "180", "1"], vec!["2", "140", "2"]]);
}

#[test]
fn subqueries() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE salary = (SELECT MAX(salary) FROM emp)"), vec![vec!["Alice"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM dept d WHERE EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id) ORDER BY id"), vec![vec!["Eng"], vec!["Ops"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM dept d WHERE NOT EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id)"), vec![vec!["Empty"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE name = 'Ops') ORDER BY id"), vec![vec!["Carol"], vec!["Dan"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE salary > ALL (SELECT salary FROM emp WHERE dept_id = 2) ORDER BY id"), vec![vec!["Alice"], vec!["Bob"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM emp WHERE salary > ANY (SELECT salary FROM emp WHERE dept_id = 1) ORDER BY id"), vec![vec!["Alice"]]);
    assert_eq!(rows(&mut c, "SELECT (SELECT COUNT(*) FROM emp e WHERE e.dept_id = d.id) FROM dept d ORDER BY id"), vec![vec!["2"], vec!["2"], vec!["0"]]);
    assert_eq!(rows(&mut c, "SELECT x.m FROM (SELECT MAX(salary) m FROM emp) x"), vec![vec!["100"]]);
    assert_eq!(err_no(&mut c, "SELECT (SELECT salary FROM emp)"), 512);
    assert_eq!(rows(&mut c, "SELECT (SELECT salary FROM emp WHERE 1=0)"), vec![vec!["NULL"]]);
}

#[test]
fn set_operations() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "SELECT 1 UNION SELECT 1 UNION SELECT 2 ORDER BY 1"), vec![vec!["1"], vec!["2"]]);
    assert_eq!(rows(&mut c, "SELECT 1 UNION ALL SELECT 1").len(), 2);
    assert_eq!(rows(&mut c, "SELECT 1 AS a UNION SELECT 2 EXCEPT SELECT 2"), vec![vec!["1"]]);
    assert_eq!(rows(&mut c, "SELECT 1 INTERSECT SELECT 1"), vec![vec!["1"]]);
    assert_eq!(rows(&mut c, "SELECT x FROM (SELECT 3 AS x UNION ALL SELECT 1 UNION ALL SELECT 2) t ORDER BY x DESC"), vec![vec!["3"], vec!["2"], vec!["1"]]);
    assert_eq!(err_no(&mut c, "SELECT 1, 2 UNION SELECT 1"), 205);
    assert_eq!(rows(&mut c, "SELECT 'a' AS v UNION ALL SELECT 'b' ORDER BY v DESC"), vec![vec!["b"], vec!["a"]]);
}

#[test]
fn common_table_expressions() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "WITH x AS (SELECT dept_id, SUM(salary) s FROM emp GROUP BY dept_id) SELECT s FROM x WHERE dept_id = 1"), vec![vec!["180"]]);
    assert_eq!(rows(&mut c, "WITH a AS (SELECT 1 AS n), b AS (SELECT n + 1 AS n FROM a) SELECT n FROM b"), vec![vec!["2"]]);
    // recursive: org chart
    let r = rows(&mut c, "WITH t AS (SELECT id, name, 0 AS lvl FROM emp WHERE boss IS NULL UNION ALL SELECT e.id, e.name, t.lvl + 1 FROM emp e JOIN t ON e.boss = t.id) SELECT name, lvl FROM t ORDER BY lvl, id");
    assert_eq!(r, vec![vec!["Alice", "0"], vec!["Bob", "1"], vec!["Carol", "1"], vec!["Dan", "2"], vec!["Eve", "2"]]);
    // numbers table
    assert_eq!(rows(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 100) SELECT SUM(i) FROM n"), vec![vec!["5050"]]);
    assert_eq!(err_no(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n) SELECT COUNT(*) FROM n"), 530);
    assert_eq!(rows(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 500) SELECT COUNT(*) FROM n OPTION (MAXRECURSION 1000)"), vec![vec!["500"]]);
}

#[test]
fn pivot_unpivot() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT * FROM (SELECT dept_id, salary FROM emp WHERE dept_id IS NOT NULL) s PIVOT (SUM(salary) FOR dept_id IN ([1],[2])) p");
    assert_eq!(r, vec![vec!["180", "140"]]);
    exec(&mut c, "CREATE TABLE q (y INT, q1 INT, q2 INT)");
    exec(&mut c, "INSERT q VALUES (2020, 10, 20)");
    let r = rows(&mut c, "SELECT y, quarter, amount FROM q UNPIVOT (amount FOR quarter IN (q1, q2)) u ORDER BY quarter");
    assert_eq!(r, vec![vec!["2020", "q1", "10"], vec!["2020", "q2", "20"]]);
}

#[test]
fn table_value_constructor_and_functions() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "SELECT a + b FROM (VALUES (1,2),(3,4)) AS t(a,b) ORDER BY 1"), vec![vec!["3"], vec!["7"]]);
    assert_eq!(rows(&mut c, "SELECT value FROM STRING_SPLIT('x,y,z', ',') ORDER BY value DESC"), vec![vec!["z"], vec!["y"], vec!["x"]]);
    assert_eq!(rows(&mut c, "SELECT SUM(value) FROM GENERATE_SERIES(1, 100)"), vec![vec!["5050"]]);
    assert_eq!(rows(&mut c, "SELECT [key], value FROM OPENJSON('{\"a\":1,\"b\":\"x\"}')"), vec![vec!["a", "1"], vec!["b", "x"]]);
    assert_eq!(rows(&mut c, "SELECT * FROM OPENJSON('[{\"id\":1,\"n\":\"a\"},{\"id\":2,\"n\":\"b\"}]') WITH (id INT '$.id', n VARCHAR(10) '$.n')"), vec![vec!["1", "a"], vec!["2", "b"]]);
}

#[test]
fn select_into_and_assignment() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "SELECT id, name INTO #copy FROM emp WHERE salary >= 80");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM #copy"), vec![vec!["2"]]);
    exec(&mut c, "SELECT * INTO emp2 FROM emp");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM emp2"), vec![vec!["5"]]);
    assert_eq!(err_no(&mut c, "SELECT * INTO emp2 FROM emp"), 2714);
    assert_eq!(rows(&mut c, "DECLARE @a INT, @b VARCHAR(10); SELECT @a = id, @b = name FROM emp WHERE id = 3; SELECT @a, @b"), vec![vec!["3", "Carol"]]);
    assert_eq!(rows(&mut c, "DECLARE @s VARCHAR(100) = ''; SELECT @s = @s + name + ',' FROM emp ORDER BY id; SELECT @s"), vec![vec!["Alice,Bob,Carol,Dan,Eve,"]]);
    exec(&mut c, "SELECT IDENTITY(INT, 10, 5) AS rid, name INTO #ids FROM emp");
    assert_eq!(rows(&mut c, "SELECT MIN(rid), MAX(rid) FROM #ids"), vec![vec!["10", "30"]]);
}

#[test]
fn for_json_and_xml() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT id, name FROM dept WHERE id < 3 FOR JSON PATH"), "[{\"id\":1,\"name\":\"Eng\"},{\"id\":2,\"name\":\"Ops\"}]");
    assert_eq!(scalar(&mut c, "SELECT name FROM dept WHERE id = 1 FOR JSON PATH, WITHOUT_ARRAY_WRAPPER"), "{\"name\":\"Eng\"}");
    assert_eq!(scalar(&mut c, "SELECT name FROM dept WHERE id = 1 FOR JSON PATH, ROOT('d')"), "{\"d\":[{\"name\":\"Eng\"}]}");
    assert_eq!(scalar(&mut c, "SELECT id, NULL AS n FROM dept WHERE id = 1 FOR JSON PATH"), "[{\"id\":1}]");
    assert_eq!(scalar(&mut c, "SELECT id, NULL AS n FROM dept WHERE id = 1 FOR JSON PATH, INCLUDE_NULL_VALUES"), "[{\"id\":1,\"n\":null}]");
    assert_eq!(scalar(&mut c, "SELECT name FROM dept WHERE id < 3 FOR XML PATH('d')"), "<d><name>Eng</name></d><d><name>Ops</name></d>");
    // classic string concatenation idiom
    assert_eq!(scalar(&mut c, "SELECT STUFF((SELECT ',' + name FROM dept ORDER BY id FOR XML PATH('')), 1, 1, '')"), "Eng,Ops,Empty");
}

#[test]
fn errors_for_bad_queries() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(err_no(&mut c, "SELECT nope FROM emp"), 207);
    assert_eq!(err_no(&mut c, "SELECT * FROM nothere"), 208);
    assert_eq!(err_no(&mut c, "SELECT id FROM emp e JOIN dept d ON 1=1"), 209);
    assert_eq!(err_no(&mut c, "SELEC 1"), 2812);
    assert_eq!(err_no(&mut c, "SELECT FROM"), 102);
    assert_eq!(err_no(&mut c, "SELECT 1 FROM emp WHERE"), 102);
    assert_eq!(err_no(&mut c, "SELECT @undeclared"), 137);
    assert_eq!(err_no(&mut c, "SELECT NOSUCHFUNC(1)"), 195);
    assert_eq!(err_no(&mut c, "SELECT SUM(salary) FROM emp WHERE SUM(salary) > 1"), 147);
}
