/*
collection.rs: document-aware wrapper around HeapFile.

Phase 1 gave us raw byte storage:
  insert(bytes) -> DocId
  read(DocId) -> Option<Vec<u8>>
  scan_all() -> Vec<(DocId, Vec<u8>)>

Phase 2 starts by saying: those bytes are not random anymore, they are encoded
Documents. Collection is the thin layer that handles that translation.

This is intentionally small. It does NOT store its own name; catalog.rs owns
the name -> heap root mapping on page 0. Collection only cares about turning
Documents into heap records and back.
*/

use std::fmt;

use crate::document::{deserialize_document, serialize_document, DecodeError, Document};
use crate::storage::heap::{DocId, HeapFile};
use crate::storage::page::PageId;

/*
CollectionError: one error type for the collection boundary.

HeapFile can fail with std::io::Error because it touches disk through the
buffer pool. Document decoding can fail if the stored bytes are corrupt or not
actually a document. Collection methods expose both as one enum so callers do
not have to remember which layer produced the error.
*/
#[derive(Debug)]
pub enum CollectionError {
    Io(std::io::Error),
    Decode(DecodeError),
}

impl fmt::Display for CollectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CollectionError::Io(err) => write!(f, "collection io error: {err}"),
            CollectionError::Decode(err) => write!(f, "document decode error: {err}"),
        }
    }
}

impl std::error::Error for CollectionError {}

impl From<std::io::Error> for CollectionError {
    fn from(err: std::io::Error) -> Self {
        CollectionError::Io(err)
    }
}

impl From<DecodeError> for CollectionError {
    fn from(err: DecodeError) -> Self {
        CollectionError::Decode(err)
    }
}

pub type CollectionResult<T> = Result<T, CollectionError>;

/*
Collection: a named-ish document heap.

For now it just owns one HeapFile. Later, catalog.rs will own the name/root
mapping and hand back Collections by opening the right root page. The important
thing is that callers above this layer deal in Documents, not raw Vec<u8>.
*/
pub struct Collection {
    heap: HeapFile,
}

impl Collection {
    pub fn create(heap: HeapFile) -> Self {
        Collection { heap }
    }

    pub fn open(heap: HeapFile) -> Self {
        Collection { heap }
    }

    /*
    root: expose the heap root page so the catalog can persist it.

    This mirrors HeapFile::root(), but keeping it here means future callers do
    not have to reach through collection.heap directly.
    */
    pub fn root(&self) -> PageId {
        self.heap.root()
    }

    /*
    insert: serialize a Document and store it as one heap record.

    DocId is still the storage-level address: (page_id, slot_id). Higher layers
    can wrap this in a richer type later if we want stable public ids.
    */
    pub fn insert(&mut self, doc: &Document) -> CollectionResult<DocId> {
        let bytes = serialize_document(doc);
        Ok(self.heap.insert(&bytes)?)
    }

    /*
    get: read one document by id.

    None means the slot does not exist or has been deleted. Some(doc) means the
    bytes existed and decoded cleanly as a Document.
    */
    pub fn get(&mut self, doc_id: DocId) -> CollectionResult<Option<Document>> {
        let Some(bytes) = self.heap.read(doc_id)? else {
            return Ok(None);
        };
        Ok(Some(deserialize_document(&bytes)?))
    }

    pub fn delete(&mut self, doc_id: DocId) -> CollectionResult<()> {
        Ok(self.heap.delete(doc_id)?)
    }

    /*
    scan: materialize all live documents in this collection.

    This matches HeapFile::scan_all for now. The query executor should
    eventually get a streaming version so big scans do not allocate everything
    at once.
    */
    pub fn scan(&mut self) -> CollectionResult<Vec<(DocId, Document)>> {
        let records = self.heap.scan_all()?;
        let mut docs = Vec::with_capacity(records.len());
        for (doc_id, bytes) in records {
            docs.push((doc_id, deserialize_document(&bytes)?));
        }
        Ok(docs)
    }

    pub fn flush(&mut self) -> CollectionResult<()> {
        Ok(self.heap.flush()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Value;
    use crate::storage::buffer_pool::BufferPool;
    use crate::storage::disk::DiskManager;
    use tempfile::NamedTempFile;

    fn make_collection(capacity: usize) -> (Collection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let disk = DiskManager::open(tmp.path()).unwrap();
        let bp = BufferPool::new(disk, capacity);
        let heap = HeapFile::create(bp).unwrap();
        (Collection::create(heap), tmp)
    }

    #[test]
    fn test_insert_get_delete_document() {
        let (mut collection, _tmp) = make_collection(4);
        let doc = user_doc(1, "alice", 25);

        let id = collection.insert(&doc).unwrap();
        assert_eq!(collection.get(id).unwrap(), Some(doc));

        collection.delete(id).unwrap();
        assert_eq!(collection.get(id).unwrap(), None);
    }

    #[test]
    fn test_scan_documents() {
        let (mut collection, _tmp) = make_collection(4);
        let docs = vec![
            user_doc(1, "alice", 25),
            user_doc(2, "bob", 17),
            user_doc(3, "cara", 31),
        ];

        for doc in &docs {
            collection.insert(doc).unwrap();
        }

        let scanned: Vec<Document> = collection
            .scan()
            .unwrap()
            .into_iter()
            .map(|(_, doc)| doc)
            .collect();

        assert_eq!(scanned.len(), docs.len());
        for doc in docs {
            assert!(scanned.contains(&doc));
        }
    }

    #[test]
    fn test_persistence_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let root;
        let saved_id;
        let saved_doc = user_doc(7, "dina", 44);

        {
            let disk = DiskManager::open(tmp.path()).unwrap();
            let bp = BufferPool::new(disk, 4);
            let heap = HeapFile::create(bp).unwrap();
            let mut collection = Collection::create(heap);
            root = collection.root();
            saved_id = collection.insert(&saved_doc).unwrap();
            collection.flush().unwrap();
        }

        let disk = DiskManager::open(tmp.path()).unwrap();
        let bp = BufferPool::new(disk, 4);
        let heap = HeapFile::open(bp, root).unwrap();
        let mut collection = Collection::open(heap);

        assert_eq!(collection.get(saved_id).unwrap(), Some(saved_doc));
    }

    fn user_doc(id: i64, name: &str, age: i64) -> Document {
        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(id));
        doc.insert("name".to_string(), Value::String(name.to_string()));
        doc.insert("age".to_string(), Value::Int(age));
        doc
    }
}
