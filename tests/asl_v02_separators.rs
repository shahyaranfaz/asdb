/*
asl_v02_separators.rs: the v0.2 stage-separation rule.

asl.txt v0.2: a new stage begins wherever a STAGE KEYWORD appears in stage
position, and any separator between stages is optional.

The central claim these tests exist to prove is EQUIVALENCE. Four ways of
writing the same query must produce byte-identical ASTs, because if they only
"mostly" agree the language has four dialects instead of one spelling choice.

The second claim is BACKWARD COMPATIBILITY: every v0.1 query still parses.
That one is mostly carried by the existing suite, which passes unchanged.
*/

use asdb::asl::{parse, tokenize, Pipeline, Statement};

fn pipeline(src: &str) -> Pipeline {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed for {src:?}: {e:?}"));
    match parse(&tokens) {
        Ok(Statement::Pipeline(stages)) => stages,
        Ok(other) => panic!("expected a pipeline for {src:?}, got {other:?}"),
        Err(e) => panic!("parse failed for {src:?}: {e:?}"),
    }
}

fn rejects(src: &str) -> bool {
    match tokenize(src) {
        Err(_) => true,
        Ok(tokens) => parse(&tokens).is_err(),
    }
}

#[test]
fn test_all_four_separator_styles_are_identical() {
    let pipe = pipeline("from users | where age > 18 | select name");
    let comma = pipeline("from users, where age > 18, select name");
    let bare = pipeline("from users where age > 18 select name");
    let newline = pipeline("from users\nwhere age > 18\nselect name");

    assert_eq!(pipe, comma, "comma form differs from pipe form");
    assert_eq!(pipe, bare, "bare form differs from pipe form");
    assert_eq!(pipe, newline, "newline form differs from pipe form");
    assert_eq!(pipe.len(), 3);
}

#[test]
fn test_separators_can_be_mixed_within_one_query() {
    // nothing requires consistency, because the separator carries no meaning.
    let mixed = pipeline("from users, where age > 18 select name | limit 5");
    let plain = pipeline("from users | where age > 18 | select name | limit 5");
    assert_eq!(mixed, plain);
}

#[test]
fn test_a_stage_may_wrap_across_lines() {
    // `and` is not a stage keyword, so the where stage simply continues.
    let wrapped = pipeline("from users\nwhere age > 18\n  and country == \"CA\"\nselect name");
    let one_line = pipeline("from users | where age > 18 and country == \"CA\" | select name");
    assert_eq!(wrapped, one_line);
    assert_eq!(wrapped.len(), 3, "the continuation must not become a stage");
}

#[test]
fn test_list_comma_versus_stage_comma() {
    /*
    The disambiguation rule: at a comma, the token AFTER it decides.
    Same punctuation, two meanings, resolved by what follows.
    */
    let list = pipeline("from users select name, email");
    assert_eq!(list.len(), 2, "name, email is ONE select stage");

    let staged = pipeline("from users select name, where age > 1");
    assert_eq!(staged.len(), 3, "the comma before `where` ended the select");
}

#[test]
fn test_the_rule_holds_for_every_stage_level_list() {
    // order keys, group keys and update assignments use the same rule.
    assert_eq!(pipeline("from users order age asc, name desc").len(), 2);
    assert_eq!(pipeline("from users order age asc, limit 5").len(), 3);

    assert_eq!(pipeline("from users group country, status select country, count").len(), 3);

    assert_eq!(pipeline("from users update set a = 1, b = 2").len(), 2);
    assert_eq!(pipeline("from users update set a = 1, where b == 2").len(), 3);
}

#[test]
fn test_bracketed_lists_are_unaffected() {
    // a closing bracket terminates these, so the stage-keyword rule must not
    // interfere with them.
    let doc = pipeline(r#"from users insert { a: 1, b: [1, 2, 3] }"#);
    assert_eq!(doc.len(), 2);
}

#[test]
fn test_every_v01_query_still_parses() {
    // backward compatibility, spelled out. These are the spec's own examples.
    for src in [
        "from users",
        "from users | where age > 18",
        "from users | where age > 18 | select name, email",
        "from users | order id asc | offset 100 | limit 25",
        "from orders | group status | select status, count",
        "from orders | join users on user_id == id | select name, total",
        r#"from users | where country == "CA" | update set active = true"#,
        "from users | where age < 13 | delete",
    ] {
        let _ = pipeline(src);
    }
}

#[test]
fn test_trailing_separator_is_still_an_error() {
    // a separator with no stage after it is a truncated query, not decoration.
    assert!(rejects("from users |"));
    assert!(rejects("from users | where age > 1 |"));
    assert!(rejects("from users,"));
}

#[test]
fn test_backtick_escapes_a_reserved_word() {
    /*
    Stage keywords were ALREADY reserved in v0.1: the lexer maps "order" to
    Token::Order unconditionally, so a field named order could not be
    referenced at all. Backticks are the escape v0.1 lacked entirely.
    */
    assert!(rejects("from logs select level, order"), "bare `order` is reserved");

    let escaped = pipeline("from logs select level, `order`");
    assert_eq!(escaped.len(), 2, "the quoted name is a projection, not a stage");
}

#[test]
fn test_backtick_identifier_behaves_as_an_ordinary_field() {
    let quoted = pipeline("from logs select `level`");
    let plain = pipeline("from logs select level");
    assert_eq!(quoted, plain, "quoting must not change the meaning");
}

#[test]
fn test_empty_and_unterminated_backticks_are_rejected() {
    assert!(rejects("from logs select ``"));
    assert!(rejects("from logs select `order"));
}
