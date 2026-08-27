// Where does a request actually spend its time?
use asdb::asl::{parse, tokenize};
use asdb::database::Database;
use asdb::query::{bind, execute, plan, BoundStatement};
use std::time::Instant;

fn main() {
    let path = std::env::temp_dir().join("asdb_phases.db");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path).unwrap();
    db.create_collection("t").unwrap();

    let stmt = r#"from t | insert { placeId: "place-1", jobId: "job-a", playerCount: 42, serverFps: 58.5, receivedAt: 1754500000000 }"#;
    let n = 20_000;

    // 1. lex
    let t = Instant::now();
    for _ in 0..n { let _ = tokenize(stmt).unwrap(); }
    let lex = t.elapsed().as_nanos() as f64 / n as f64;

    // 2. parse
    let toks = tokenize(stmt).unwrap();
    let t = Instant::now();
    for _ in 0..n { let _ = parse(&toks).unwrap(); }
    let parse_ns = t.elapsed().as_nanos() as f64 / n as f64;

    // 3. bind + plan
    let st = parse(&toks).unwrap();
    let t = Instant::now();
    for _ in 0..n {
        let b = bind(&db, st.clone()).unwrap();
        if let BoundStatement::Pipeline(p) = b { let _ = plan(&db, &p).unwrap(); }
    }
    let bindplan = t.elapsed().as_nanos() as f64 / n as f64;

    // 4. execute (the real work)
    let b = bind(&db, parse(&toks).unwrap()).unwrap();
    let BoundStatement::Pipeline(p) = b else { panic!() };
    let physical = plan(&db, &p).unwrap();
    let t = Instant::now();
    for _ in 0..n { execute(&mut db, &physical).unwrap(); }
    let exec = t.elapsed().as_nanos() as f64 / n as f64;

    let total = lex + parse_ns + bindplan + exec;
    println!("per single-document insert, in-process (no network, no HTTP):");
    println!("  lex        {:>8.2} us  {:>5.1}%", lex/1000.0, 100.0*lex/total);
    println!("  parse      {:>8.2} us  {:>5.1}%", parse_ns/1000.0, 100.0*parse_ns/total);
    println!("  bind+plan  {:>8.2} us  {:>5.1}%", bindplan/1000.0, 100.0*bindplan/total);
    println!("  execute    {:>8.2} us  {:>5.1}%", exec/1000.0, 100.0*exec/total);
    println!("  ---------------------------------");
    println!("  in-process {:>8.2} us", total/1000.0);
    let _ = std::fs::remove_file(&path);
}
