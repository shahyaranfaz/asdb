/*
multi_collection.rs: a latent storage-aliasing bug, pinned down before the
executor can trip over it.

STATUS: FIXED, and this test now passes. Kept as the regression guard, with
the original diagnosis intact because the same root cause produced three
separate performance bugs as well (see the note at the bottom).

WHAT WAS WRONG

Catalog opened a brand new DiskManager and BufferPool on every call.
create_collection, open_collection and flush each did it, and the returned
Collection owned that pool. DiskManager caches num_pages from the file length
at open time. So with two Collection handles alive at once:

  Catalog::create          allocates page 0, file is 1 page
  create_collection("a")   opens disk, num_pages = 1, takes page 1 as root
  create_collection("b")   opens disk, num_pages = 2, takes page 2 as root
  a inserts, needs a page  a's num_pages is still 2, so it takes page 2

which is b's root. Both collections now believe they own page 2.

In-process this is INVISIBLE, and that is what makes it dangerous: each pool
serves reads from its own cached frame, so both collections read back exactly
what they wrote. The assertions before the restart below pass even on the
broken tree. The damage only appears once the caches are gone.

Measured before the fix: collection a came back with 206 documents, every one
of them carrying b's tag. a's page chain had been rewritten to walk into b's
pages.

THE FIX

One DiskManager and one BufferPool for the whole database, owned by Database,
with HeapFile and Collection taking `bp: &mut BufferPool` per call instead of
owning one. The alternatives were a shared Rc<RefCell<BufferPool>>, which
moves borrow checking to runtime and turns a nested access into a panic, or
holding a &'a mut borrow in the struct, which makes two live collections a
compile error and so rules out the hash join that motivated this.

THE SAME ROOT CAUSE WAS ALSO THREE PERFORMANCE BUGS.

Opening a pool per document, rather than per database, was the dominant cost
in insert, delete, and index fetch. Before this: throughput flat at ~3000
docs/s regardless of batch size, a 0.86s TTL sweep over 2400 documents, and an
index scan 940x SLOWER than the sequential scan it was supposed to beat. One
design decision, one correctness bug, three performance bugs.

Note Catalog::flush still writes page 0 through its own DiskManager, outside
the pool. That is safe because nothing else ever allocates page 0, but it is
the same pattern and worth keeping in view.
*/

use asdb::database::Database;
use asdb::document::{Catalog, Document, Value};
use asdb::storage::{BufferPool, DiskManager};

/*
Documents are padded so a couple of hundred of them span many pages. Small
documents would both fit inside their root page and the allocation paths would
never overlap, so the test would pass while proving nothing.
*/
fn padded(tag: i64) -> Document {
    let mut doc = Document::new();
    doc.insert("tag".to_string(), Value::Int(tag));
    doc.insert("pad".to_string(), Value::String("x".repeat(600)));
    doc
}

#[test]
fn two_live_collection_handles_survive_a_restart() {
    let path = std::env::temp_dir().join("asdb_multi_collection_test.db");
    let _ = std::fs::remove_file(&path);

    // write phase, with both handles alive and inserts interleaved so the two
    // collections grow the file alternately.
    {
        let mut db = Database::create(&path).unwrap();
        db.create_collection("a").unwrap();
        db.create_collection("b").unwrap();

        for _ in 0..200 {
            db.insert_doc("a", &padded(1)).unwrap();
            db.insert_doc("b", &padded(2)).unwrap();
        }
        db.flush().unwrap();

        // Both look correct HERE, before the caches are dropped. These two
        // assertions passed even on the broken tree, which is exactly why the
        // restart below is the load-bearing part of this test.
        assert_eq!(db.scan_collection("a").unwrap().len(), 200);
        assert_eq!(db.scan_collection("b").unwrap().len(), 200);
    }

    // read phase, with every cache gone.
    let mut db = Database::open(&path).unwrap();
    let scan_a = db.scan_collection("a").unwrap();
    let scan_b = db.scan_collection("b").unwrap();
    drop(db);
    let _ = std::fs::remove_file(&path);

    let wrong_a = scan_a.iter().filter(|(_, d)| d.get("tag") != Some(&Value::Int(1))).count();
    let wrong_b = scan_b.iter().filter(|(_, d)| d.get("tag") != Some(&Value::Int(2))).count();

    assert_eq!(scan_a.len(), 200, "collection a changed size across restart");
    assert_eq!(scan_b.len(), 200, "collection b changed size across restart");
    assert_eq!(
        wrong_a + wrong_b,
        0,
        "collections are reading each other's documents after restart"
    );
}

#[test]
fn two_handles_to_one_collection_preserve_the_heap_chain() {
    let path = std::env::temp_dir().join("asdb_same_collection_handles_test.db");
    let _ = std::fs::remove_file(&path);

    {
        let mut catalog = Catalog::create(&path).unwrap();
        let disk = DiskManager::open(&path).unwrap();
        let mut pool = BufferPool::new(disk, 1024);
        let mut first = catalog.create_collection(&mut pool, "events").unwrap();
        let mut second = catalog.open_collection(&mut pool, "events").unwrap();

        for _ in 0..200 {
            first.insert(&mut pool, &padded(1)).unwrap();
            second.insert(&mut pool, &padded(2)).unwrap();
        }
        pool.flush_all().unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    let rows = db.scan_collection("events").unwrap();
    drop(db);
    let _ = std::fs::remove_file(&path);

    assert_eq!(rows.len(), 400, "a stale heap tail orphaned inserted rows");
    assert_eq!(
        rows.iter().filter(|(_, d)| d.get("tag") == Some(&Value::Int(1))).count(),
        200,
    );
    assert_eq!(
        rows.iter().filter(|(_, d)| d.get("tag") == Some(&Value::Int(2))).count(),
        200,
    );
}

/*
The same scenario driven document by document rather than in one batch, so the
allocation windows interleave as finely as possible.
*/
#[test]
fn interleaved_single_writes_also_survive() {
    let path = std::env::temp_dir().join("asdb_facade_test.db");
    let _ = std::fs::remove_file(&path);

    {
        let mut db = Database::create(&path).unwrap();
        db.create_collection("a").unwrap();
        db.create_collection("b").unwrap();
        for _ in 0..200 {
            db.insert_doc("a", &padded(1)).unwrap();
            db.insert_doc("b", &padded(2)).unwrap();
        }
    }

    let mut db = Database::open(&path).unwrap();
    let scan_a = db.scan_collection("a").unwrap();
    let scan_b = db.scan_collection("b").unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(scan_a.len(), 200);
    assert_eq!(scan_b.len(), 200);
    assert!(scan_a.iter().all(|(_, d)| d.get("tag") == Some(&Value::Int(1))));
    assert!(scan_b.iter().all(|(_, d)| d.get("tag") == Some(&Value::Int(2))));
}
