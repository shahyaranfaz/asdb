/*
database.rs: high-level runtime coordinator for one database file.

Database owns the catalog and coordinates collections, indexes, and flush
boundaries. Lower layers stay independently testable; callers above this layer
should not manually pair collection inserts/deletes with IndexManager updates.
*/
use super::DatabaseResult;

use crate::btree::IndexManager;
use crate::document::{Catalog, Document, Value};
use crate::storage::{BufferPool, DiskManager, DocId, PageId};

use std::path::Path;

/// Frames the shared pool keeps resident. Was per-collection, so it used to be
/// multiplied by however many collections happened to be open.
const DEFAULT_POOL_CAPACITY: usize = 1024;

/*
ONE DiskManager, ONE BufferPool, ONE Catalog, for the whole database.

This used to hold only a path and a capacity, and build a fresh BufferPool per
operation via make_pool, while Catalog separately built one per Collection.
That was wrong twice over:

  CORRECTNESS. Two live Collection handles meant two DiskManagers over one
  file, each caching its own num_pages, each allocating page ids from its own
  stale count. They handed out the same page twice, and the per-pool caches
  hid it until a restart. tests/multi_collection.rs is the regression test.

  PERFORMANCE. Opening a pool per document was the dominant cost in insert,
  delete, and index fetch. Measured before batching: ~3000 docs/s regardless
  of batch size, a 0.86s TTL sweep over 2400 documents, and an index scan
  940x SLOWER than the sequential scan it was meant to beat.

Both symptoms, one cause. Sharing the pool fixes the class rather than the
instances.
*/
pub struct Database {
    catalog: Catalog,
    pool: BufferPool,
}

impl Database {
    pub fn create(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = Catalog::create(&path)?;
        // Catalog::create allocates page 0 through its own short-lived handle
        // and must finish BEFORE the pool's DiskManager reads the file length,
        // or the pool believes the file is empty and hands out page 0 twice.
        let pool = BufferPool::new(DiskManager::open(&path)?, DEFAULT_POOL_CAPACITY);
        Ok(Database { catalog, pool })
    }

    pub fn open(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = Catalog::open(&path)?;
        let pool = BufferPool::new(DiskManager::open(&path)?, DEFAULT_POOL_CAPACITY);
        Ok(Database { catalog, pool })
    }

    /*
    catalog_and_pool: both borrows at once.

    db.catalog() and db.pool() in one expression does not compile, because each
    would take &mut self. Returning the pair from one method does, because
    inside it the compiler can see the two fields are disjoint. Standard escape
    hatch, and the reason the helpers below live on Database.
    */
    fn catalog_and_pool(&mut self) -> (&mut Catalog, &mut BufferPool) {
        (&mut self.catalog, &mut self.pool)
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.catalog.collection_root(name).is_some()
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.catalog.has_index(collection, field)
    }

    pub fn create_collection(&mut self, name: &str) -> DatabaseResult<()> {
        let (catalog, pool) = self.catalog_and_pool();
        let collection = catalog.create_collection(pool, name)?;
        collection.flush(pool)?;
        self.catalog.flush()?;
        Ok(())
    }

    pub fn drop_collection(&mut self, name: &str) -> DatabaseResult<()> {
        let fields = self.catalog.index_fields(name);
        for field in fields {
            self.drop_index(name, &field)?;
        }
        self.catalog.drop_collection(name)?;
        self.catalog.flush()?;
        Ok(())
    }

    pub fn scan_collection(&mut self, collection: &str) -> DatabaseResult<Vec<(DocId, Document)>> {
        let (catalog, pool) = self.catalog_and_pool();
        let handle = catalog.open_collection(pool, collection)?;
        Ok(handle.scan(pool)?)
    }

    /*
    scan_page: one page of a collection, plus the id of the next.

    The streaming alternative to scan_collection. The executor's SeqScan uses
    it so `limit 25` over a large collection stops after the first page instead
    of materialising every document first.

    Pass None as the cursor to start at the beginning. Returns None as the next
    cursor when the chain is exhausted.
    */
    pub fn scan_page(
        &mut self,
        collection: &str,
        cursor: Option<PageId>,
    ) -> DatabaseResult<(Vec<(DocId, Document)>, Option<PageId>)> {
        let (catalog, pool) = self.catalog_and_pool();
        let handle = catalog.open_collection(pool, collection)?;
        let page = cursor.unwrap_or_else(|| handle.first_page());
        Ok(handle.scan_page(pool, page)?)
    }

    pub fn create_index(&mut self, collection: &str, field: &str) -> DatabaseResult<()> {
        let (catalog, pool) = self.catalog_and_pool();
        {
            let mut indexes = IndexManager::new(catalog, pool);
            indexes.create(collection, field)?;
        }
        self.pool.flush_all()?;
        Ok(())
    }

    pub fn drop_index(&mut self, collection: &str, field: &str) -> DatabaseResult<()> {
        let (catalog, pool) = self.catalog_and_pool();
        {
            let mut indexes = IndexManager::new(catalog, pool);
            indexes.drop(collection, field)?;
        }
        self.pool.flush_all()?;
        Ok(())
    }

    /*
    insert_doc: one document. Delegates to the batch path.

    Before the pool was shared these were genuinely different code, because
    the batch version existed to avoid re-opening the collection per document.
    With one pool there is nothing left to avoid, so a single insert is just a
    batch of one and the duplication goes away.
    */
    pub fn insert_doc(&mut self, collection: &str, doc: &Document) -> DatabaseResult<DocId> {
        Ok(self.insert_docs(collection, std::slice::from_ref(doc))?[0])
    }

    /*
    insert_docs: insert a batch.

    One collection handle and one index pass for the whole batch, then a single
    flush. The flush is what makes batching worth anything now that the pool is
    shared: fsync-per-document is the remaining fixed cost, and this pays it
    once per statement instead of once per row.
    */
    pub fn insert_docs(
        &mut self,
        collection: &str,
        docs: &[Document],
    ) -> DatabaseResult<Vec<DocId>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let (catalog, pool) = self.catalog_and_pool();
        let mut handle = catalog.open_collection(pool, collection)?;
        let mut doc_ids = Vec::with_capacity(docs.len());
        for doc in docs {
            doc_ids.push(handle.insert(pool, doc)?);
        }

        {
            let mut indexes = IndexManager::new(catalog, pool);
            for (doc, doc_id) in docs.iter().zip(doc_ids.iter()) {
                indexes.on_insert(collection, doc, *doc_id)?;
            }
        }
        self.pool.flush_all()?;
        Ok(doc_ids)
    }

    pub fn delete_doc(&mut self, collection: &str, doc_id: DocId) -> DatabaseResult<bool> {
        Ok(self.delete_docs(collection, &[doc_id])? == 1)
    }

    /*
    delete_docs: delete a batch.

    Documents are READ BEFORE being deleted, because on_delete needs the old
    field values to find the right index entries. An id that is already gone is
    skipped rather than failing the batch, which matters because the TTL
    sweeper can overlap its own previous pass.
    */
    pub fn delete_docs(&mut self, collection: &str, doc_ids: &[DocId]) -> DatabaseResult<usize> {
        if doc_ids.is_empty() {
            return Ok(0);
        }

        let (catalog, pool) = self.catalog_and_pool();
        let handle = catalog.open_collection(pool, collection)?;

        let mut removed: Vec<(DocId, Document)> = Vec::with_capacity(doc_ids.len());
        for doc_id in doc_ids {
            let Some(doc) = handle.get(pool, *doc_id)? else {
                continue; // already gone
            };
            handle.delete(pool, *doc_id)?;
            removed.push((*doc_id, doc));
        }

        {
            let mut indexes = IndexManager::new(catalog, pool);
            for (doc_id, doc) in &removed {
                indexes.on_delete(collection, doc, *doc_id)?;
            }
        }
        self.pool.flush_all()?;
        Ok(removed.len())
    }

    pub fn get_doc(&mut self, collection: &str, doc_id: DocId) -> DatabaseResult<Option<Document>> {
        let (catalog, pool) = self.catalog_and_pool();
        let handle = catalog.open_collection(pool, collection)?;
        Ok(handle.get(pool, doc_id)?)
    }

    /*
    fetch_docs: read many documents by id, one handle for the batch.

    This used to loop over get_doc, which opened a fresh Collection, DiskManager
    and BufferPool per call. On an index scan returning 22,000 rows that was
    22,000 file opens, and it made the indexed path 940x SLOWER than a
    sequential scan of the same data: 18.8s against 0.020s, measured.
    */
    fn fetch_docs(
        &mut self,
        collection: &str,
        doc_ids: &[DocId],
    ) -> DatabaseResult<Vec<(DocId, Document)>> {
        if doc_ids.is_empty() {
            return Ok(Vec::new());
        }
        let (catalog, pool) = self.catalog_and_pool();
        let handle = catalog.open_collection(pool, collection)?;
        let mut out = Vec::with_capacity(doc_ids.len());
        for doc_id in doc_ids {
            if let Some(doc) = handle.get(pool, *doc_id)? {
                out.push((*doc_id, doc));
            }
        }
        Ok(out)
    }

    pub fn search_index(
        &mut self,
        collection: &str,
        field: &str,
        value: &Value,
    ) -> DatabaseResult<Option<(DocId, Document)>> {
        let doc_id = {
            let (catalog, pool) = self.catalog_and_pool();
            let mut indexes = IndexManager::new(catalog, pool);
            indexes.search(collection, field, value)?
        };
        let Some(doc_id) = doc_id else {
            return Ok(None);
        };
        Ok(self.get_doc(collection, doc_id)?.map(|doc| (doc_id, doc)))
    }

    pub fn range_index(
        &mut self,
        collection: &str,
        field: &str,
        low: &Value,
        high: &Value,
    ) -> DatabaseResult<Vec<(DocId, Document)>> {
        let doc_ids = {
            let (catalog, pool) = self.catalog_and_pool();
            let mut indexes = IndexManager::new(catalog, pool);
            indexes.range_scan(collection, field, low, high)?
        };
        self.fetch_docs(collection, &doc_ids)
    }

    /// Flush everything to disk. The catalog writes page 0 through its own
    /// handle, deliberately outside the pool, since nothing else allocates it.
    pub fn flush(&mut self) -> DatabaseResult<()> {
        self.pool.flush_all()?;
        self.catalog.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::document::Value;

    use tempfile::NamedTempFile;

    fn user_doc(id: i64, age: i64, name: &str) -> Document {
        let mut doc = Document::new();
        doc.insert("age".to_string(), Value::Int(age));
        doc.insert("id".to_string(), Value::Int(id));
        doc.insert("name".to_string(), Value::String(name.to_string()));
        doc
    }

    #[test]
    fn test_database_create_collection_and_reopen() {
        let tmp = NamedTempFile::new().unwrap();

        {
            let mut db = Database::create(tmp.path()).unwrap();
            db.create_collection("users").unwrap();
            assert!(db.has_collection("users"));
        }

        let db = Database::open(tmp.path()).unwrap();
        assert!(db.has_collection("users"));
    }

    #[test]
    fn test_database_insert_and_scan() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();

        let alice = user_doc(1, 25, "alice");
        let bob = user_doc(2, 17, "bob");
        db.insert_doc("users", &alice).unwrap();
        db.insert_doc("users", &bob).unwrap();

        let docs: Vec<Document> = db.scan_collection("users")
            .unwrap()
            .into_iter()
            .map(|(_, doc)| doc)
            .collect();

        assert_eq!(docs.len(), 2);
        assert!(docs.contains(&alice));
        assert!(docs.contains(&bob));
    }

    #[test]
    fn test_database_create_index_backfills_existing_documents() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();

        let alice = user_doc(1, 25, "alice");
        let bob = user_doc(2, 17, "bob");
        let alice_id = db.insert_doc("users", &alice).unwrap();
        let bob_id = db.insert_doc("users", &bob).unwrap();

        db.create_index("users", "age").unwrap();

        assert!(db.has_index("users", "age"));
        assert_eq!(
            db.search_index("users", "age", &Value::Int(25)).unwrap(),
            Some((alice_id, alice))
        );
        assert_eq!(
            db.search_index("users", "age", &Value::Int(17)).unwrap(),
            Some((bob_id, bob))
        );
    }

    #[test]
    fn test_database_insert_updates_index() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();
        db.create_index("users", "age").unwrap();

        let alice = user_doc(1, 25, "alice");
        let alice_id = db.insert_doc("users", &alice).unwrap();

        assert_eq!(
            db.search_index("users", "age", &Value::Int(25)).unwrap(),
            Some((alice_id, alice))
        );
    }

    #[test]
    fn test_database_delete_updates_index() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();
        db.create_index("users", "age").unwrap();

        let alice = user_doc(1, 25, "alice");
        let alice_id = db.insert_doc("users", &alice).unwrap();
        assert!(db.delete_doc("users", alice_id).unwrap());

        assert_eq!(
            db.search_index("users", "age", &Value::Int(25)).unwrap(),
            None
        );
        assert!(!db.delete_doc("users", alice_id).unwrap());
    }

    #[test]
    fn test_database_range_index_fetches_documents() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();
        db.create_index("users", "age").unwrap();

        let alice = user_doc(1, 25, "alice");
        let bob = user_doc(2, 17, "bob");
        let cara = user_doc(3, 31, "cara");
        let dina = user_doc(4, 25, "dina");
        let alice_id = db.insert_doc("users", &alice).unwrap();
        db.insert_doc("users", &bob).unwrap();
        db.insert_doc("users", &cara).unwrap();
        let dina_id = db.insert_doc("users", &dina).unwrap();

        let got = db.range_index("users", "age", &Value::Int(20), &Value::Int(30)).unwrap();

        assert_eq!(got, vec![(alice_id, alice), (dina_id, dina)]);
    }

    #[test]
    fn test_database_reopen_preserves_data_and_index_metadata() {
        let tmp = NamedTempFile::new().unwrap();
        let alice = user_doc(1, 25, "alice");
        let alice_id;

        {
            let mut db = Database::create(tmp.path()).unwrap();
            db.create_collection("users").unwrap();
            alice_id = db.insert_doc("users", &alice).unwrap();
            db.create_index("users", "age").unwrap();
        }

        let mut db = Database::open(tmp.path()).unwrap();
        assert!(db.has_collection("users"));
        assert!(db.has_index("users", "age"));
        assert_eq!(db.get_doc("users", alice_id).unwrap(), Some(alice.clone()));
        assert_eq!(
            db.search_index("users", "age", &Value::Int(25)).unwrap(),
            Some((alice_id, alice))
        );
    }

    #[test]
    fn test_database_drop_collection_removes_indexes() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();
        db.create_index("users", "age").unwrap();

        db.drop_collection("users").unwrap();

        assert!(!db.has_collection("users"));
        assert!(!db.has_index("users", "age"));
    }

    /*
    The batch paths changed how writes are issued (one open per statement
    rather than one per document), so they need their own coverage: the
    per-document methods passing proves nothing about them.
    */
    #[test]
    fn test_insert_docs_matches_repeated_insert_doc() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("batched").unwrap();
        db.create_collection("singly").unwrap();

        let docs: Vec<Document> = (0..50)
            .map(|i| {
                let mut d = Document::new();
                d.insert("n".to_string(), Value::Int(i));
                d.insert("tag".to_string(), Value::String(format!("t{}", i % 5)));
                d
            })
            .collect();

        let ids = db.insert_docs("batched", &docs).unwrap();
        assert_eq!(ids.len(), 50, "one id per document, in order");
        for doc in &docs {
            db.insert_doc("singly", doc).unwrap();
        }

        let mut batched: Vec<Document> =
            db.scan_collection("batched").unwrap().into_iter().map(|(_, d)| d).collect();
        let mut singly: Vec<Document> =
            db.scan_collection("singly").unwrap().into_iter().map(|(_, d)| d).collect();
        let key = |d: &Document| match d.get("n") {
            Some(Value::Int(n)) => *n,
            _ => -1,
        };
        batched.sort_by_key(key);
        singly.sort_by_key(key);
        assert_eq!(batched, singly, "batching must not change what is stored");

        // the returned ids must actually address the documents they claim to
        for (id, doc) in ids.iter().zip(docs.iter()) {
            assert_eq!(db.get_doc("batched", *id).unwrap().as_ref(), Some(doc));
        }
    }

    #[test]
    fn test_insert_docs_maintains_indexes() {
        // the index update moved out of the per-document loop, so prove the
        // entries still land.
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("c").unwrap();
        db.create_index("c", "n").unwrap();

        let docs: Vec<Document> = (0..20)
            .map(|i| {
                let mut d = Document::new();
                d.insert("n".to_string(), Value::Int(i));
                d
            })
            .collect();
        let ids = db.insert_docs("c", &docs).unwrap();

        assert_eq!(db.search_index("c", "n", &Value::Int(7)).unwrap().map(|(id, _)| id), Some(ids[7]));
        assert_eq!(db.range_index("c", "n", &Value::Int(5), &Value::Int(8)).unwrap().len(), 4);
    }

    #[test]
    fn test_delete_docs_removes_exactly_the_given_set() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("c").unwrap();

        let docs: Vec<Document> = (0..30)
            .map(|i| {
                let mut d = Document::new();
                d.insert("n".to_string(), Value::Int(i));
                d
            })
            .collect();
        let ids = db.insert_docs("c", &docs).unwrap();

        let doomed: Vec<DocId> = ids.iter().step_by(3).copied().collect();
        assert_eq!(db.delete_docs("c", &doomed).unwrap(), doomed.len());

        let left = db.scan_collection("c").unwrap();
        assert_eq!(left.len(), 30 - doomed.len());
        assert!(
            left.iter().all(|(_, d)| matches!(d.get("n"), Some(Value::Int(n)) if n % 3 != 0)),
            "only every third document should be gone"
        );
    }

    #[test]
    fn test_delete_docs_skips_ids_that_are_already_gone() {
        // the TTL sweeper can race its own previous pass; a stale id must not
        // fail the whole batch.
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("c").unwrap();
        let mut doc = Document::new();
        doc.insert("n".to_string(), Value::Int(1));
        let ids = db.insert_docs("c", &[doc]).unwrap();

        assert_eq!(db.delete_docs("c", &ids).unwrap(), 1);
        assert_eq!(db.delete_docs("c", &ids).unwrap(), 0, "second pass is a no-op, not an error");
    }

    #[test]
    fn test_empty_batches_are_no_ops() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("c").unwrap();
        assert!(db.insert_docs("c", &[]).unwrap().is_empty());
        assert_eq!(db.delete_docs("c", &[]).unwrap(), 0);
    }

    #[test]
    fn test_batch_survives_a_restart() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut db = Database::create(tmp.path()).unwrap();
            db.create_collection("c").unwrap();
            let docs: Vec<Document> = (0..100)
                .map(|i| {
                    let mut d = Document::new();
                    d.insert("n".to_string(), Value::Int(i));
                    d
                })
                .collect();
            let ids = db.insert_docs("c", &docs).unwrap();
            db.delete_docs("c", &ids[..40]).unwrap();
        }
        let mut db = Database::open(tmp.path()).unwrap();
        assert_eq!(db.scan_collection("c").unwrap().len(), 60);
    }
}
