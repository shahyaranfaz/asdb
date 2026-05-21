/*
database.rs: high-level runtime coordinator for one database file.

Database owns the catalog and coordinates collections, indexes, and flush
boundaries. Lower layers stay independently testable; callers above this layer
should not manually pair collection inserts/deletes with IndexManager updates.
*/
use super::DatabaseResult;

use crate::btree::IndexManager;
use crate::document::{Catalog, Document, Value};
use crate::storage::{BufferPool, DiskManager, DocId};

use std::path::{Path, PathBuf};

pub struct Database {
    path: PathBuf,
    catalog: Catalog,
    pool_capacity: usize,
}

impl Database {
    pub fn create(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = Catalog::create(&path)?;
        Ok(Database {
            path,
            catalog,
            pool_capacity: 1024,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = Catalog::open(&path)?;
        Ok(Database {
            path,
            catalog,
            pool_capacity: 1024,
        })
    }

    fn make_pool(&self) -> DatabaseResult<BufferPool> {
        let disk = DiskManager::open(&self.path)?;
        Ok(BufferPool::new(disk, self.pool_capacity))
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.catalog.collection_root(name).is_some()
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.catalog.has_index(collection, field)
    }

    pub fn create_collection(&mut self, name: &str) -> DatabaseResult<()> {
        let mut collection = self.catalog.create_collection(name)?;
        collection.flush()?;
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
        let mut collection = self.catalog.open_collection(collection)?;
        Ok(collection.scan()?)
    }

    pub fn create_index(&mut self, collection: &str, field: &str) -> DatabaseResult<()> {
        let mut pool = self.make_pool()?;
        {
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.create(collection, field)?;
        }
        pool.flush_all()?;
        Ok(())
    }

    pub fn drop_index(&mut self, collection: &str, field: &str) -> DatabaseResult<()> {
        let mut pool = self.make_pool()?;
        {
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.drop(collection, field)?;
        }
        pool.flush_all()?;
        Ok(())
    }

    pub fn insert_doc(&mut self, collection: &str, doc: &Document) -> DatabaseResult<DocId> {
        let mut collection_handle = self.catalog.open_collection(collection)?;
        let doc_id = collection_handle.insert(doc)?;
        collection_handle.flush()?;

        let mut pool = self.make_pool()?;
        {
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.on_insert(collection, doc, doc_id)?;
        }
        pool.flush_all()?;

        Ok(doc_id)
    }

    pub fn delete_doc(&mut self, collection: &str, doc_id: DocId) -> DatabaseResult<bool> {
        let mut collection_handle = self.catalog.open_collection(collection)?;

        let Some(doc) = collection_handle.get(doc_id)? else {
            return Ok(false);
        };

        collection_handle.delete(doc_id)?;
        collection_handle.flush()?;

        let mut pool = self.make_pool()?;
        {
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.on_delete(collection, &doc, doc_id)?;
        }
        pool.flush_all()?;

        Ok(true)
    }

    pub fn get_doc(&mut self, collection: &str, doc_id: DocId) -> DatabaseResult<Option<Document>> {
        let mut collection = self.catalog.open_collection(collection)?;
        Ok(collection.get(doc_id)?)
    }

    pub fn search_index(
        &mut self,
        collection: &str,
        field: &str,
        value: &Value,
    ) -> DatabaseResult<Option<(DocId, Document)>> {
        let doc_id = {
            let mut pool = self.make_pool()?;
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.search(collection, field, value)?
        };

        let Some(doc_id) = doc_id else {
            return Ok(None);
        };

        let Some(doc) = self.get_doc(collection, doc_id)? else {
            return Ok(None);
        };

        Ok(Some((doc_id, doc)))
    }

    pub fn range_index(
        &mut self,
        collection: &str,
        field: &str,
        low: &Value,
        high: &Value,
    ) -> DatabaseResult<Vec<(DocId, Document)>> {
        let doc_ids = {
            let mut pool = self.make_pool()?;
            let mut indexes = IndexManager::new(&mut self.catalog, &mut pool);
            indexes.range_scan(collection, field, low, high)?
        };

        let mut out = Vec::new();
        for doc_id in doc_ids {
            if let Some(doc) = self.get_doc(collection, doc_id)? {
                out.push((doc_id, doc));
            }
        }
        Ok(out)
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
}
