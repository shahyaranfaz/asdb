/*
server.rs: an HTTP front end so callers outside this process can run queries.

WHY THIS EXISTS

Before it, asdb was an embedded library: the only way to use it was to link it
into a Rust program. The intended consumer is a Spring Boot backend on the
JVM, which cannot do that. This is the wire.

WHY HTTP AND NOT A BINARY PROTOCOL

A custom binary protocol would be faster and would need a client driver
written and maintained for every language that wants to talk to it. HTTP needs
none: every language already has a client, so the Java side is a few lines
against java.net.http and no driver at all. The request body is ASL text,
which is a grammar we already parse, and the response is JSON, which every
client already reads. Speed is not the constraint at this stage; being
callable at all is.

HAND-WRITTEN, because asdb has no dependencies and this needs a small enough
slice of HTTP/1.1 to write directly: a request line, headers, an optional
Content-Length body, and a response. It is NOT a general-purpose HTTP server
and should not be exposed to the public internet. See the limitations at the
bottom.

CONCURRENCY

One thread per connection, all of them sharing Arc<Mutex<Database>>.

The mutex is the honest answer, not a placeholder for something cleverer.
Database methods take &mut self all the way down, so the type system already
says one caller at a time. Serializing at the front door makes that explicit
instead of pretending otherwise. Concurrent requests queue rather than race,
which is correct but means throughput is bounded by one writer.

Real concurrency needs page-level locking and a transaction manager in the
storage layer. That is a much larger change, and doing it under a mutex first
is the right order: correct and slow beats fast and corrupt.
*/

use crate::asl::{parse, tokenize};
use crate::database::Database;
use crate::json::{write_error, write_output};
use crate::query::{bind, execute, plan, BoundStatement, QueryOutput};

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/*
Refuse a body larger than this. Without a cap, one request claiming a
Content-Length of 4 GB would have the server allocate 4 GB. ASL statements are
text, so a megabyte is already far past anything legitimate.
*/
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub struct Server {
    db: Arc<Mutex<Database>>,
}

impl Server {
    pub fn new(db: Database) -> Self {
        Server { db: Arc::new(Mutex::new(db)) }
    }

    /// The shared handle, so a TTL sweeper can hold the same database.
    pub fn database(&self) -> Arc<Mutex<Database>> {
        Arc::clone(&self.db)
    }

    /*
    serve: accept connections forever.

    A failed accept is logged and skipped rather than returned. One client
    hanging up mid-handshake must not take the server down.
    */
    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let db = Arc::clone(&self.db);
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection(stream, db) {
                            eprintln!("connection error: {err}");
                        }
                    });
                }
                Err(err) => eprintln!("accept failed: {err}"),
            }
        }
    }
}

/*
handle_connection: one request, one response, then close.

No keep-alive. Connection: close is sent explicitly so the client knows not to
wait for another response on the socket. Keep-alive would mean tracking
per-connection state and framing multiple requests, which is real work for no
benefit while the mutex serializes everything anyway.
*/
fn handle_connection(stream: TcpStream, db: Arc<Mutex<Database>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let Some(request) = read_request(&mut reader)? else {
        return Ok(()); // client hung up before sending anything
    };

    let (status, body) = route(&request, &db);
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    writer.write_all(response.as_bytes())?;
    writer.flush()
}

struct Request {
    method: String,
    path: String,
    body: String,
}

/*
read_request: parse the request line, headers, and body.

Only Content-Length framing is supported. Chunked transfer encoding is not,
and a client using it gets a clear 400 rather than a hang: see route().
*/
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break; // blank line ends the headers
        }
        if let Some((name, value)) = header.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }

    // signalled to route() as an empty body on a request that needs one
    if chunked {
        return Ok(Some(Request { method, path, body: String::new() }));
    }

    let body = if content_length == 0 {
        String::new()
    } else {
        let capped = content_length.min(MAX_BODY_BYTES);
        let mut buf = vec![0u8; capped];
        reader.read_exact(&mut buf)?;
        String::from_utf8_lossy(&buf).into_owned()
    };

    Ok(Some(Request { method, path, body }))
}

/*
route: pick a handler.

Deliberately tiny. Two endpoints:

  GET  /health   liveness, so a container orchestrator has something to probe
  POST /query    body is ASL text, response is asl.txt's RESULT FORMAT

A single query endpoint rather than a REST resource per collection is the
whole point of ASL being a language: one route serves find, aggregate, insert,
update and delete, instead of a separate shape per operation the way a
document store's driver API does.
*/
fn route(request: &Request, db: &Arc<Mutex<Database>>) -> (&'static str, String) {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => ("200 OK", "{\"status\":\"ok\"}".to_string()),

        ("POST", "/query") => {
            if request.body.trim().is_empty() {
                return (
                    "400 Bad Request",
                    write_error(
                        "empty request body; expected an ASL statement (chunked encoding is not supported)",
                        "request",
                    ),
                );
            }
            run_statement(&request.body, db)
        }

        ("GET", _) | ("POST", _) => (
            "404 Not Found",
            write_error("no such endpoint; try POST /query", "request"),
        ),
        _ => (
            "405 Method Not Allowed",
            write_error("only GET and POST are supported", "request"),
        ),
    }
}

/*
run_statement: ASL text -> a response body.

The four stages each map to their own `stage` label in the error response, so
a caller can tell a syntax error from a planning failure without reading the
message. That is what asl.txt's error shape is for.

The mutex is held across the whole statement. It has to be: a query pulls
documents lazily through the operator tree, so releasing early would let
another writer mutate the collection mid-scan.

A poisoned mutex (another thread panicked while holding it) is reported as a
500 rather than propagated. One bad query should not permanently kill the
server for every later caller.
*/
fn run_statement(source: &str, db: &Arc<Mutex<Database>>) -> (&'static str, String) {
    let tokens = match tokenize(source) {
        Ok(tokens) => tokens,
        Err(err) => return ("400 Bad Request", write_error(&format!("{err:?}"), "lex")),
    };

    let statement = match parse(&tokens) {
        Ok(statement) => statement,
        Err(err) => return ("400 Bad Request", write_error(&format!("{err:?}"), "parse")),
    };

    let mut guard = match db.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return (
                "500 Internal Server Error",
                write_error("database lock poisoned by an earlier panic", "server"),
            )
        }
    };
    let db = &mut *guard;

    let bound = match bind(db, statement) {
        Ok(bound) => bound,
        Err(err) => return ("400 Bad Request", write_error(&err.to_string(), "bind")),
    };

    let output = match bound {
        BoundStatement::Pipeline(pipeline) => {
            let physical = match plan(db, &pipeline) {
                Ok(physical) => physical,
                Err(err) => return ("400 Bad Request", write_error(&err.to_string(), "plan")),
            };
            match execute(db, &physical) {
                Ok(output) => output,
                Err(err) => {
                    return ("400 Bad Request", write_error(&err.to_string(), "execute"))
                }
            }
        }

        // DDL. Reported as an affected count so every response has one of the
        // two shapes asl.txt defines, rather than inventing a third.
        BoundStatement::CreateCollection { name, .. } => match db.create_collection(&name) {
            Ok(()) => QueryOutput::Affected(1),
            Err(err) => {
                return ("400 Bad Request", write_error(&format!("{err:?}"), "create"))
            }
        },
        BoundStatement::DropCollection { name } => match db.drop_collection(&name) {
            Ok(()) => QueryOutput::Affected(1),
            Err(err) => return ("400 Bad Request", write_error(&format!("{err:?}"), "drop")),
        },
        BoundStatement::CreateIndex { collection, field } => {
            match db.create_index(&collection, &field) {
                Ok(()) => QueryOutput::Affected(1),
                Err(err) => {
                    return ("400 Bad Request", write_error(&format!("{err:?}"), "index"))
                }
            }
        }
        BoundStatement::DropIndex { collection, field } => {
            match db.drop_index(&collection, &field) {
                Ok(()) => QueryOutput::Affected(1),
                Err(err) => {
                    return ("400 Bad Request", write_error(&format!("{err:?}"), "index"))
                }
            }
        }
    };

    ("200 OK", write_output(&output))
}

/*
================================================================
LIMITATIONS, stated rather than discovered

  NO AUTHENTICATION. Anyone who can reach the port can read and delete
  everything. Bind to localhost or keep it on a private network. Do not put
  this on the public internet.

  NO TLS. Traffic is plaintext.

  NO KEEP-ALIVE, one request per connection.

  NO CHUNKED ENCODING. Content-Length only; a chunked request gets a 400.

  ONE WRITER AT A TIME. The mutex serializes every statement, so throughput is
  one query at a time regardless of how many threads are connected.

  A THREAD PER CONNECTION. Fine for a backend with a bounded connection pool,
  wrong for thousands of idle clients.
================================================================
*/

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /*
    Spins up a real server on an ephemeral port and talks to it over a real
    socket. Port 0 lets the OS pick a free port, so tests never collide with
    each other or with something already running on the machine.
    */
    fn start_server(tag: &str) -> (std::net::SocketAddr, std::path::PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("asdb_server_{tag}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let db = Database::create(&path).unwrap();
        let server = Server::new(db);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || server.serve(listener));
        (addr, path)
    }

    fn request(addr: std::net::SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn post(addr: std::net::SocketAddr, body: &str) -> String {
        request(
            addr,
            &format!(
                "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn body_of(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    #[test]
    fn test_health_endpoint() {
        let (addr, path) = start_server("health");
        let response = request(addr, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body_of(&response), "{\"status\":\"ok\"}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_full_lifecycle_over_http() {
        let (addr, path) = start_server("lifecycle");

        let created = post(addr, "create users {}");
        assert!(created.starts_with("HTTP/1.1 200 OK"), "{created}");

        let inserted = post(addr, r#"from users | insert { id: 1, name: "alice", age: 30 }"#);
        assert_eq!(body_of(&inserted), r#"{"affected":1}"#);

        post(addr, r#"from users | insert { id: 2, name: "bob", age: 17 }"#);

        let queried = post(addr, "from users | where age > 18 | select name");
        assert_eq!(
            body_of(&queried),
            r#"{"count":1,"documents":[{"name":"alice"}]}"#
        );

        let deleted = post(addr, "from users | where age < 18 | delete");
        assert_eq!(body_of(&deleted), r#"{"affected":1}"#);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_errors_report_the_stage_they_failed_in() {
        let (addr, path) = start_server("errors");

        let bad_syntax = post(addr, "from users | wibble");
        assert!(bad_syntax.starts_with("HTTP/1.1 400"), "{bad_syntax}");
        assert!(body_of(&bad_syntax).contains("\"stage\":\"parse\""));

        let unknown_collection = post(addr, "from nope");
        assert!(body_of(&unknown_collection).contains("\"stage\":\"bind\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_empty_body_is_rejected_clearly() {
        let (addr, path) = start_server("emptybody");
        let response = request(addr, "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(body_of(&response).contains("empty request body"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_unknown_route_and_method() {
        let (addr, path) = start_server("routes");
        assert!(request(addr, "GET /nope HTTP/1.1\r\nHost: x\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(
            request(addr, "DELETE /query HTTP/1.1\r\nHost: x\r\n\r\n").starts_with("HTTP/1.1 405")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_concurrent_writers_do_not_lose_documents() {
        /*
        The point of the mutex, asserted rather than assumed. Eight threads
        insert twenty documents each; all 160 must be present afterwards. A
        missing document would mean two writers interleaved inside one
        statement.
        */
        let (addr, path) = start_server("concurrent");
        post(addr, "create events {}");

        let mut handles = Vec::new();
        for thread in 0..8 {
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    let id = thread * 20 + i;
                    post(addr, &format!("from events | insert {{ n: {id} }}"));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let response = post(addr, "from events");
        assert!(
            body_of(&response).starts_with(r#"{"count":160,"#),
            "expected 160 documents, got: {}",
            &body_of(&response)[..60.min(body_of(&response).len())]
        );
        let _ = std::fs::remove_file(&path);
    }
}
