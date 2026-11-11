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
