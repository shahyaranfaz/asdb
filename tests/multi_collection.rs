/*
multi_collection.rs: a latent storage-aliasing bug, pinned down before the
executor can trip over it.

STATUS: this test FAILS today. It is #[ignore]d so it does not break the
suite, not because it is flaky. It is here as a specification of the problem,
with the fix deliberately left out because the fix is a design change to
Database and Catalog that should be reviewed on its own.

WHAT IS WRONG

Catalog opens a brand new DiskManager and BufferPool on every call.
create_collection, open_collection and flush each do it, and the returned
Collection owns that pool. DiskManager caches num_pages from the file length
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

Measured on this commit: collection a comes back with 206 documents, every
one of them carrying b's tag. a's page chain had been rewritten to walk into
b's pages.

WHY Database DOES NOT HIT THIS TODAY

It is worth being precise, because the facade looks fine. Database::insert_doc
and friends open a collection, do one operation, flush, and drop it before the
next call. Nothing ever holds two handles at once, so the allocation windows
never overlap and the same test written against Database passes.

The exposure is the LAYER BENEATH, which is public API. Any caller that holds
two Collection handles simultaneously hits this, and the query executor is
going to be exactly that caller: a hash join reads two collections at once by
definition. So this wants fixing before 5.6, not after.

THE SHAPE OF A FIX

One DiskManager and one BufferPool for the whole database, owned by Database,
with HeapFile and Collection taking `bp: &mut BufferPool` per call instead of
owning one. The alternatives are a shared Rc<RefCell<BufferPool>>, which moves
borrow checking to runtime and turns a nested access into a panic, or holding
a &'a mut borrow in the struct, which makes two live collections a compile
error and so rules out the join that motivated this.

Note also that Catalog::flush writes page 0 through its own DiskManager,
outside any pool. That one is safe as long as nothing else ever allocates
page 0, but it is the same pattern and worth keeping in view.
*/

use asdb::document::{Catalog, Document, Value};

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
#[ignore = "known latent bug: Catalog hands every Collection its own BufferPool"]
fn two_live_collection_handles_survive_a_restart() {
    let path = std::env::temp_dir().join("asdb_multi_collection_test.db");
    let _ = std::fs::remove_file(&path);

    // write phase, with both handles alive and inserts interleaved so the two
    // collections grow the file alternately.
    {
        let mut catalog = Catalog::create(&path).unwrap();
        let mut a = catalog.create_collection("a").unwrap();
        let mut b = catalog.create_collection("b").unwrap();

        for _ in 0..200 {
            a.insert(&padded(1)).unwrap();
            b.insert(&padded(2)).unwrap();
        }
        a.flush().unwrap();
        b.flush().unwrap();
        catalog.flush().unwrap();

        // Both look correct HERE, before the caches are dropped. These two
        // assertions pass even on the broken tree, which is exactly why the
        // restart below is the load-bearing part of this test.
        assert_eq!(a.scan().unwrap().len(), 200);
        assert_eq!(b.scan().unwrap().len(), 200);
    }

    // read phase, with every cache gone.
    let catalog = Catalog::open(&path).unwrap();
    let mut a = catalog.open_collection("a").unwrap();
    let mut b = catalog.open_collection("b").unwrap();
    let scan_a = a.scan().unwrap();
    let scan_b = b.scan().unwrap();
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

/*
The same scenario through the Database facade, which PASSES. Kept alongside
the failing test so the boundary is documented rather than rediscovered: the
facade is safe because it never holds two handles, and that is the only reason.
*/
#[test]
fn database_facade_is_not_affected() {
    use asdb::database::Database;

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
