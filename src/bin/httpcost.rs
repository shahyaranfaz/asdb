// How much of a request is protocol rather than work?
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

fn one_request(addr: &str, body: &str) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    let req = format!(
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:7070".into());
    let body = r#"from bench_http | insert { placeId: "place-1", jobId: "job-a", playerCount: 42, serverFps: 58.5, receivedAt: 1754500000000 }"#;

    // create the collection first
    one_request(&addr, "create bench_http {}");

    let n = 2000;
    let t = Instant::now();
    for _ in 0..n { one_request(&addr, body); }
    let per = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

    // cost of just opening+closing a connection to the same port
    let t = Instant::now();
    for _ in 0..n { let _ = TcpStream::connect(&addr).unwrap(); }
    let conn = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;

    println!("HTTP round trip, connection per request : {:>7.2} us", per);
    println!("  of which: TCP connect + close         : {:>7.2} us  ({:.0}%)", conn, 100.0*conn/per);
    println!("  in-process work (measured separately) :    11.62 us  ({:.0}%)", 100.0*11.62/per);
    println!("  protocol + network overhead           : {:>7.2} us  ({:.0}%)", per-11.62, 100.0*(per-11.62)/per);
}
