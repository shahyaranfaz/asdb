/*
query_end_to_end.rs: real ASL text, through the whole stack, against a real
database file.

parse -> bind -> plan -> execute. These are the tests that would have caught a
planner and executor that compile but do not agree with each other, which unit
tests on either side alone cannot do.
*/

use asdb::asl::{parse, tokenize, Statement};
use asdb::database::Database;
use asdb::document::{Document, Value};
use asdb::query::{bind, execute, plan, BoundStatement, QueryOutput};

/*
run: the whole pipeline for one query string.

Kept as a helper rather than repeated, because the four-step shape is the
thing under test and it should look the same in every case.
*/
fn run(db: &mut Database, query: &str) -> QueryOutput {
    let tokens = tokenize(query).expect("lex failed");
    let statement = parse(&tokens).expect("parse failed");
    let Statement::Pipeline(_) = &statement else {
        panic!("expected a pipeline");
    };
    let bound = bind(db, statement).expect("bind failed");
    let BoundStatement::Pipeline(pipeline) = bound else {
        panic!("expected a bound pipeline");
    };
    let physical = plan(db, &pipeline).expect("plan failed");
    execute(db, &physical).expect("execute failed")
}

fn docs(output: QueryOutput) -> Vec<Document> {
    match output {
        QueryOutput::Documents(d) => d,
        QueryOutput::Affected(n) => panic!("expected documents, got {n} affected"),
    }
}

fn affected(output: QueryOutput) -> usize {
    match output {
        QueryOutput::Affected(n) => n,
        QueryOutput::Documents(d) => panic!("expected a count, got {} documents", d.len()),
    }
}

fn int(doc: &Document, field: &str) -> i64 {
    match doc.get(field) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected int at {field}, got {other:?}"),
    }
}

fn text(doc: &Document, field: &str) -> String {
    match doc.get(field) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected string at {field}, got {other:?}"),
    }
}

struct TempDb {
    path: std::path::PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("asdb_e2e_{tag}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }
    fn open(&self) -> Database {
        Database::create(&self.path).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn user(id: i64, name: &str, age: i64, country: &str) -> Document {
    let mut doc = Document::new();
    doc.insert("id".to_string(), Value::Int(id));
    doc.insert("name".to_string(), Value::String(name.to_string()));
    doc.insert("age".to_string(), Value::Int(age));
    doc.insert("country".to_string(), Value::String(country.to_string()));
    doc
}

fn seed(db: &mut Database) {
    db.create_collection("users").unwrap();
    for doc in [
        user(1, "alice", 30, "CA"),
        user(2, "bob", 17, "US"),
        user(3, "cara", 45, "CA"),
        user(4, "dan", 25, "US"),
        user(5, "eve", 17, "CA"),
    ] {
        db.insert_doc("users", &doc).unwrap();
    }
}

#[test]
fn test_scan_returns_everything() {
    let tmp = TempDb::new("scan");
    let mut db = tmp.open();
    seed(&mut db);
    assert_eq!(docs(run(&mut db, "from users")).len(), 5);
}

#[test]
fn test_filter_selects_the_right_subset() {
    let tmp = TempDb::new("filter");
    let mut db = tmp.open();
    seed(&mut db);

    let mut names: Vec<String> = docs(run(&mut db, "from users | where age > 18"))
        .iter()
        .map(|d| text(d, "name"))
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice", "cara", "dan"]);
}

#[test]
fn test_filter_boundary_is_exclusive() {
    // the 17s must NOT come back for `> 17`, and must for `>= 17`.
    let tmp = TempDb::new("boundary");
    let mut db = tmp.open();
    seed(&mut db);
    assert_eq!(docs(run(&mut db, "from users | where age > 17")).len(), 3);
    assert_eq!(docs(run(&mut db, "from users | where age >= 17")).len(), 5);
}

#[test]
fn test_projection_keeps_only_selected_fields() {
    let tmp = TempDb::new("project");
    let mut db = tmp.open();
    seed(&mut db);

    let rows = docs(run(&mut db, "from users | where age > 40 | select name, age"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2, "only the two selected fields survive");
    assert_eq!(text(&rows[0], "name"), "cara");
    assert!(rows[0].get("country").is_none());
}

#[test]
fn test_projection_alias() {
    let tmp = TempDb::new("alias");
    let mut db = tmp.open();
    seed(&mut db);
    let rows = docs(run(&mut db, "from users | where age > 40 | select name as who"));
    assert_eq!(text(&rows[0], "who"), "cara");
}

#[test]
fn test_drop_removes_named_fields() {
    let tmp = TempDb::new("drop");
    let mut db = tmp.open();
    seed(&mut db);
    let rows = docs(run(&mut db, "from users | where age > 40 | drop country"));
    assert!(rows[0].get("country").is_none());
    assert!(rows[0].get("name").is_some());
}

#[test]
fn test_order_ascending_and_descending() {
    let tmp = TempDb::new("order");
    let mut db = tmp.open();
    seed(&mut db);

    let asc: Vec<i64> = docs(run(&mut db, "from users | order age asc"))
        .iter()
        .map(|d| int(d, "age"))
        .collect();
    assert_eq!(asc, vec![17, 17, 25, 30, 45]);

    let desc: Vec<i64> = docs(run(&mut db, "from users | order age desc"))
        .iter()
        .map(|d| int(d, "age"))
        .collect();
    assert_eq!(desc, vec![45, 30, 25, 17, 17]);
}

#[test]
fn test_pagination_pattern_from_the_spec() {
    // order, offset, limit together. The canonical asl.txt example, scaled
    // down: rows 2 and 3 of the age-ascending order.
    let tmp = TempDb::new("paginate");
    let mut db = tmp.open();
    seed(&mut db);

    let ages: Vec<i64> = docs(run(&mut db, "from users | order age asc | offset 2 | limit 2"))
        .iter()
        .map(|d| int(d, "age"))
        .collect();
    assert_eq!(ages, vec![25, 30]);
}

#[test]
fn test_limit_past_the_end_is_not_an_error() {
    let tmp = TempDb::new("overlimit");
    let mut db = tmp.open();
    seed(&mut db);
    assert_eq!(docs(run(&mut db, "from users | limit 100")).len(), 5);
    assert_eq!(docs(run(&mut db, "from users | offset 100")).len(), 0);
}

#[test]
fn test_insert_then_read_back() {
    let tmp = TempDb::new("insert");
    let mut db = tmp.open();
    db.create_collection("users").unwrap();

    let n = affected(run(&mut db, r#"from users | insert { id: 1, name: "zoe", age: 22 }"#));
    assert_eq!(n, 1);

    let rows = docs(run(&mut db, "from users"));
    assert_eq!(rows.len(), 1);
    assert_eq!(text(&rows[0], "name"), "zoe");
}

#[test]
fn test_update_touches_only_matched_rows() {
    let tmp = TempDb::new("update");
    let mut db = tmp.open();
    seed(&mut db);

    let n = affected(run(&mut db, r#"from users | where age > 40 | update set country = "MX""#));
    assert_eq!(n, 1, "only cara matches");

    let all = docs(run(&mut db, "from users"));
    assert_eq!(all.len(), 5, "update must not change the row count");
    let mx: Vec<&Document> = all.iter().filter(|d| text(d, "country") == "MX").collect();
    assert_eq!(mx.len(), 1);
    assert_eq!(text(mx[0], "name"), "cara");
}

#[test]
fn test_delete_removes_exactly_the_matched_set() {
    let tmp = TempDb::new("delete");
    let mut db = tmp.open();
    seed(&mut db);

    let n = affected(run(&mut db, "from users | where age < 18 | delete"));
    assert_eq!(n, 2, "bob and eve");

    let left = docs(run(&mut db, "from users"));
    assert_eq!(left.len(), 3);
    assert!(left.iter().all(|d| int(d, "age") >= 18));
}

#[test]
fn test_group_with_count() {
    let tmp = TempDb::new("group");
    let mut db = tmp.open();
    seed(&mut db);

    let rows = docs(run(&mut db, "from users | group country | select country, count"));
    assert_eq!(rows.len(), 2);
    let mut counts: Vec<(String, i64)> = rows
        .iter()
        .map(|d| (text(d, "country"), int(d, "count")))
        .collect();
    counts.sort();
    assert_eq!(counts, vec![("CA".to_string(), 3), ("US".to_string(), 2)]);
}

#[test]
fn test_group_with_sum_and_min_max() {
    let tmp = TempDb::new("aggregates");
    let mut db = tmp.open();
    seed(&mut db);

    let rows = docs(run(
        &mut db,
        "from users | group country | select country, sum age, min age, max age",
    ));
    let ca = rows.iter().find(|d| text(d, "country") == "CA").unwrap();
    // alice 30 + cara 45 + eve 17
    assert_eq!(int(ca, "sum_age"), 92);
    assert_eq!(int(ca, "min_age"), 17);
    assert_eq!(int(ca, "max_age"), 45);
}

#[test]
fn test_index_scan_agrees_with_sequential_scan() {
    /*
    THE test for the index path. Same query, same data, two different physical
    plans. The answers must be identical, and that equality is the assertion,
    because an index that returns a different answer than a full scan is worse
    than no index at all.
    */
    let tmp = TempDb::new("indexed");
    let mut db = tmp.open();
    seed(&mut db);

    let without_index = docs(run(&mut db, "from users | where age == 17"));
    db.create_index("users", "age").unwrap();
    let with_index = docs(run(&mut db, "from users | where age == 17"));

    assert_eq!(without_index.len(), 2);
    assert_eq!(with_index.len(), without_index.len());

    let mut a: Vec<String> = without_index.iter().map(|d| text(d, "name")).collect();
    let mut b: Vec<String> = with_index.iter().map(|d| text(d, "name")).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn test_planner_actually_chooses_the_index() {
    // the previous test would still pass if the planner silently ignored the
    // index, so assert the plan shape directly.
    use asdb::query::PhysicalOp;

    let tmp = TempDb::new("planshape");
    let mut db = tmp.open();
    seed(&mut db);
    db.create_index("users", "age").unwrap();

    let tokens = tokenize("from users | where age == 17").unwrap();
    let statement = parse(&tokens).unwrap();
    let BoundStatement::Pipeline(pipeline) = bind(&db, statement).unwrap() else {
        panic!("expected a pipeline");
    };
    let physical = plan(&db, &pipeline).unwrap();

    // an exact bound is fully enforced by the index, so no Filter remains
    assert!(
        matches!(physical, PhysicalOp::IndexScan { .. }),
        "expected a bare IndexScan, got {physical:?}"
    );
}

#[test]
fn test_unindexed_field_stays_a_sequential_scan() {
    use asdb::query::PhysicalOp;

    let tmp = TempDb::new("noindex");
    let mut db = tmp.open();
    seed(&mut db);

    let tokens = tokenize("from users | where age == 17").unwrap();
    let statement = parse(&tokens).unwrap();
    let BoundStatement::Pipeline(pipeline) = bind(&db, statement).unwrap() else {
        panic!("expected a pipeline");
    };
    match plan(&db, &pipeline).unwrap() {
        PhysicalOp::Filter { input, .. } => {
            assert!(matches!(*input, PhysicalOp::SeqScan { .. }));
        }
        other => panic!("expected Filter(SeqScan), got {other:?}"),
    }
}

#[test]
fn test_results_survive_a_restart() {
    /*
    The milestone case. Write through the query engine, close the database,
    reopen it, and re-run the query. Anything cached-but-not-durable shows up
    here and nowhere else.
    */
    let tmp = TempDb::new("restart");
    {
        let mut db = tmp.open();
        seed(&mut db);
        assert_eq!(docs(run(&mut db, "from users | where age > 18")).len(), 3);
    }

    let mut db = Database::open(&tmp.path).unwrap();
    let rows = docs(run(&mut db, "from users | where age > 18"));
    assert_eq!(rows.len(), 3, "results changed across a restart");

    let mut names: Vec<String> = rows.iter().map(|d| text(d, "name")).collect();
    names.sort();
    assert_eq!(names, vec!["alice", "cara", "dan"]);
}

#[test]
fn test_strict_truthiness_reaches_the_executor() {
    // `where age` is not a boolean. It must be a loud error, not an empty
    // result that looks like "nothing matched".
    let tmp = TempDb::new("truthy");
    let mut db = tmp.open();
    seed(&mut db);

    let tokens = tokenize("from users | where age").unwrap();
    let statement = parse(&tokens).unwrap();
    let BoundStatement::Pipeline(pipeline) = bind(&db, statement).unwrap() else {
        panic!("expected a pipeline");
    };
    let physical = plan(&db, &pipeline).unwrap();
    assert!(execute(&mut db, &physical).is_err());
}

fn order(id: i64, user_id: i64, total: i64) -> Document {
    let mut doc = Document::new();
    doc.insert("order_id".to_string(), Value::Int(id));
    doc.insert("user_id".to_string(), Value::Int(user_id));
    doc.insert("total".to_string(), Value::Int(total));
    doc
}

#[test]
fn test_hash_join_matches_the_right_pairs() {
    let tmp = TempDb::new("join");
    let mut db = tmp.open();
    seed(&mut db);
    db.create_collection("orders").unwrap();
    for doc in [
        order(100, 1, 50),  // alice
        order(101, 1, 75),  // alice again, two orders
        order(102, 3, 20),  // cara
        order(103, 99, 10), // no such user, must not match
    ] {
        db.insert_doc("orders", &doc).unwrap();
    }

    let rows = docs(run(
        &mut db,
        "from orders | join users on user_id == id | select name, total",
    ));

    // three matches: alice twice, cara once. The orphan order is dropped
    // because this is an inner join.
    assert_eq!(rows.len(), 3);
    let mut pairs: Vec<(String, i64)> = rows
        .iter()
        .map(|d| (text(d, "name"), int(d, "total")))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("alice".to_string(), 50),
            ("alice".to_string(), 75),
            ("cara".to_string(), 20),
        ]
    );
}

#[test]
fn test_join_with_a_filter_before_it() {
    let tmp = TempDb::new("joinfilter");
    let mut db = tmp.open();
    seed(&mut db);
    db.create_collection("orders").unwrap();
    for doc in [order(100, 1, 50), order(101, 3, 75)] {
        db.insert_doc("orders", &doc).unwrap();
    }

    let rows = docs(run(
        &mut db,
        "from orders | where total > 60 | join users on user_id == id | select name",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(text(&rows[0], "name"), "cara");
}

/*
One-sided range predicates now plan as IndexScan with a synthesised open end
(planner::close_bounds). The risk in doing that is under-fetching: an index
scan MUST return a superset of the matches, or rows silently disappear. These
tests assert the indexed and unindexed paths agree exactly.
*/

fn ages_matching(db: &mut Database, query: &str) -> Vec<i64> {
    let mut ages: Vec<i64> = docs(run(db, query)).iter().map(|d| int(d, "age")).collect();
    ages.sort();
    ages
}

#[test]
fn test_one_sided_ranges_agree_with_and_without_an_index() {
    let tmp = TempDb::new("onesided");
    let mut db = tmp.open();
    seed(&mut db);

    let queries = [
        "from users | where age > 17 | select age",
        "from users | where age >= 17 | select age",
        "from users | where age < 30 | select age",
        "from users | where age <= 30 | select age",
        "from users | where age > 100 | select age",
        "from users | where age < 0 | select age",
    ];

    let before: Vec<Vec<i64>> = queries.iter().map(|q| ages_matching(&mut db, q)).collect();
    db.create_index("users", "age").unwrap();
    let after: Vec<Vec<i64>> = queries.iter().map(|q| ages_matching(&mut db, q)).collect();

    for (i, query) in queries.iter().enumerate() {
        assert_eq!(before[i], after[i], "index changed the answer for: {query}");
    }
    // sanity: the corpus really does exercise these
    assert_eq!(before[0], vec![25, 30, 45]);
    assert_eq!(before[1], vec![17, 17, 25, 30, 45]);
}

#[test]
fn test_one_sided_range_actually_uses_the_index() {
    // the previous test would still pass if the planner ignored the index, so
    // assert the plan shape directly.
    use asdb::query::PhysicalOp;

    let tmp = TempDb::new("onesidedplan");
    let mut db = tmp.open();
    seed(&mut db);
    db.create_index("users", "age").unwrap();

    let tokens = tokenize("from users | where age > 17").unwrap();
    let statement = parse(&tokens).unwrap();
    let BoundStatement::Pipeline(pipeline) = bind(&db, statement).unwrap() else {
        panic!("expected a pipeline");
    };

    match plan(&db, &pipeline).unwrap() {
        // exclusive bound over-fetches, so the Filter must be KEPT
        PhysicalOp::Filter { input, .. } => {
            assert!(
                matches!(*input, PhysicalOp::IndexScan { .. }),
                "expected Filter(IndexScan), got {input:?}"
            );
        }
        other => panic!("expected Filter(IndexScan), got {other:?}"),
    }
}

#[test]
fn test_string_ranges_still_fall_back_to_a_scan() {
    // there is no greatest string, so close_bounds declines and the planner
    // must not emit a half-open IndexScan.
    use asdb::query::PhysicalOp;

    let tmp = TempDb::new("stringrange");
    let mut db = tmp.open();
    seed(&mut db);
    db.create_index("users", "name").unwrap();

    let tokens = tokenize(r#"from users | where name > "b""#).unwrap();
    let statement = parse(&tokens).unwrap();
    let BoundStatement::Pipeline(pipeline) = bind(&db, statement).unwrap() else {
        panic!("expected a pipeline");
    };
    match plan(&db, &pipeline).unwrap() {
        PhysicalOp::Filter { input, .. } => {
            assert!(matches!(*input, PhysicalOp::SeqScan { .. }), "got {input:?}");
        }
        other => panic!("expected Filter(SeqScan), got {other:?}"),
    }
}

#[test]
fn test_ttl_shaped_delete_agrees_with_and_without_an_index() {
    /*
    The sweeper's exact query shape, which is what motivated closing one-sided
    bounds. Run it twice over identical data, once unindexed and once indexed,
    and require the same rows to disappear.
    */
    let tmp_a = TempDb::new("ttl_noidx");
    let tmp_b = TempDb::new("ttl_idx");
    let mut a = tmp_a.open();
    let mut b = tmp_b.open();

    for db in [&mut a, &mut b] {
        db.create_collection("snaps").unwrap();
        for i in 0..40i64 {
            let mut doc = Document::new();
            doc.insert("n".to_string(), Value::Int(i));
            doc.insert("receivedAt".to_string(), Value::Int(1_000 + i * 10));
            db.insert_doc("snaps", &doc).unwrap();
        }
    }
    // a document with no timestamp must survive both paths
    let mut orphan = Document::new();
    orphan.insert("n".to_string(), Value::Int(999));
    a.insert_doc("snaps", &orphan).unwrap();
    b.insert_doc("snaps", &orphan).unwrap();

    b.create_index("snaps", "receivedAt").unwrap();

    let cutoff = "from snaps | where receivedAt < 1200 | delete";
    let removed_a = match run(&mut a, cutoff) {
        QueryOutput::Affected(n) => n,
        other => panic!("{other:?}"),
    };
    let removed_b = match run(&mut b, cutoff) {
        QueryOutput::Affected(n) => n,
        other => panic!("{other:?}"),
    };

    assert_eq!(removed_a, removed_b, "index changed how many rows expired");
    assert_eq!(removed_a, 20, "receivedAt 1000..1190 step 10 is 20 documents");

    let mut left_a: Vec<i64> = a.scan_collection("snaps").unwrap().iter().map(|(_, d)| int(d, "n")).collect();
    let mut left_b: Vec<i64> = b.scan_collection("snaps").unwrap().iter().map(|(_, d)| int(d, "n")).collect();
    left_a.sort();
    left_b.sort();
    assert_eq!(left_a, left_b, "index changed which rows expired");
    assert!(left_a.contains(&999), "the untimestamped document must never expire");
}
