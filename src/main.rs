/*
main.rs: run asdb as a server.

    asdb <database-file> [--port N] [--bind ADDR] [--ttl coll.field=7d]...

Before this, the binary printed its own name. asdb was usable only by linking
it into another Rust program, which ruled out the intended consumer: a Spring
Boot backend on the JVM. This makes it a process something else can talk to.

    asdb telemetry.db --port 7070 --ttl telemtry_snapshots.receivedAt=7d

BINDS TO LOCALHOST BY DEFAULT, deliberately. There is no authentication and no
TLS (see server.rs), so anyone who can reach the port can read and delete
everything. Exposing it takes an explicit --bind, so that is a decision
someone makes rather than a default they inherit.
*/

use asdb::binserver::BinServer;
use asdb::database::Database;
use asdb::server::Server;
use asdb::ttl::{Policy, Sweeper};

use std::net::TcpListener;
use std::time::Duration;

const DEFAULT_PORT: u16 = 7070;
/*
The binary protocol listens on its own port alongside HTTP rather than
replacing it. Same process, same Database, same file: only the framing
differs. Keeping HTTP alive means curl still works for debugging and a client
can move one call site at a time instead of all at once.
*/
const DEFAULT_ABP_PORT: u16 = 7071;
const DEFAULT_BIND: &str = "127.0.0.1";

/// How often the TTL sweeper runs. Mongo's own TTL monitor uses 60s.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match Config::parse(&args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("error: {message}\n");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let db = match Database::create(&config.path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: could not open {}: {err:?}", config.path);
            std::process::exit(1);
        }
    };

    let server = Server::new(db);

    // The sweeper shares the same Database behind the same mutex, so its
    // deletes serialize with incoming queries rather than racing them.
    if !config.policies.is_empty() {
        let mut sweeper = Sweeper::new(SWEEP_INTERVAL);
        for policy in config.policies {
            println!(
                "ttl: {}.{} after {}s",
                policy.collection,
                policy.field,
                policy.ttl.as_secs()
            );
            sweeper = sweeper.with_policy(policy);
        }
        sweeper.spawn(server.database());
    }

    // ABP first, on its own thread, sharing the Arc the HTTP server already
    // holds. Failing to bind it is fatal rather than a warning: a client
    // configured for the binary protocol falling back to nothing silently is
    // worse than not starting.
    if config.abp_port != 0 {
        let abp_address = format!("{}:{}", config.bind, config.abp_port);
        match TcpListener::bind(&abp_address) {
            Ok(listener) => {
                let db = server.database();
                std::thread::spawn(move || BinServer::new(db).serve(listener));
                println!("asdb listening on abp://{abp_address}  (binary protocol, ABP/1)");
            }
            Err(err) => {
                eprintln!("error: could not bind {abp_address}: {err}");
                std::process::exit(1);
            }
        }
    }

    let address = format!("{}:{}", config.bind, config.port);
    let listener = match TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("error: could not bind {address}: {err}");
            std::process::exit(1);
        }
    };

    println!("asdb listening on http://{address}  (db: {})", config.path);
    println!("  POST /query   body is an ASL statement");
    println!("  GET  /health");
    if config.bind != DEFAULT_BIND {
        println!("WARNING: bound beyond localhost with no authentication or TLS.");
    }

    server.serve(listener);
}

const USAGE: &str = "\
usage: asdb <database-file> [options]

options:
  --port N                 HTTP port to listen on (default 7070)
  --abp-port N             binary protocol port (default 7071, 0 disables)
  --bind ADDR              address to bind (default 127.0.0.1)
  --ttl COLL.FIELD=SPEC    expire documents older than SPEC.
                           SPEC is Ns, Nm, Nh or Nd, matching Spring Data's
                           @Indexed(expireAfter = \"...\") spelling.
                           May be given more than once.

example:
  asdb telemetry.db --port 7070 --ttl telemtry_snapshots.receivedAt=7d";

struct Config {
    path: String,
    port: u16,
    abp_port: u16,
    bind: String,
    policies: Vec<Policy>,
}

impl Config {
    /*
    Hand-rolled, because asdb has no dependencies and this is four options.
    Every failure names the offending argument rather than returning a generic
    parse error: a mistyped TTL spec that silently defaulted would quietly
    expire data on the wrong schedule, which is the kind of bug nobody notices
    until the data is gone.
    */
    fn parse(args: &[String]) -> Result<Config, String> {
        let mut path = None;
        let mut port = DEFAULT_PORT;
        let mut abp_port = DEFAULT_ABP_PORT;
        let mut bind = DEFAULT_BIND.to_string();
        let mut policies = Vec::new();

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--port" => {
                    let raw = args.get(i + 1).ok_or("--port needs a value")?;
                    port = raw.parse().map_err(|_| format!("bad port: {raw}"))?;
                    i += 2;
                }
                "--abp-port" => {
                    let raw = args.get(i + 1).ok_or("--abp-port needs a value")?;
                    abp_port = raw.parse().map_err(|_| format!("bad abp port: {raw}"))?;
                    i += 2;
                }
                "--bind" => {
                    bind = args.get(i + 1).ok_or("--bind needs a value")?.clone();
                    i += 2;
                }
                "--ttl" => {
                    let raw = args.get(i + 1).ok_or("--ttl needs a value")?;
                    policies.push(parse_ttl(raw)?);
                    i += 2;
                }
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"))
                }
                other => {
                    if path.is_some() {
                        return Err(format!("unexpected extra argument: {other}"));
                    }
                    path = Some(other.to_string());
                    i += 1;
                }
            }
        }

        Ok(Config {
            path: path.ok_or("no database file given")?,
            port,
            abp_port,
            bind,
            policies,
        })
    }
}

/// `collection.field=7d` -> a Policy.
fn parse_ttl(spec: &str) -> Result<Policy, String> {
    let (target, duration) = spec
        .split_once('=')
        .ok_or_else(|| format!("bad --ttl {spec:?}, expected COLLECTION.FIELD=DURATION"))?;
    let (collection, field) = target
        .split_once('.')
        .ok_or_else(|| format!("bad --ttl target {target:?}, expected COLLECTION.FIELD"))?;
    let ttl = Policy::parse_duration(duration)
        .ok_or_else(|| format!("bad --ttl duration {duration:?}, expected Ns, Nm, Nh or Nd"))?;
    Ok(Policy::new(collection, field, ttl))
}
