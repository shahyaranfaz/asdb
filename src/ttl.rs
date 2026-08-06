/*
ttl.rs: background expiry of aged-out documents.

WHY THIS EXISTS

MongoDB has TTL indexes: mark a field, give it an age, and the server deletes
documents once they pass it. Spring Data spells that
`@Indexed(expireAfter = "7d")`, and SHAYVERI's TelemetrySnapshot depends on it
to keep telemetry from growing without bound.

asdb has no equivalent, so without this, swapping asdb in for Mongo would be a
silent functional regression: the annotation would still compile on the Java
side and simply stop meaning anything, and the collection would grow forever.
That is the worst kind of migration bug, because nothing fails, it just gets
slower for months.

HOW IT WORKS

A background thread that periodically runs, for each registered policy:

    from <collection> | where <field> < <cutoff> | delete

That is it. It is not a new storage feature, it is a scheduled query, which is
exactly what asl.txt already suggested for this case. Building it as a sweeper
rather than as an index property keeps the storage layer untouched.

HOW IT DIFFERS FROM MONGO, and callers need to know both:

  GRANULARITY. Mongo's TTL monitor runs every 60 seconds; this runs on
  whatever interval it is given. Either way a document lives somewhat PAST
  its expiry, up to one sweep interval. Neither is a precise deadline, so
  anything requiring exact expiry has to filter on the timestamp itself in
  the query, not rely on the sweeper having run.

  TIME REPRESENTATION. asdb has no date type. Timestamps are stored as
  epoch-millis Int, which is what the Java mapper already converts Instant
  into, so a cutoff is plain integer comparison. This is why the policy takes
  a field name rather than inferring anything.

  NO PARTIAL INDEX SUPPORT, and expiry applies to the whole collection.
*/

use crate::asl::{parse, tokenize};
use crate::database::Database;
use crate::query::{bind, execute, plan, BoundStatement, QueryOutput};

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/*
Policy: expire documents in `collection` whose `field` is older than `ttl`.

`field` must hold epoch millis as an Int. A document missing the field is
NEVER expired: `where receivedAt < cutoff` is false for a missing field,
because a comparison against Null has no ordering. That is the safe default,
since a document with no timestamp has no demonstrated age.
*/
#[derive(Clone, Debug)]
pub struct Policy {
    pub collection: String,
    pub field: String,
    pub ttl: Duration,
}

impl Policy {
    pub fn new(collection: impl Into<String>, field: impl Into<String>, ttl: Duration) -> Self {
        Policy { collection: collection.into(), field: field.into(), ttl }
    }

    /*
    parse_duration: the Spring Data spelling, so a policy can be built
    straight from an @Indexed(expireAfter = "7d") value without the Java side
    having to translate it.

    Accepts s, m, h, d. Returns None for anything else rather than guessing,
    because silently treating "7" as 7 seconds when the author meant 7 days is
    the kind of mistake that only shows up as missing data.
    */
    pub fn parse_duration(spec: &str) -> Option<Duration> {
        let spec = spec.trim();
        let (digits, unit) = spec.split_at(spec.len().checked_sub(1)?);
        let n: u64 = digits.parse().ok()?;
        let seconds = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 60 * 60,
            "d" => n * 60 * 60 * 24,
            _ => return None,
        };
        Some(Duration::from_secs(seconds))
    }

    /// The ASL this policy runs, given a cutoff in epoch millis.
    fn statement(&self, cutoff_millis: i64) -> String {
        format!(
            "from {} | where {} < {} | delete",
            self.collection, self.field, cutoff_millis
        )
    }
}

/*
Sweeper: the policies plus the interval between passes.

Owns nothing but configuration. The database is passed in when it runs, so a
sweeper can be built and inspected without one, and tested by running a single
pass synchronously instead of waiting on a thread.
*/
pub struct Sweeper {
    policies: Vec<Policy>,
    interval: Duration,
}

impl Sweeper {
    pub fn new(interval: Duration) -> Self {
        Sweeper { policies: Vec::new(), interval }
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policies.push(policy);
        self
    }

    pub fn policies(&self) -> &[Policy] {
        &self.policies
    }

    /*
    sweep_once: run every policy a single time, returning how many documents
    were deleted in total.

    Separate from the thread loop on purpose. A background task that can only
    be observed by waiting for a timer is close to untestable; this is the
    same work, callable directly.

    A failing policy is logged and skipped rather than aborting the pass. One
    collection that has been dropped out from under its policy must not stop
    the others from being swept.
    */
    pub fn sweep_once(&self, db: &Arc<Mutex<Database>>) -> usize {
        let Some(now) = now_millis() else {
            eprintln!("ttl: system clock is before the unix epoch, skipping sweep");
            return 0;
        };

        let mut removed = 0;
        for policy in &self.policies {
            let cutoff = now - policy.ttl.as_millis() as i64;
            match run(db, &policy.statement(cutoff)) {
                Ok(n) => removed += n,
                Err(err) => eprintln!(
                    "ttl: sweep of {}.{} failed: {err}",
                    policy.collection, policy.field
                ),
            }
        }
        removed
    }

    /*
    spawn: run sweeps forever on a background thread.

    Sleeps FIRST so startup is not blocked by a sweep, and so a service that
    restarts frequently does not hammer the database with a pass on every
    boot.

    The handle is returned rather than detached so a caller can join it, but
    in practice the process outlives it.
    */
    pub fn spawn(self, db: Arc<Mutex<Database>>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || loop {
            std::thread::sleep(self.interval);
            let removed = self.sweep_once(&db);
            if removed > 0 {
                println!("ttl: expired {removed} documents");
            }
        })
    }
}

fn now_millis() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// Run one ASL statement and report how many documents it affected.
fn run(db: &Arc<Mutex<Database>>, source: &str) -> Result<usize, String> {
    let tokens = tokenize(source).map_err(|e| format!("{e:?}"))?;
    let statement = parse(&tokens).map_err(|e| format!("{e:?}"))?;

    let mut guard = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    let db = &mut *guard;

    let bound = bind(db, statement).map_err(|e| e.to_string())?;
    let BoundStatement::Pipeline(pipeline) = bound else {
        return Err("ttl statement must be a pipeline".to_string());
    };
    let physical = plan(db, &pipeline).map_err(|e| e.to_string())?;

    match execute(db, &physical).map_err(|e| e.to_string())? {
        QueryOutput::Affected(n) => Ok(n),
        QueryOutput::Documents(docs) => Ok(docs.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Value};

    fn temp_db(tag: &str) -> (Arc<Mutex<Database>>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("asdb_ttl_{tag}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(&path).unwrap();
        (Arc::new(Mutex::new(db)), path)
    }

    fn snapshot(at_millis: i64) -> Document {
        let mut doc = Document::new();
        doc.insert("receivedAt".to_string(), Value::Int(at_millis));
        doc
    }

    #[test]
    fn test_parse_duration_matches_the_spring_spelling() {
        assert_eq!(Policy::parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(Policy::parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(Policy::parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(Policy::parse_duration("7d"), Some(Duration::from_secs(604_800)));
    }

    #[test]
    fn test_parse_duration_rejects_rather_than_guesses() {
        // "7" could mean anything; assuming seconds would silently expire data
        // a thousand times sooner than intended.
        assert_eq!(Policy::parse_duration("7"), None);
        assert_eq!(Policy::parse_duration("7w"), None);
        assert_eq!(Policy::parse_duration(""), None);
        assert_eq!(Policy::parse_duration("d"), None);
    }

    #[test]
    fn test_sweep_removes_only_aged_documents() {
        let (db, path) = temp_db("expiry");
        {
            let mut guard = db.lock().unwrap();
            guard.create_collection("snapshots").unwrap();

            let now = now_millis().unwrap();
            let hour = 3_600_000;
            // two well past a 7-day ttl, two comfortably inside it
            guard.insert_doc("snapshots", &snapshot(now - 8 * 24 * hour)).unwrap();
            guard.insert_doc("snapshots", &snapshot(now - 9 * 24 * hour)).unwrap();
            guard.insert_doc("snapshots", &snapshot(now - hour)).unwrap();
            guard.insert_doc("snapshots", &snapshot(now)).unwrap();
        }

        let sweeper = Sweeper::new(Duration::from_secs(60)).with_policy(Policy::new(
            "snapshots",
            "receivedAt",
            Duration::from_secs(7 * 24 * 3600),
        ));

        assert_eq!(sweeper.sweep_once(&db), 2, "exactly the two aged documents");

        let remaining = db.lock().unwrap().scan_collection("snapshots").unwrap();
        assert_eq!(remaining.len(), 2, "the fresh documents must survive");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_documents_without_the_field_are_never_expired() {
        /*
        A missing timestamp means unknown age, not infinite age. The comparison
        `receivedAt < cutoff` is false for a missing field because a comparison
        against Null has no ordering, so this falls out of the engine's
        semantics rather than needing a special case. Asserted because deleting
        untimestamped data would be silent and unrecoverable.
        */
        let (db, path) = temp_db("nofield");
        {
            let mut guard = db.lock().unwrap();
            guard.create_collection("snapshots").unwrap();

            let mut orphan = Document::new();
            orphan.insert("placeId".to_string(), Value::String("abc".to_string()));
            guard.insert_doc("snapshots", &orphan).unwrap();
            guard
                .insert_doc("snapshots", &snapshot(now_millis().unwrap() - 999_999_999))
                .unwrap();
        }

        let sweeper = Sweeper::new(Duration::from_secs(60)).with_policy(Policy::new(
            "snapshots",
            "receivedAt",
            Duration::from_secs(60),
        ));
        assert_eq!(sweeper.sweep_once(&db), 1, "only the timestamped one");

        let remaining = db.lock().unwrap().scan_collection("snapshots").unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].1.contains_key("placeId"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_a_broken_policy_does_not_stop_the_others() {
        let (db, path) = temp_db("resilient");
        {
            let mut guard = db.lock().unwrap();
            guard.create_collection("good").unwrap();
            guard
                .insert_doc("good", &snapshot(now_millis().unwrap() - 999_999_999))
                .unwrap();
        }

        let sweeper = Sweeper::new(Duration::from_secs(60))
            // this collection does not exist, so its policy fails to bind
            .with_policy(Policy::new("missing", "receivedAt", Duration::from_secs(60)))
            .with_policy(Policy::new("good", "receivedAt", Duration::from_secs(60)));

        assert_eq!(sweeper.sweep_once(&db), 1, "the healthy policy still ran");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sweep_is_a_no_op_when_nothing_has_aged_out() {
        let (db, path) = temp_db("noop");
        {
            let mut guard = db.lock().unwrap();
            guard.create_collection("snapshots").unwrap();
            guard.insert_doc("snapshots", &snapshot(now_millis().unwrap())).unwrap();
        }
        let sweeper = Sweeper::new(Duration::from_secs(60)).with_policy(Policy::new(
            "snapshots",
            "receivedAt",
            Duration::from_secs(7 * 24 * 3600),
        ));
        assert_eq!(sweeper.sweep_once(&db), 0);
        let _ = std::fs::remove_file(&path);
    }
}
