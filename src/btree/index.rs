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

    /*
    COMPOUND INDEXES are stored as one catalog entry whose field name is the
    parts joined by FIELD_SEPARATOR, a byte no ASL identifier can contain. Its
    btree key is each part's serialized value, length-prefixed so "ab" + "c"
    and "a" + "bc" cannot collide.

    Only uniqueness uses them today. The planner looks indexes up by a single
    field name, which never matches a joined one, so a compound index is never
    mistaken for one it could answer a query with.
    */
    pub fn create_compound(&mut self, collection: &str, fields: &[String], unique: bool) -> CatalogResult<()> {
        if fields.len() == 1 {
            return self.create(collection, &fields[0], unique);
        }

        self.catalog
            .collection_root(collection)
            .ok_or_else(|| CatalogError::CollectionNotFound(collection.to_string()))?;

        let name = join_fields(fields);
        let key = (collection.to_string(), name);
        if self.catalog.indexes.contains_key(&key) {
            return Err(CatalogError::IndexAlreadyExists(collection.to_string()));
        }

        let root = {
            let btree = BTree::new(self.pool)?;
            btree.root()
        };

        let existing = self.catalog.open_collection(self.pool, collection)?;
        let docs = existing.scan(self.pool)?;
        existing.flush(self.pool)?;

        if unique {
            let mut seen = std::collections::HashSet::new();
            for (_, doc) in &docs {
                if let Some(bytes) = compound_key(fields, doc) {
                    if !seen.insert(bytes) {
                        return Err(CatalogError::DuplicateKey(
                            collection.to_string(),
                            fields.join(", "),
                        ));
                    }
                }
            }
        }

        let mut btree = BTree::open(root, self.pool);
        for (doc_id, doc) in docs {
            if let Some(bytes) = compound_key(fields, &doc) {
                btree.insert(&bytes, doc_id)?;
            }
        }

        self.catalog.indexes.insert(key.clone(), root);
        if unique {
            self.catalog.unique_indexes.insert(key);
        }
        self.catalog.flush()?;
        Ok(())
    }

    pub fn create(&mut self, collection: &str, field: &str, unique: bool) -> CatalogResult<()> {
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

        /*
        A unique index is refused if the data already breaks it, rather than
        being created and then lying: an index that claims uniqueness over
        duplicates would let the next insert through on a check that is already
        false.
        */
        if unique {
            let mut seen = std::collections::HashSet::new();
            for (_, doc) in &docs {
                if let Some(value) = doc.get(field) {
                    if !seen.insert(serialize_key(value)) {
                        return Err(CatalogError::DuplicateKey(
                            collection.to_string(),
                            field.to_string(),
                        ));
                    }
                }
            }
        }

        let mut btree = BTree::open(root, self.pool);
        for (doc_id, doc) in docs {
            if let Some(value) = doc.get(field) {
                let key = serialize_key(value);
                btree.insert(&key, doc_id)?;
            }
        }

        self.catalog.indexes.insert(key.clone(), root);
        if unique {
            self.catalog.unique_indexes.insert(key);
        }
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
        self.catalog.unique_indexes.remove(&key);
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

    /*
    check_unique: refuse a document that would duplicate a unique index's value.

    Run before anything is written, so a rejected insert leaves nothing behind.
    A document that simply lacks the field is allowed: the index has no entry for
    it, so there is nothing to collide with, which matches the index not storing
    missing fields in the first place.
    */
    pub fn check_unique(&mut self, collection: &str, doc: &Document) -> CatalogResult<()> {
        self.check_unique_except(collection, doc, None)
    }

    /*
    Check a prospective document against every unique index. `except` is the
    stored row being replaced, if any: its unchanged unique values must not be
    treated as collisions with themselves.
    */
    pub fn check_unique_except(&mut self, collection: &str, doc: &Document, except: Option<DocId>) -> CatalogResult<()> {
        let unique: Vec<String> = self.catalog.unique_index_fields(collection);
        for name in unique {
            let key = match index_key_for(&name, doc) {
                Some(key) => key,
                None => continue,
            };
            let root = match self.catalog.indexes.get(&(collection.to_string(), name.clone())) {
                Some(root) => *root,
                None => continue,
            };

            let mut btree = BTree::open(root, self.pool);
            if let Some(found) = btree.search(&key)? {
                if Some(found) != except {
                    return Err(CatalogError::DuplicateKey(
                        collection.to_string(),
                        name.replace(FIELD_SEPARATOR, ", "),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate stored keys and duplicates inside a batch before changing a heap page.
    pub fn check_unique_batch(&mut self, collection: &str, docs: &[Document]) -> CatalogResult<()> {
        let mut seen = std::collections::HashSet::new();
        let unique = self.catalog.unique_index_fields(collection);
        for doc in docs {
            self.check_unique(collection, doc)?;
            for name in &unique {
                let key = match index_key_for(name, doc) {
                    Some(key) => key,
                    None => continue,
                };
                if !seen.insert((name.clone(), key)) {
                    return Err(CatalogError::DuplicateKey(
                        collection.to_string(),
                        name.replace(FIELD_SEPARATOR, ", "),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn on_insert(&mut self, collection: &str, doc: &Document,
                     doc_id: DocId) -> CatalogResult<()> {

        let index_keys: Vec<((String, String), PageId)> = self.indexes_for(collection);
        for ((_, field), root) in index_keys {
            if let Some(key) = index_key_for(&field, doc) {
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
            if let Some(key) = index_key_for(&field, doc) {
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
            indexes.create("users", "age", false).unwrap();
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
            indexes.create("users", "age", false),
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
        indexes.create("users", "age", false).unwrap();

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
            indexes.create("users", "age", false).unwrap();
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
            indexes.create("users", "age", false).unwrap();
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
        indexes.create("users", "age", false).unwrap();
        indexes.drop("users", "age").unwrap();

        assert!(!indexes.catalog.indexes.contains_key(&("users".to_string(), "age".to_string())));
        assert!(matches!(
            indexes.search("users", "age", &Value::Int(25)),
            Err(CatalogError::IndexNotFound(_))
        ));
    }
}

/*
The byte that joins a compound index's field names into one catalog entry.
Chosen because an ASL identifier cannot contain it, so a joined name can never
be confused with a real single field name.
*/
pub const FIELD_SEPARATOR: char = '\u{1}';

pub fn join_fields(fields: &[String]) -> String {
    fields.join(&FIELD_SEPARATOR.to_string())
}

/*
index_key_for: the btree key a document has under an index, single or compound.

None means the document is not indexed here: a missing field has no entry, and
a compound index needs every part, since a partial key would sort among complete
ones and collide with them.
*/
fn index_key_for(name: &str, doc: &Document) -> Option<Vec<u8>> {
    if !name.contains(FIELD_SEPARATOR) {
        return doc.get(name).map(serialize_key);
    }
    let fields: Vec<String> = name.split(FIELD_SEPARATOR).map(str::to_string).collect();
    compound_key(&fields, doc)
}

// each part's serialized value, length-prefixed so "ab" + "c" cannot equal "a" + "bc"
fn compound_key(fields: &[String], doc: &Document) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for field in fields {
        let value = doc.get(field)?;
        let bytes = serialize_key(value);
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    Some(out)
}
