/*
abpbench: what did the binary protocol actually buy, and which change bought it?

Every configuration below runs against ONE asdb process holding ONE Database,
so the storage cost is identical across rows and the only variable is how the
request arrives. The rows are ordered so each adds exactly one change to the
row above it, which is what makes the differences attributable instead of
merely suggestive.

    1. HTTP, connect per request     the starting point
    2. HTTP, connection reused       isolates the TCP setup cost alone
    3. ABP OP_EXEC                   adds binary framing, still parses ASL
    4. ABP OP_INSERT                 adds skipping lex/parse/bind/plan
    5. ABP OP_INSERT, batch of 100   amortises the round trip itself
    6. ABP OP_PING                   the protocol floor: no work at all
*/

use asdb::binserver::BinServer;
use asdb::database::Database;
use asdb::document::{Document, Value};
use asdb::server::Server;
use asdb::wire::*;

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

const N: usize = 2000;

fn telemetry_doc(i: usize) -> Document {
    let mut metrics = Document::new();
    metrics.insert("kills".into(), Value::Int(7));

    let mut d = Document::new();
    d.insert("placeId".into(), Value::String("place-1".into()));
    d.insert("jobId".into(), Value::String(format!("job-{i}")));
    d.insert("playerCount".into(), Value::Int(42));
    d.insert("serverFps".into(), Value::Float(58.5));
    d.insert("customMetrics".into(), Value::Document(metrics));
    d.insert("receivedAt".into(), Value::Int(1_754_500_000_000));
    d
}

fn asl_insert(collection: &str, i: usize) -> String {
    format!(
        "from {collection} | insert {{ placeId: \"place-1\", jobId: \"job-{i}\", \
         playerCount: 42, serverFps: 58.5, customMetrics: {{ kills: 7 }}, \
         receivedAt: 1754500000000 }}"
    )
}

/* ---------- HTTP client ---------- */

fn http_once(addr: &str, body: &str) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    let req = format!(
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
}

/* ---------- ABP client ---------- */

struct Abp {
    r: BufReader<TcpStream>,
    w: BufWriter<TcpStream>,
    buf: Vec<u8>,
}

impl Abp {
    fn connect(addr: &str) -> Abp {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Abp {
            r: BufReader::new(s.try_clone().unwrap()),
            w: BufWriter::new(s),
            buf: Vec::new(),
        }
    }

    fn send(&mut self, opcode: u8, body: &[u8]) {
        let mut frame = Vec::with_capacity(body.len() + 5);
        put_u32(&mut frame, (body.len() + 1) as u32);
        frame.push(opcode);
        frame.extend_from_slice(body);
        self.w.write_all(&frame).unwrap();
        self.w.flush().unwrap();
    }

    fn recv(&mut self) -> u8 {
        let mut len = [0u8; 4];
        self.r.read_exact(&mut len).unwrap();
        let n = u32::from_le_bytes(len) as usize;
        self.buf.clear();
        self.buf.resize(n, 0);
        self.r.read_exact(&mut self.buf).unwrap();
        if self.buf[0] == OP_ERROR {
            let msg = Reader::new(&self.buf[1..]).str().unwrap();
            panic!("server error: {msg}");
        }
        self.buf[0]
    }

    fn exec(&mut self, source: &str) {
        let mut body = Vec::new();
        put_str(&mut body, source);
        self.send(OP_EXEC, &body);
        self.recv();
    }

    fn insert(&mut self, collection: &str, docs: &[Document]) {
        let mut body = Vec::new();
        put_str(&mut body, collection);
        put_u32(&mut body, docs.len() as u32);
        for d in docs {
            put_document_body(&mut body, d);
        }
        self.send(OP_INSERT, &body);
        self.recv();
    }

    /*
    Send many requests before reading any reply.

    This separates two costs the simple loop conflates: the LATENCY of a round
    trip (send, wait for the other thread to wake, read) and the per-request
    syscall THROUGHPUT cost. If pipelining is much faster, the floor was
    latency and the protocol should let clients pipeline; if it is not, the
    floor is syscalls and pipelining would be a feature nobody benefits from.
    */
    fn insert_pipelined(&mut self, collection: &str, docs: &[Document], depth: usize) {
        let mut body = Vec::new();
        put_str(&mut body, collection);
        put_u32(&mut body, docs.len() as u32);
        for d in docs {
            put_document_body(&mut body, d);
        }
        for _ in 0..depth {
            let mut frame = Vec::with_capacity(body.len() + 5);
            put_u32(&mut frame, (body.len() + 1) as u32);
            frame.push(OP_INSERT);
            frame.extend_from_slice(&body);
            self.w.write_all(&frame).unwrap();
        }
        self.w.flush().unwrap();
        for _ in 0..depth {
            self.recv();
        }
    }

    fn ping(&mut self) {
        self.send(OP_PING, &[]);
        self.recv();
    }
}

fn us(elapsed: std::time::Duration, ops: usize) -> f64 {
    elapsed.as_nanos() as f64 / ops as f64 / 1000.0
}

fn main() {
    /*
    ROW 0, taken first and against its own database file: the same inserts with
    no socket at all. Everything a socket path costs is (its number) minus
    this, which is the only way to say how much of a request is protocol
    without guessing.
    */
    let local_path = std::env::temp_dir().join(format!("abplocal-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&local_path);
    let (local_single, local_batch) = {
        let mut db = Database::create(&local_path).unwrap();
        db.create_collection("l_single").unwrap();
        db.create_collection("l_batch").unwrap();
        let one = [telemetry_doc(0)];
        let hundred: Vec<Document> = (0..100).map(telemetry_doc).collect();
        for _ in 0..50 {
            db.insert_docs("l_single", &one).unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            db.insert_docs("l_single", &one).unwrap();
        }
        let single = us(t.elapsed(), N);
        let t = Instant::now();
        for _ in 0..(N / 100) {
            db.insert_docs("l_batch", &hundred).unwrap();
        }
        (single, us(t.elapsed(), N))
    };
    let _ = std::fs::remove_file(&local_path);

    let path = std::env::temp_dir().join(format!("abpbench-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Database::create(&path).unwrap();
    let server = Server::new(db);

    let http_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let abp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let http_addr = http_listener.local_addr().unwrap().to_string();
    let abp_addr = abp_listener.local_addr().unwrap().to_string();

    let abp_db = server.database();
    std::thread::spawn(move || BinServer::new(abp_db).serve(abp_listener));
    std::thread::spawn(move || server.serve(http_listener));
    std::thread::sleep(std::time::Duration::from_millis(120));

    let mut c = Abp::connect(&abp_addr);
    for name in ["b_http", "b_http_ka", "b_exec", "b_insert", "b_batch"] {
        c.exec(&format!("create {name} {{}}"));
    }

    // warm every path so the first-touch page faults land outside the timers
    for i in 0..50 {
        http_once(&http_addr, &asl_insert("b_http", i));
        c.exec(&asl_insert("b_exec", i));
        c.insert("b_insert", &[telemetry_doc(i)]);
    }

    // 1. HTTP, one connection per request
    let t = Instant::now();
    for i in 0..N {
        http_once(&http_addr, &asl_insert("b_http", i));
    }
    let http = us(t.elapsed(), N);

    // 2. HTTP with the connection cost removed, approximated by timing the
    //    connect+close alone and subtracting. asdb's HTTP server sends
    //    Connection: close and cannot actually keep a connection alive, which
    //    is itself part of the finding.
    let t = Instant::now();
    for _ in 0..N {
        let _ = TcpStream::connect(&http_addr).unwrap();
    }
    let connect_cost = us(t.elapsed(), N);

    // 3. ABP OP_EXEC: binary framing, persistent connection, still parses ASL
    let t = Instant::now();
    for i in 0..N {
        c.exec(&asl_insert("b_exec", i));
    }
    let exec = us(t.elapsed(), N);

    // 4. ABP OP_INSERT: binary documents, no lexer or parser
    let one = [telemetry_doc(0)];
    let t = Instant::now();
    for _ in 0..N {
        c.insert("b_insert", &one);
    }
    let insert = us(t.elapsed(), N);

    // 5. ABP OP_INSERT batched
    let batch: Vec<Document> = (0..100).map(telemetry_doc).collect();
    let batches = N / 100;
    let t = Instant::now();
    for _ in 0..batches {
        c.insert("b_batch", &batch);
    }
    let batched = us(t.elapsed(), batches * 100);

    // 6. ABP OP_INSERT, pipelined: one document per request as in row 4, but
    //    32 requests in flight before any reply is read.
    c.exec("create b_pipe {}");
    let depth = 32;
    let rounds = N / depth;
    let t = Instant::now();
    for _ in 0..rounds {
        c.insert_pipelined("b_pipe", &one, depth);
    }
    let pipelined = us(t.elapsed(), rounds * depth);

    // 7. protocol floor
    let t = Instant::now();
    for _ in 0..N {
        c.ping();
    }
    let ping = us(t.elapsed(), N);

    println!("\nper insert, {N} operations each, same Database\n");
    println!("  0. in-process, no socket       {local_single:>8.2} us   the work itself");
    println!("  1. HTTP, connect per request   {http:>8.2} us   baseline");
    println!("     of which TCP connect+close  {connect_cost:>8.2} us   {:.0}% of it", 100.0 * connect_cost / http);
    println!("  3. ABP exec  (ASL text)        {exec:>8.2} us   {:.2}x faster", http / exec);
    println!("  4. ABP insert (binary docs)    {insert:>8.2} us   {:.2}x faster", http / insert);
    println!("  5. ABP insert, batch of 100    {batched:>8.2} us   {:.2}x faster", http / batched);
    println!("  6. ABP insert, pipelined x32   {pipelined:>8.2} us   {:.2}x faster", http / pipelined);
    println!("  7. ABP ping  (protocol floor)  {ping:>8.2} us");
    println!("\nattribution");
    println!("  persistent connection + framing  {:>8.2} us saved", http - exec);
    println!("  skipping lex/parse/bind/plan     {:>8.2} us saved", exec - insert);
    println!("  pipelining alone (same docs)     {:>8.2} us saved", insert - pipelined);
    println!("  batching the round trip          {:>8.2} us saved", insert - batched);
    println!("  remaining protocol floor         {ping:>8.2} us");
    println!("\nprotocol cost, single insert (row minus row 0)");
    println!("  over HTTP                        {:>8.2} us   {:.0}% of the request", http - local_single, 100.0 * (http - local_single) / http);
    println!("  over ABP                         {:>8.2} us   {:.0}% of the request", insert - local_single, 100.0 * (insert - local_single) / insert);
    println!("  over ABP, pipelined              {:>8.2} us   {:.0}% of the request", pipelined - local_single, 100.0 * (pipelined - local_single) / pipelined);
    println!("\nbatch of 100, per document");
    println!("  in-process                       {local_batch:>8.2} us");
    println!("  over ABP                         {batched:>8.2} us   protocol adds {:.2} us ({:.0}%)", batched - local_batch, 100.0 * (batched - local_batch) / batched);

    let _ = std::fs::remove_file(&path);
}
