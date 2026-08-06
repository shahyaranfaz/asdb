use super::BTree;

use crate::document::{serialize_key, Catalog, CatalogError, CatalogResult, Document, Value};
use crate::storage::{BufferPool, DocId, PageId};

pub struct IndexManager<'a> {
    catalog: &'a mut Catalog,
    pool: &'a mut BufferPool,
}

impl<'a> IndexManager<'a> {
    pub fn new(catalog: &'a mut Catalog, pool: &'a mut BufferPool) -> Self {
        IndexManager { catalog, pool }
    }

    pub fn create(&mut self, collection: &str, field: &str) -> CatalogResult<()> {
        self.catalog
            .collection_root(collection)
            .ok_or_else(|| CatalogError::CollectionNotFound(collection.to_string()))?;

        let key = (collection.to_string(), field.to_string());
        if self.catalog.indexes.contains_key(&key) {
            return Err(CatalogError::IndexAlreadyExists(collection.to_string()));
        }

        let root = {
            let btree = BTree::new(self.pool)?;
            btree.root()
        };

        /*
        Scan the collection BEFORE opening the btree. Both need the pool
        mutably and BTree::open holds its borrow for as long as the btree
        lives, so materialising the documents first ends the collection's
        borrow and leaves the pool free.
        */
        let existing = self.catalog.open_collection(self.pool, collection)?;
        let docs = existing.scan(self.pool)?;
        existing.flush(self.pool)?;

        let mut btree = BTree::open(root, self.pool);
        for (doc_id, doc) in docs {
            if let Some(value) = doc.get(field) {
                let key = serialize_key(value);
                btree.insert(&key, doc_id)?;
            }
        }

        self.catalog.indexes.insert(key, root);
        self.catalog.flush()?;
        Ok(())
    }

    pub fn drop(&mut self, collection: &str, field: &str) -> CatalogResult<()> {
        let key = (collection.to_string(), field.to_string());
        let root = self.catalog.indexes.get(&key)
            .copied()
            .ok_or_else(|| CatalogError::IndexNotFound(collection.to_string()))?;

        let mut btree = BTree::open(root, self.pool);
        btree.free()?;
        self.catalog.indexes.remove(&key);
        self.catalog.flush()?;
        Ok(())
    }

    fn indexes_for(&self, collection: &str) -> Vec<((String, String), PageId)> {
        self.catalog.indexes
            .iter()
            .filter(|((col, _), _)| col == collection)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    pub fn on_insert(&mut self, collection: &str, doc: &Document,
                     doc_id: DocId) -> CatalogResult<()> {

        let index_keys: Vec<((String, String), PageId)> = self.indexes_for(collection);
        for ((_, field), root) in index_keys {
            if let Some(value) = doc.get(&field) {
                let key = serialize_key(value);
                let mut btree = BTree::open(root, self.pool);
                btree.insert(&key, doc_id)?;
            }
        }
        Ok(())
    }

    pub fn on_delete(&mut self, collection: &str, doc: &Document,
                     doc_id: DocId) -> CatalogResult<()> {

        let index_keys: Vec<((String, String), PageId)> = self.indexes_for(collection);
        for ((_, field), root) in index_keys {
            if let Some(value) = doc.get(&field) {
                let key = serialize_key(value);
                let mut btree = BTree::open(root, self.pool);
                btree.delete(&key, doc_id)?;
            }
        }
        Ok(())
    }

    pub fn search(&mut self, collection: &str, field: &str,
                  key: &Value) -> CatalogResult<Option<DocId>> {
        let index_key = (collection.to_string(), field.to_string());
        let root = self.catalog.indexes.get(&index_key)
            .copied()
            .ok_or_else(|| CatalogError::IndexNotFound(collection.to_string()))?;

        let key_bytes = serialize_key(key);
        let mut btree = BTree::open(root, self.pool);
        Ok(btree.search(&key_bytes)?)
    }

    pub fn range_scan(&mut self, collection: &str, field: &str, low: &Value,
                      high: &Value) -> CatalogResult<Vec<DocId>> {
        let index_key = (collection.to_string(), field.to_string());
        let root = self.catalog.indexes.get(&index_key)
            .copied()
            .ok_or_else(|| CatalogError::IndexNotFound(collection.to_string()))?;

        let low_bytes = serialize_key(low);
        let high_bytes = serialize_key(high);
        let mut btree = BTree::open(root, self.pool);
        Ok(btree.range_scan(&low_bytes, &high_bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;
    use tempfile::NamedTempFile;

    fn make_doc(id: i64, age: i64, name: &str) -> Document {
        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(id));
        doc.insert("age".to_string(), Value::Int(age));
        doc.insert("name".to_string(), Value::String(name.to_string()));
        doc
    }

    /*
    ONE catalog and ONE pool per test, in that order.

    These used to call make_pool() repeatedly, building a fresh pool per
    IndexManager while the Collection held yet another. That is the exact
    aliasing the shared pool removed, so they now share one the way real
    callers do.

    The consequence shows up in the shape below: IndexManager borrows both the
    catalog and the pool for its whole lifetime, so a Collection cannot be
    touched while one is alive. Index work sits in its own scope, collection
    work outside it. That is the borrow checker making "who is mutating storage
    right now" explicit, which is the point of threading &mut.
    */
    fn open_catalog_and_pool(path: &std::path::Path) -> (Catalog, BufferPool) {
        let catalog = Catalog::create(path).unwrap();
        let disk = DiskManager::open(path).unwrap();
        (catalog, BufferPool::new(disk, 64))
    }

    #[test]
    fn test_create_index_persists_catalog_entry() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        catalog.create_collection(&mut pool, "users").unwrap();

        {
            let mut indexes = IndexManager::new(&mut catalog, &mut pool);
            indexes.create("users", "age").unwrap();
        }

        let reopened = Catalog::open(tmp.path()).unwrap();
        assert!(reopened.indexes.contains_key(&("users".to_string(), "age".to_string())));
    }

    #[test]
    fn test_create_index_rejects_missing_collection() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        let mut indexes = IndexManager::new(&mut catalog, &mut pool);

        assert!(matches!(
            indexes.create("users", "age"),
            Err(CatalogError::CollectionNotFound(_))
        ));
    }

    #[test]
    fn test_create_index_backfills_existing_documents() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        let mut users = catalog.create_collection(&mut pool, "users").unwrap();

        let alice = make_doc(1, 25, "alice");
        let bob = make_doc(2, 17, "bob");
        let alice_id = users.insert(&mut pool, &alice).unwrap();
        let bob_id = users.insert(&mut pool, &bob).unwrap();
        users.flush(&mut pool).unwrap();
        let mut indexes = IndexManager::new(&mut catalog, &mut pool);
        indexes.create("users", "age").unwrap();

        assert_eq!(indexes.search("users", "age", &Value::Int(25)).unwrap(), Some(alice_id));
        assert_eq!(indexes.search("users", "age", &Value::Int(17)).unwrap(), Some(bob_id));
    }

    #[test]
    fn test_index_manager_insert_hook_search_and_range() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        let mut users = catalog.create_collection(&mut pool, "users").unwrap();
        users.flush(&mut pool).unwrap();
        {
            let mut indexes = IndexManager::new(&mut catalog, &mut pool);
            indexes.create("users", "age").unwrap();
        }

        let docs = vec![
            make_doc(1, 25, "alice"),
            make_doc(2, 17, "bob"),
            make_doc(3, 31, "cara"),
            make_doc(4, 25, "dina"),
        ];

        let mut ids = Vec::new();
        for doc in &docs {
            ids.push(users.insert(&mut pool, doc).unwrap());
        }
        users.flush(&mut pool).unwrap();
        let mut indexes = IndexManager::new(&mut catalog, &mut pool);
        for (doc, doc_id) in docs.iter().zip(ids.iter()) {
            indexes.on_insert("users", doc, *doc_id).unwrap();
        }

        assert_eq!(indexes.search("users", "age", &Value::Int(17)).unwrap(), Some(ids[1]));

        let got = indexes
            .range_scan("users", "age", &Value::Int(20), &Value::Int(30))
            .unwrap();
        assert_eq!(got, vec![ids[0], ids[3]]);
    }

    #[test]
    fn test_index_manager_delete_hook_removes_entry() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        let mut users = catalog.create_collection(&mut pool, "users").unwrap();
        users.flush(&mut pool).unwrap();
        {
            let mut indexes = IndexManager::new(&mut catalog, &mut pool);
            indexes.create("users", "age").unwrap();
        }

        let doc = make_doc(1, 25, "alice");
        let doc_id = users.insert(&mut pool, &doc).unwrap();
        users.flush(&mut pool).unwrap();

        {
            let mut indexes = IndexManager::new(&mut catalog, &mut pool);
            indexes.on_insert("users", &doc, doc_id).unwrap();
            assert_eq!(indexes.search("users", "age", &Value::Int(25)).unwrap(), Some(doc_id));
        }

        // the caller reads the document BEFORE deleting it, because on_delete
        // needs the old field value to find the right index entry.
        let before_delete = users.get(&mut pool, doc_id).unwrap().unwrap();
        users.delete(&mut pool, doc_id).unwrap();

        let mut indexes = IndexManager::new(&mut catalog, &mut pool);
        indexes.on_delete("users", &before_delete, doc_id).unwrap();
        assert_eq!(indexes.search("users", "age", &Value::Int(25)).unwrap(), None);
    }

    #[test]
    fn test_drop_index_removes_catalog_entry_and_frees_tree() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut pool) = open_catalog_and_pool(tmp.path());
        catalog.create_collection(&mut pool, "users").unwrap();
        let mut indexes = IndexManager::new(&mut catalog, &mut pool);
        indexes.create("users", "age").unwrap();
        indexes.drop("users", "age").unwrap();

        assert!(!indexes.catalog.indexes.contains_key(&("users".to_string(), "age".to_string())));
        assert!(matches!(
            indexes.search("users", "age", &Value::Int(25)),
            Err(CatalogError::IndexNotFound(_))
        ));
    }
}
