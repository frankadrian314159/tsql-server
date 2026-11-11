//! Full-text search: CONTAINS, FREETEXT, CONTAINSTABLE and FREETEXTTABLE (matching by scan; see src/fulltext.rs).
mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE docs (id INT PRIMARY KEY, title VARCHAR(60) NULL, body NVARCHAR(MAX) NULL);
         INSERT INTO docs VALUES (1, 'Quick fox', 'The quick brown fox jumps over the lazy dog.');
         INSERT INTO docs VALUES (2, 'Lazy days', 'A dog sleeps all day. Dogs are lazy, and foxes are quick.');
         INSERT INTO docs VALUES (3, 'Computing', 'Computers compute. Computing is fun for computer scientists.');
         INSERT INTO docs VALUES (4, NULL, NULL);
         INSERT INTO docs VALUES (5, 'Running', 'He was running home; she runs daily, they ran yesterday.');",
    );
}

fn ids(c: &mut tsql_server::Connection, sql: &str) -> Vec<String> {
    rows(c, sql).into_iter().map(|r| r[0].clone()).collect()
}

#[test]
fn contains_matches_words_phrases_prefixes_and_boolean_conditions() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'fox') ORDER BY id"), vec!["1"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'DOG') ORDER BY id"), vec!["1", "2"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, '\"lazy dog\"') ORDER BY id"), vec!["1"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, '\"comput*\"') ORDER BY id"), vec!["3"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'dog AND lazy') ORDER BY id"), vec!["1", "2"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'dog AND NOT fox') ORDER BY id"), vec!["2"], "document 2 has foxes, not fox");
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'dog AND NOT lazy') ORDER BY id"), Vec::<String>::new());
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'fox OR computers') ORDER BY id"), vec!["1", "3"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'quick NEAR lazy') ORDER BY id"), vec!["1", "2"]);
    // inflectional forms
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'FORMSOF(INFLECTIONAL, run)') ORDER BY id"), vec!["5"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'FORMSOF(INFLECTIONAL, fox)') ORDER BY id"), vec!["1", "2"]);
    // several columns, and all columns
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS((title, body), 'lazy') ORDER BY id"), vec!["1", "2"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(title, 'lazy') ORDER BY id"), vec!["2"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(*, 'running') ORDER BY id"), vec!["5"]);
    // a NULL column does not match (so NOT CONTAINS has it); the rest of a WHERE works as always
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM docs WHERE NOT CONTAINS(body, 'fox')"), "4");
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'dog') AND id > 1"), vec!["2"]);
    // a variable as the condition, and LANGUAGE
    assert_eq!(ids(&mut c, "DECLARE @s NVARCHAR(100) = 'fox'; SELECT id FROM docs WHERE CONTAINS(body, @s, LANGUAGE 1033)"), vec!["1"]);
    // errors in the condition
    assert_eq!(err_no(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'the')"), 7619);
    assert_eq!(err_no(&mut c, "SELECT id FROM docs WHERE CONTAINS(body, 'fox AND')"), 7630);
}

#[test]
fn freetext_matches_any_word_by_stem() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE FREETEXT(body, 'foxes sleeping') ORDER BY id"), vec!["1", "2"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE FREETEXT(*, 'computer') ORDER BY id"), vec!["3"]);
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE FREETEXT(body, 'run') ORDER BY id"), vec!["5"]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM docs WHERE FREETEXT(body, 'the of')"), "0");
}

#[test]
fn containstable_and_freetexttable_return_key_and_rank() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(
        rows(&mut c, "SELECT d.id, k.[RANK] > 0 FROM docs d INNER JOIN CONTAINSTABLE(docs, body, 'dog') AS k ON d.id = k.[KEY] ORDER BY d.id"),
        vec![vec!["1", "1"], vec!["2", "1"]]
    );
    // the more often a word occurs the higher the rank
    exec(&mut c, "UPDATE docs SET body = 'lazy lazy lazy cat' WHERE id = 2");
    let r = rows(&mut c, "SELECT [KEY] FROM CONTAINSTABLE(docs, *, 'dog OR lazy') ORDER BY [RANK] DESC, [KEY]");
    assert_eq!(r.iter().map(|x| x[0].as_str()).collect::<Vec<_>>(), vec!["2", "1"]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM CONTAINSTABLE(docs, (title, body), 'lazy')"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM CONTAINSTABLE(docs, body, 'lazy', 1)"), "1", "the top n by rank");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM FREETEXTTABLE(docs, *, 'fox computing')"), "2", "document 2 no longer has a fox");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM FREETEXTTABLE(dbo.docs, body, 'fox', LANGUAGE 1033)"), "1");
}

/// The procedure of AdventureWorks, as the database scripts it.
#[test]
fn a_search_procedure_in_the_style_of_adventureworks() {
    let mut c = conn();
    setup(&mut c);
    exec(
        &mut c,
        "CREATE PROCEDURE dbo.uspSearch @searchString NVARCHAR(1000), @useInflectional BIT = 0, @language INT = 0
         WITH EXECUTE AS CALLER
         AS
         BEGIN
             SET NOCOUNT ON;
             DECLARE @string NVARCHAR(1050)
             IF @language = NULL OR @language = 0
             BEGIN
                 SELECT @language = CONVERT(int, SERVERPROPERTY('lcid'))
             END
             IF @useInflectional = 1
             BEGIN
                 SELECT @string = 'FORMSOF(INFLECTIONAL,\"' + @searchString + '\"' + ')'
                 SELECT FT_TBL.[id], KEY_TBL.[RANK] FROM [dbo].[docs] AS FT_TBL
                     INNER JOIN CONTAINSTABLE([dbo].[docs], *, @string, LANGUAGE @language) AS KEY_TBL
                 ON FT_TBL.[id] = KEY_TBL.[KEY]
             END
             ELSE
             BEGIN
                 SELECT @string = '\"' + @searchString + '\"'
                 SELECT FT_TBL.[id], KEY_TBL.[RANK] FROM [dbo].[docs] AS FT_TBL
                     INNER JOIN CONTAINSTABLE([dbo].[docs], *, @string, LANGUAGE @language) AS KEY_TBL
                 ON FT_TBL.[id] = KEY_TBL.[KEY]
             END
         END",
    );
    assert_eq!(ids(&mut c, "EXEC dbo.uspSearch N'lazy dog'"), vec!["1"]);
    let mut got = ids(&mut c, "EXEC dbo.uspSearch N'fox', 1");
    got.sort();
    assert_eq!(got, vec!["1", "2"]);
}

#[test]
fn catalogs_and_indexes_can_be_declared_and_are_listed() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE docs (id INT NOT NULL, title VARCHAR(60) NULL, body NVARCHAR(MAX) NULL, n INT NULL, CONSTRAINT pk_docs PRIMARY KEY (id));
         CREATE UNIQUE INDEX ux_title ON docs (title);
         INSERT INTO docs VALUES (1, 'a', 'the quick fox', 1);",
    );
    exec(&mut c, "CREATE FULLTEXT CATALOG ftc AS DEFAULT");
    exec(&mut c, "CREATE FULLTEXT CATALOG other WITH ACCENT_SENSITIVITY = OFF");
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT CATALOG ftc"), 7609);
    assert_eq!(rows(&mut c, "SELECT name, is_default, is_accent_sensitivity_on FROM sys.fulltext_catalogs ORDER BY name").iter().map(|r| r.join("/")).collect::<Vec<_>>(), vec!["ftc/1/1", "other/0/0"]);
    // what is checked
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX nope"), 7653);
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX ux_title"), 7653, "the key must not be nullable");
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT INDEX ON docs (n) KEY INDEX pk_docs"), 7654, "an int column cannot be indexed");
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX pk_docs ON missing"), 7608);
    assert_eq!(err_no(&mut c, "ALTER FULLTEXT INDEX ON docs DISABLE"), 7612, "there is no index yet");
    exec(&mut c, "CREATE FULLTEXT INDEX ON docs (title LANGUAGE 1033, body LANGUAGE 'English') KEY INDEX pk_docs ON other WITH (CHANGE_TRACKING = MANUAL, STOPLIST = SYSTEM)");
    assert_eq!(err_no(&mut c, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX pk_docs"), 7611, "one index per table");
    assert_eq!(rows(&mut c, "SELECT fi.is_enabled, fi.change_tracking_state_desc, fc.name FROM sys.fulltext_indexes fi JOIN sys.fulltext_catalogs fc ON fc.fulltext_catalog_id = fi.fulltext_catalog_id WHERE fi.object_id = OBJECT_ID('docs')")[0].join("/"), "1/MANUAL/other");
    assert_eq!(rows(&mut c, "SELECT column_id, language_id FROM sys.fulltext_index_columns WHERE object_id = OBJECT_ID('docs') ORDER BY column_id").iter().map(|r| r.join("/")).collect::<Vec<_>>(), vec!["2/1033", "3/1033"]);
    // the search works as before
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM docs WHERE CONTAINS(body, 'fox')"), "1");
    // changes
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs DISABLE");
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs SET CHANGE_TRACKING AUTO");
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs ENABLE");
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs DROP (title)");
    assert_eq!(err_no(&mut c, "ALTER FULLTEXT INDEX ON docs DROP (title)"), 7614);
    assert_eq!(err_no(&mut c, "ALTER FULLTEXT INDEX ON docs DROP (body)"), 7615);
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs ADD (title)");
    assert_eq!(err_no(&mut c, "ALTER FULLTEXT INDEX ON docs ADD (title)"), 7613);
    exec(&mut c, "ALTER FULLTEXT INDEX ON docs START FULL POPULATION");
    assert_eq!(scalar(&mut c, "SELECT change_tracking_state_desc FROM sys.fulltext_indexes WHERE object_id = OBJECT_ID('docs')"), "AUTO");
    // dependencies
    assert_eq!(err_no(&mut c, "ALTER TABLE docs DROP COLUMN body"), 5074);
    assert_eq!(err_no(&mut c, "ALTER TABLE docs DROP CONSTRAINT pk_docs"), 7653);
    assert_eq!(err_no(&mut c, "DROP FULLTEXT CATALOG other"), 7610);
    exec(&mut c, "ALTER FULLTEXT CATALOG ftc REBUILD");
    exec(&mut c, "ALTER FULLTEXT CATALOG other AS DEFAULT");
    assert_eq!(scalar(&mut c, "SELECT name FROM sys.fulltext_catalogs WHERE is_default = 1"), "other");
    exec(&mut c, "DROP FULLTEXT INDEX ON docs");
    assert_eq!(err_no(&mut c, "DROP FULLTEXT INDEX ON docs"), 7612);
    exec(&mut c, "DROP FULLTEXT CATALOG other");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.fulltext_catalogs"), "1");
    assert_eq!(err_no(&mut c, "DROP FULLTEXT CATALOG other"), 7608);
}
