/*
binserver.rs: the ABP/1 listener.

THIS IS NOT A SECOND DATABASE. It is a second way for bytes to arrive at the
same one. server.rs speaks HTTP, this speaks binary frames, and both hold the
same Arc<Mutex<Database>>: same storage, same B-trees, same query engine, same
file on disk. Running both at once is normal and is how a client migrates one
call site at a time while curl still works against the HTTP port.

WHY IT EXISTS, in one measurement: over HTTP, 82% of a request was protocol
overhead and 49% of the whole request was opening and closing a TCP connection
the previous request had just thrown away. PROTOCOL.txt has the numbers.

THREE THINGS IT DOES DIFFERENTLY

1. THE CONNECTION PERSISTS. A client connects once and issues many requests.
   The largest single win, and it needed a loop rather than a return.

2. FRAMING IS LENGTH-PREFIXED BINARY. No header block to format outbound or
   scan inbound. A length prefix also cannot be spoofed by content, unlike a
   delimiter, which is why values need no escaping on this path.

3. OP_INSERT CARRIES DOCUMENTS DIRECTLY. The dominant operation in the target
   workload is "insert these documents into this collection". Expressed as ASL
   text, the client renders values to text and the server immediately lexes,
   parses, binds and plans them back into the values it started with.
   OP_INSERT skips all four and hands the storage layer the documents. OP_EXEC
   still accepts arbitrary ASL, so nothing is lost.

CONCURRENCY is unchanged and still honest: one Mutex around the Database, so
statements serialise and every connection thread queues on the same lock. That
is the storage layer's constraint, not the protocol's.
*/

use crate::asl::{parse, tokenize};
use crate::database::Database;
use crate::query::{bind, execute, plan, BoundStatement, QueryOutput};
use crate::wire::*;

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub struct BinServer {
    db: Arc<Mutex<Database>>,
}

impl BinServer {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        BinServer { db }
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let db = Arc::clone(&self.db);
                    std::thread::spawn(move || {
                        if let Err(e) = handle(stream, db) {
                            // a client hanging up mid-frame is normal shutdown,
                            // not an error worth printing
                            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                                eprintln!("abp connection ended: {e}");
                            }
                        }
                    });
                }
                Err(e) => eprintln!("abp accept failed: {e}"),
            }
        }
    }
}

/*
One connection, many requests.

TCP_NODELAY is set because this is request/response with small messages:
Nagle's algorithm would hold a small reply for up to 40ms waiting to coalesce
with more data that is never coming, which on a 10us operation is a 4000x
latency penalty. This single line is the one whose absence looks like "the
database is mysteriously slow sometimes".

BufWriter is the same argument outbound: without it a reply assembled from
several small writes becomes several small packets.
*/
fn handle(stream: TcpStream, db: Arc<Mutex<Database>>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut frame = Vec::new();

    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut len_buf) {
            // a clean hangup between frames is how a client says goodbye
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(e)
            };
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_FRAME {
            reply_error(&mut writer, &format!("bad frame length {len}"))?;
            writer.flush()?;
            return Ok(());
        }

        // one buffer reused across every request on this connection, rather
        // than an allocation per frame
        frame.clear();
        frame.resize(len, 0);
        reader.read_exact(&mut frame)?;

        /*
        The byte becomes an Op here and stays one. An unrecognised code is
        answered rather than dropped, so a mismatched client gets told why.
        */
        let opcode = match Op::try_from(frame[0]) {
            Ok(op) => op,
            Err(code) => {
                reply_error(&mut writer, &format!("unknown opcode {code}"))?;
                writer.flush()?;
                continue;
            }
        };
        if opcode == Op::Close {
            return Ok(());
        }
        dispatch(opcode, &frame[1..], &db, &mut writer)?;
        writer.flush()?;
    }
}

fn dispatch(
    opcode: Op,
    payload: &[u8],
    db: &Arc<Mutex<Database>>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    /*
    Exhaustive over Op, with no catch-all: adding an opcode breaks this match
    until it is handled here, which is the point of the enum.
    */
    match opcode {
        Op::Ping => {
            let mut f = Vec::with_capacity(5);
            put_u32(&mut f, 1);
            f.push(Op::Pong.code());
            out.write_all(&f)
        }
        Op::Insert => match do_insert(payload, db) {
            Ok(n) => reply_affected(out, n as u64),
            Err(e) => reply_error(out, &e),
        },
        Op::Upsert => match do_upsert(payload, db) {
            Ok(n) => reply_affected(out, n as u64),
            Err(e) => reply_error(out, &e),
        },
        Op::Exec => match do_exec(payload, db) {
            Ok(output) => reply_output(out, &output),
            Err(e) => reply_error(out, &e),
        },
        // Close is handled by the read loop, and a response opcode arriving as a
        // request means the peer is confused about which end it is
        Op::Close => Ok(()),
        Op::Affected | Op::Documents | Op::Error | Op::Pong => {
            reply_error(out, &format!("{opcode:?} is a response, not a request"))
        }
    }
}

/*
OP_INSERT: [u32 collection_len][collection][u32 doc_count][document bodies]

The fast path. No lexer, no parser, no binder, no planner: documents are
decoded straight into the Document the storage layer stores, and handed to
insert_docs, which is already the one-handle one-flush batched path.
*/
fn do_insert(payload: &[u8], db: &Arc<Mutex<Database>>) -> Result<usize, String> {
    let mut r = Reader::new(payload);
    let collection = r.str().map_err(|e| e.to_string())?;
    let count = r.u32().map_err(|e| e.to_string())? as usize;

    // a count is bounded by the bytes actually present: an empty document body
    // still costs its 4-byte field count, so this cannot over-reserve.
    if count > r.remaining() {
        return Err("document count exceeds frame".into());
    }

    let mut docs = Vec::with_capacity(count);
    for _ in 0..count {
        docs.push(r.document_body().map_err(|e| e.to_string())?);
    }

    let mut guard = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    guard
        .insert_docs(&collection, &docs)
        .map(|ids| ids.len())
        .map_err(|e| format!("{e:?}"))
}

/*
OP_UPSERT: [u32 collection_len][collection][u32 field_len][field][value][document body]

The same write AS-CORE makes on every save: address one document by a field's
value, and write it whether or not it is there. Binary for the same reason
OP_INSERT is: the values never become ASL text, so nothing has to be escaped
and nothing can be mistaken for syntax.
*/
fn do_upsert(payload: &[u8], db: &Arc<Mutex<Database>>) -> Result<usize, String> {
    let mut r = Reader::new(payload);
    let collection = r.str().map_err(|e| e.to_string())?;
    let field = r.str().map_err(|e| e.to_string())?;
    let value = r.value().map_err(|e| e.to_string())?;
    let doc = r.document_body().map_err(|e| e.to_string())?;

    let mut guard = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    guard
        .upsert_by(&collection, &field, &value, &doc)
        .map_err(|e| format!("{e:?}"))
}

/// OP_EXEC: [u32 len][ASL text]. The general path, same engine as HTTP.
fn do_exec(payload: &[u8], db: &Arc<Mutex<Database>>) -> Result<QueryOutput, String> {
    let mut r = Reader::new(payload);
    let source = r.str().map_err(|e| e.to_string())?;

    // lex and parse OUTSIDE the lock: neither touches the database, and
    // holding a global mutex across work that does not need it is how a
    // serialised engine gets slower than it has to be.
    let tokens = tokenize(&source).map_err(|e| format!("{e:?}"))?;
    let statement = parse(&tokens).map_err(|e| format!("{e:?}"))?;

    let mut guard = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    let db = &mut *guard;

    let bound = bind(db, statement).map_err(|e| e.to_string())?;

    match bound {
        BoundStatement::Pipeline(pipeline) => {
            let physical = plan(db, &pipeline).map_err(|e| e.to_string())?;
            execute(db, &physical).map_err(|e| e.to_string())
        }
        // a guarded create or drop whose object was already in the state asked for
        BoundStatement::NoOp => Ok(QueryOutput::Affected(0)),
        BoundStatement::CreateCollection { name, .. } => db
            .create_collection(&name)
            .map(|()| QueryOutput::Affected(1))
            .map_err(|e| format!("{e:?}")),
        BoundStatement::DropCollection { name } => db
            .drop_collection(&name)
            .map(|()| QueryOutput::Affected(1))
            .map_err(|e| format!("{e:?}")),
        BoundStatement::CreateIndex { collection, fields, unique } => db
            .create_index_on(&collection, &fields, unique)
            .map(|()| QueryOutput::Affected(1))
            .map_err(|e| format!("{e:?}")),
        BoundStatement::DropIndex { collection, field } => db
            .drop_index(&collection, &field)
            .map(|()| QueryOutput::Affected(1))
            .map_err(|e| format!("{e:?}")),
    }
}

/* ---------- replies ---------- */

fn frame(opcode: Op, body: Vec<u8>) -> Vec<u8> {
    let mut f = Vec::with_capacity(body.len() + 5);
    put_u32(&mut f, (body.len() + 1) as u32);
    f.push(opcode.code());
    f.extend_from_slice(&body);
    f
}

fn reply_affected(out: &mut impl Write, n: u64) -> std::io::Result<()> {
    out.write_all(&frame(Op::Affected, n.to_le_bytes().to_vec()))
}

fn reply_error(out: &mut impl Write, msg: &str) -> std::io::Result<()> {
    let mut body = Vec::new();
    put_str(&mut body, msg);
    out.write_all(&frame(Op::Error, body))
}

fn reply_output(out: &mut impl Write, output: &QueryOutput) -> std::io::Result<()> {
    match output {
        QueryOutput::Affected(n) => reply_affected(out, *n as u64),
        QueryOutput::Documents(docs) => {
            let mut body = Vec::new();
            put_u32(&mut body, docs.len() as u32);
            for d in docs {
                put_document_body(&mut body, d);
            }
            out.write_all(&frame(Op::Documents, body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Value};
    use std::io::BufReader;

    /// A server on an ephemeral port with its own temp database.
    struct Harness {
        addr: String,
        path: std::path::PathBuf,
    }

    impl Harness {
        fn start(tag: &str) -> Harness {
            let path = std::env::temp_dir()
                .join(format!("abp-{tag}-{}-{:?}.db", std::process::id(), std::thread::current().id()));
            let _ = std::fs::remove_file(&path);
            let db = Database::create(&path).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let shared = Arc::new(Mutex::new(db));
            std::thread::spawn(move || BinServer::new(shared).serve(listener));
            Harness { addr, path }
        }

        fn client(&self) -> Client {
            Client::connect(&self.addr)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct Client {
        r: BufReader<TcpStream>,
        w: TcpStream,
    }

    impl Client {
        fn connect(addr: &str) -> Client {
            let s = TcpStream::connect(addr).unwrap();
            s.set_nodelay(true).unwrap();
            Client { r: BufReader::new(s.try_clone().unwrap()), w: s }
        }

        // for the one test that has to put a code on the wire that Op rejects
        fn send_raw(&mut self, code: u8, body: &[u8]) {
            let mut f = Vec::new();
            put_u32(&mut f, (body.len() + 1) as u32);
            f.push(code);
            f.extend_from_slice(body);
            self.w.write_all(&f).unwrap();
        }

        fn send(&mut self, opcode: Op, body: &[u8]) {
            let mut f = Vec::new();
            put_u32(&mut f, (body.len() + 1) as u32);
            f.push(opcode.code());
            f.extend_from_slice(body);
            self.w.write_all(&f).unwrap();
        }

        /// (opcode, payload). None when the server closed the connection.
        fn recv(&mut self) -> Option<(Op, Vec<u8>)> {
            let mut len = [0u8; 4];
            self.r.read_exact(&mut len).ok()?;
            let n = u32::from_le_bytes(len) as usize;
            let mut buf = vec![0u8; n];
            self.r.read_exact(&mut buf).ok()?;
            Some((Op::try_from(buf[0]).expect("server sent an unknown opcode"), buf[1..].to_vec()))
        }

        fn exec(&mut self, source: &str) -> (Op, Vec<u8>) {
            let mut body = Vec::new();
            put_str(&mut body, source);
            self.send(Op::Exec, &body);
            self.recv().expect("server closed")
        }

        fn insert(&mut self, collection: &str, docs: &[Document]) -> (Op, Vec<u8>) {
            let mut body = Vec::new();
            put_str(&mut body, collection);
            put_u32(&mut body, docs.len() as u32);
            for d in docs {
                put_document_body(&mut body, d);
            }
            self.send(Op::Insert, &body);
            self.recv().expect("server closed")
        }

        fn affected(&mut self, reply: (Op, Vec<u8>)) -> u64 {
            let (op, payload) = reply;
            assert_eq!(op, Op::Affected, "expected affected, got {}", describe(op, &payload));
            u64::from_le_bytes(payload[..8].try_into().unwrap())
        }

        fn documents(&mut self, reply: (Op, Vec<u8>)) -> Vec<Document> {
            let (op, payload) = reply;
            assert_eq!(op, Op::Documents, "expected documents, got {}", describe(op, &payload));
            let mut r = Reader::new(&payload);
            let n = r.u32().unwrap() as usize;
            (0..n).map(|_| r.document_body().unwrap()).collect()
        }
    }

    fn describe(op: Op, payload: &[u8]) -> String {
        if op == Op::Error {
            format!("error: {}", Reader::new(payload).str().unwrap())
        } else {
            format!("{op:?}")
        }
    }

    fn doc(place: &str) -> Document {
        let mut d = Document::new();
        d.insert("placeId".into(), Value::String(place.into()));
        d.insert("playerCount".into(), Value::Int(42));
        d
    }

    #[test]
    fn test_binary_upsert_inserts_then_merges_without_a_socket() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut database = Database::create(tmp.path()).unwrap();
        database.create_collection("nodes").unwrap();
        database.create_index("nodes", "nodeId").unwrap();
        let db = Arc::new(Mutex::new(database));

        let mut initial = Document::new();
        initial.insert("nodeId".into(), Value::String("n1".into()));
        initial.insert("load".into(), Value::Int(3));
        let mut first = Vec::new();
        put_str(&mut first, "nodes");
        put_str(&mut first, "nodeId");
        put_value(&mut first, &Value::String("n1".into()));
        put_document_body(&mut first, &initial);
        assert_eq!(do_upsert(&first, &db).unwrap(), 1);

        let mut patch = Document::new();
        patch.insert("nodeId".into(), Value::String("n1".into()));
        patch.insert("load".into(), Value::Int(9));
        let mut second = Vec::new();
        put_str(&mut second, "nodes");
        put_str(&mut second, "nodeId");
        put_value(&mut second, &Value::String("n1".into()));
        put_document_body(&mut second, &patch);
        assert_eq!(do_upsert(&second, &db).unwrap(), 1);

        let rows = db.lock().unwrap().scan_collection("nodes").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1["load"], Value::Int(9));
    }

    #[test]
    fn test_ping_answers_pong() {
        let h = Harness::start("ping");
        let mut c = h.client();
        c.send(Op::Ping, &[]);
        assert_eq!(c.recv().unwrap().0, Op::Pong);
    }

    #[test]
    fn test_binary_insert_is_readable_by_asl() {
        // the two paths are the same database: what Op::Insert writes, a text
        // query must find. This is the check that the fast path is a shortcut
        // and not a separate store.
        let h = Harness::start("insert");
        let mut c = h.client();
        c.exec("create t {}");

        let reply = c.insert("t", &[doc("place-1"), doc("place-2")]);
        assert_eq!(c.affected(reply), 2);

        let reply = c.exec("from t | where placeId == \"place-2\"");
        let docs = c.documents(reply);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["playerCount"], Value::Int(42));
    }

    #[test]
    fn test_one_connection_serves_many_requests() {
        // the entire point of the protocol: no reconnect between requests.
        let h = Harness::start("persist");
        let mut c = h.client();
        c.exec("create t {}");
        for i in 0..200 {
            let reply = c.insert("t", &[doc(&format!("p{i}"))]);
            assert_eq!(c.affected(reply), 1);
        }
        let reply = c.exec("from t");
        assert_eq!(c.documents(reply).len(), 200);
    }

    #[test]
    fn test_pipelined_requests_reply_in_order() {
        /*
        Frames are length-prefixed and replies are written in the order the
        requests arrived, so a client can send many before reading any. That
        is what makes pipelining work with no request ids and no correlation
        field, and pipelining is worth 13 us per insert (see PROTOCOL.txt).
        This test is what makes that property something the protocol PROMISES
        rather than something that happens to be true today.
        */
        let h = Harness::start("pipeline");
        let mut c = h.client();
        c.exec("create t {}");

        let mut bodies = Vec::new();
        for i in 0..64 {
            let mut body = Vec::new();
            put_str(&mut body, "t");
            put_u32(&mut body, 1);
            put_document_body(&mut body, &doc(&format!("p{i}")));
            bodies.push(body);
        }
        for body in &bodies {
            c.send(Op::Insert, body);
        }
        for _ in 0..64 {
            let reply = c.recv().expect("server closed mid-pipeline");
            assert_eq!(c.affected(reply), 1);
        }
    }

    #[test]
    fn test_error_does_not_kill_the_connection() {
        /*
        A connection that dies on a bad statement would force a reconnect and
        hand back the 30 us this protocol exists to remove. Errors are a frame,
        not a hangup.
        */
        let h = Harness::start("err");
        let mut c = h.client();
        c.exec("create t {}");

        let (op, payload) = c.exec("this is not asl");
        assert_eq!(op, Op::Error, "expected an error frame");
        assert!(!payload.is_empty());

        let reply = c.insert("t", &[doc("still-here")]);
        assert_eq!(c.affected(reply), 1, "connection must survive an error");
    }

    #[test]
    fn test_unknown_opcode_is_reported_not_fatal() {
        let h = Harness::start("opcode");
        let mut c = h.client();
        c.send_raw(0x7f, &[]);
        let (op, payload) = c.recv().unwrap();
        assert_eq!(op, Op::Error);
        assert!(Reader::new(&payload).str().unwrap().contains("127"));
        c.send(Op::Ping, &[]);
        assert_eq!(c.recv().unwrap().0, Op::Pong);
    }

    #[test]
    fn test_insert_into_missing_collection_is_an_error_frame() {
        let h = Harness::start("missing");
        let mut c = h.client();
        let (op, _) = c.insert("nope", &[doc("x")]);
        assert_eq!(op, Op::Error);
    }

    #[test]
    fn test_close_ends_the_connection() {
        let h = Harness::start("close");
        let mut c = h.client();
        c.send(Op::Close, &[]);
        assert!(c.recv().is_none(), "Op::Close should end the stream");
    }

    #[test]
    fn test_oversized_frame_is_rejected_without_allocating() {
        // a length header claiming more than MAX_FRAME must be refused on the
        // header alone, before the server waits for or reserves that many bytes.
        let h = Harness::start("huge");
        let mut c = h.client();
        c.w.write_all(&(u32::MAX).to_le_bytes()).unwrap();
        let (op, payload) = c.recv().unwrap();
        assert_eq!(op, Op::Error);
        assert!(Reader::new(&payload).str().unwrap().contains("bad frame length"));
    }

    #[test]
    fn test_values_cannot_become_syntax() {
        /*
        THE SECURITY CLAIM, tested rather than asserted.

        On the ASL text path this payload is exactly what AsdbEntityMapper.quote
        exists to defend against: it would close the string, close the document
        and append a delete stage. On the binary path the value arrives as a
        length-prefixed byte run that is never lexed, so there is no escaping to
        get wrong. The bytes come back out identical and the collection is
        still there.
        */
        let h = Harness::start("inject");
        let mut c = h.client();
        c.exec("create t {}");

        let hostile = "x\" } | delete //";
        let mut d = Document::new();
        d.insert("placeId".into(), Value::String(hostile.into()));
        // a field NAME that is an ASL keyword, which the text path has to backtick
        d.insert("order".into(), Value::Int(1));
        c.insert("t", &[d]);

        let reply = c.exec("from t");
        let docs = c.documents(reply);
        assert_eq!(docs.len(), 1, "the delete stage must not have run");
        assert_eq!(docs[0]["placeId"], Value::String(hostile.into()));
        assert_eq!(docs[0]["order"], Value::Int(1));
    }

    #[test]
    fn test_concurrent_connections_share_one_database() {
        let h = Harness::start("threads");
        h.client().exec("create t {}");

        let handles: Vec<_> = (0..8)
            .map(|t| {
                let addr = h.addr.clone();
                std::thread::spawn(move || {
                    let mut c = Client::connect(&addr);
                    for i in 0..25 {
                        let reply = c.insert("t", &[doc(&format!("t{t}-{i}"))]);
                        assert_eq!(c.affected(reply), 1);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut c = h.client();
        let reply = c.exec("from t");
        assert_eq!(c.documents(reply).len(), 200);
    }
}
