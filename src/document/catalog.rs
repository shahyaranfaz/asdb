/*
catalog.rs: page-0 metadata for collections.

Phase 1 stored anonymous heap files. If you knew the root PageId, you could
reopen one, but the database itself did not know names like "users".

The catalog fixes that by reserving page 0 for metadata:
  collection name -> heap root page id

Important design choice:
  One shared BufferPool coordinates page allocation and cached state for the
  catalog and every collection opened from it. This prevents independent
  allocators or caches from assigning and rewriting the same pages.
*/

use super::{Collection, CollectionError};

use crate::storage::{
    BufferPool, DiskManager, HeapFile, Page, PageId, SharedBufferPool, PAGE_SIZE,
};

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

const CATALOG_PAGE_ID: PageId = 0;
const CATALOG_MAGIC: &[u8; 8] = b"ASDBCAT1";
const DEFAULT_BUFFER_POOL_CAPACITY: usize = 1024;

/*
CATALOG FORMAT

byte 0..8       magic bytes: "ASDBCAT1"
byte 8..12      collection_count: u32
byte 12..       repeated entries:
                  [name_len: u16][name utf-8 bytes][root_page_id: u64]

This is intentionally boring. It fits easily in one page for small projects,
and if we outgrow one page later, we can make page 0 point to a catalog heap.
*/
const MAGIC_OFFSET: usize = 0;
const COUNT_OFFSET: usize = 8;
const ENTRIES_OFFSET: usize = 12;

#[derive(Debug)]
pub enum CatalogError {
    Io(std::io::Error),
    InvalidCatalog,
    CatalogFull,
    CollectionAlreadyExists(String),
    IndexAlreadyExists(String),
    IndexNotFound(String),
    CollectionNotFound(String),
    Collection(CollectionError),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Io(err) => write!(f, "catalog io error: {err}"),
            CatalogError::InvalidCatalog => write!(f, "invalid catalog page"),
            CatalogError::CatalogFull => write!(f, "catalog page is full"),
            CatalogError::CollectionAlreadyExists(name) => write!(f, "collection already exists: {name}"),
            CatalogError::IndexAlreadyExists(name) => write!(f, "index already exists: {name}"),
            CatalogError::IndexNotFound(name) => write!(f, "index not found: {name}"),
            CatalogError::CollectionNotFound(name) => write!(f, "collection not found: {name}"),
            CatalogError::Collection(err) => write!(f, "collection error: {err}"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<std::io::Error> for CatalogError {
    fn from(err: std::io::Error) -> Self {
        CatalogError::Io(err)
    }
}

impl From<CollectionError> for CatalogError {
    fn from(err: CollectionError) -> Self {
        CatalogError::Collection(err)
    }
}

pub type CatalogResult<T> = Result<T, CatalogError>;

pub struct Catalog {
    pool: SharedBufferPool,
    collections: HashMap<String, PageId>,
    pub(crate) indexes: HashMap<(String, String), PageId>,
}

impl Catalog {
    /*
    create: initialize a database file with page 0 reserved for catalog data.

    If the file is empty, allocate page 0 and write an empty catalog. If the
    file already has pages, we treat create() as "open or initialize" and read
    the existing catalog instead.
    */
    pub fn create(path: &Path) -> CatalogResult<Self> {
        let mut disk = DiskManager::open(path)?;
        if disk.num_pages() == 0 {
            let page_id = disk.allocate_page()?;
            assert_eq!(page_id, CATALOG_PAGE_ID);
            let page = encode_catalog_page(&HashMap::new(), &HashMap::new())?;
            disk.write_page(CATALOG_PAGE_ID, &page)?;
        }
        Self::open(path)
    }

    /*
    open: read page 0 and build the in-memory name -> root map.

    The in-memory map is just a cache of the persisted page. Every mutation
    rewrites page 0 immediately so a restart sees the latest catalog.
    */
    pub fn open(path: &Path) -> CatalogResult<Self> {
        let disk = DiskManager::open(path)?;
        if disk.num_pages() == 0 {
            return Err(CatalogError::InvalidCatalog);
        }
        let pool = Arc::new(Mutex::new(BufferPool::new(
            disk,
            DEFAULT_BUFFER_POOL_CAPACITY,
        )));
        let page = {
            let mut guard = pool
                .lock()
                .map_err(|_| CatalogError::Io(poisoned_pool()))?;
            guard.with_page(CATALOG_PAGE_ID, |page| {
                let mut copy = Page::new();
                copy.data.copy_from_slice(&page.data);
                copy
            })?
        };
        let (collections, indexes) = decode_catalog_page(&page)?;
        Ok(Catalog {
            pool,
            collections,
            indexes,
        })
    }

    pub fn collection_root(&self, name: &str) -> Option<PageId> {
        self.collections.get(name).copied()
    }

    pub fn collection_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.collections.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.indexes.contains_key(&(collection.to_string(), field.to_string()))
    }

    pub fn index_fields(&self, collection: &str) -> Vec<String> {
        let mut fields: Vec<String> = self.indexes
            .keys()
            .filter(|(col, _)| col == collection)
            .map(|(_, field)| field.clone())
            .collect();
        fields.sort();
        fields
    }

    pub(crate) fn shared_pool(&self) -> SharedBufferPool {
        Arc::clone(&self.pool)
    }

    /*
    create_collection: allocate a new HeapFile root and persist its name.

    The heap uses the Catalog's shared BufferPool. Since page 0 already exists,
    HeapFile::create_shared will allocate page 1 or later for the root.
    */
    pub fn create_collection(&mut self, name: &str) -> CatalogResult<Collection> {
        if self.collections.contains_key(name) {
            return Err(CatalogError::CollectionAlreadyExists(name.to_string()));
        }

        let heap = HeapFile::create_shared(Arc::clone(&self.pool))?;
        let collection = Collection::create(heap);
        self.collections.insert(name.to_string(), collection.root());
        self.flush()?;
        Ok(collection)
    }

    pub fn open_collection(&self, name: &str) -> CatalogResult<Collection> {
        let pool = Arc::clone(&self.pool);
        let root = self.collection_root_or_error(name)?;
        let heap = HeapFile::open_shared(pool, root)?;
        Ok(Collection::open(heap))
    }

    pub(crate) fn open_collection_with_pool(
        &self,
        name: &str,
        pool: &mut BufferPool,
    ) -> CatalogResult<Collection> {
        let root = self.collection_root_or_error(name)?;
        let heap = HeapFile::open_shared_with_pool(Arc::clone(&self.pool), pool, root)?;
        Ok(Collection::open(heap))
    }

    fn collection_root_or_error(&self, name: &str) -> CatalogResult<PageId> {
        let root = self
            .collections
            .get(name)
            .copied()
            .ok_or_else(|| CatalogError::CollectionNotFound(name.to_string()))?;
        Ok(root)
    }

    /*
    drop_collection: remove the catalog entry only.

    Phase 2 does not reclaim the heap pages yet. Once the catalog can track free
    heap roots or once storage has persistent free lists, drop_collection can
    walk the heap chain and free those pages too.
    */
    pub fn drop_collection(&mut self, name: &str) -> CatalogResult<()> {
        if self.collections.remove(name).is_none() {
            return Err(CatalogError::CollectionNotFound(name.to_string()));
        }
        self.flush()
    }

    pub fn flush(&self) -> CatalogResult<()> {
        let pool = Arc::clone(&self.pool);
        let mut pool = pool.lock().map_err(|_| CatalogError::Io(poisoned_pool()))?;
        self.flush_with_pool(&mut pool)
    }

    pub(crate) fn flush_with_pool(&self, pool: &mut BufferPool) -> CatalogResult<()> {
        let page = encode_catalog_page(&self.collections, &self.indexes)?;
        pool.with_page_mut(CATALOG_PAGE_ID, |target| *target = page)?;
        pool.flush_all()?;
        Ok(())
    }
}

fn poisoned_pool() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, "buffer pool lock poisoned")
}

fn encode_catalog_page(collections: &HashMap<String, PageId>,
                       indexes: &HashMap<(String, String), PageId>) -> CatalogResult<Page> {
    let mut page = Page::new();
    page.data[MAGIC_OFFSET..MAGIC_OFFSET + CATALOG_MAGIC.len()].copy_from_slice(CATALOG_MAGIC);
    write_u32(&mut page.data, COUNT_OFFSET, collections.len() as u32);

    let mut entries: Vec<(&String, &PageId)> = collections.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut pos = ENTRIES_OFFSET;
    for (name, root) in entries {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(CatalogError::CatalogFull);
        }
        let entry_len = 2 + name_bytes.len() + 8;
        if pos + entry_len > PAGE_SIZE {
            return Err(CatalogError::CatalogFull);
        }

        write_u16(&mut page.data, pos, name_bytes.len() as u16);
        pos += 2;
        page.data[pos..pos + name_bytes.len()].copy_from_slice(name_bytes);
        pos += name_bytes.len();
        write_u64(&mut page.data, pos, *root);
        pos += 8;
    }

    write_u32(&mut page.data, pos, indexes.len() as u32);
    pos += 4;
    for ((collection, field), root) in indexes {
        let collection_bytes = collection.as_bytes();
        let field_bytes = field.as_bytes();
        if collection_bytes.len() > u16::MAX as usize || field_bytes.len() > u16::MAX as usize {
            return Err(CatalogError::CatalogFull);
        }
        let entry_len = 2 + collection_bytes.len() + 2 + field_bytes.len() + 8;
        if pos + entry_len > PAGE_SIZE {
            return Err(CatalogError::CatalogFull);
        }

        // write collection name len + bytes
        write_u16(&mut page.data, pos, collection_bytes.len() as u16);
        pos += 2;
        page.data[pos..pos + collection_bytes.len()].copy_from_slice(collection_bytes);
        pos += collection_bytes.len();

        // write field name len + bytes
        write_u16(&mut page.data, pos, field_bytes.len() as u16);
        pos += 2;
        page.data[pos..pos + field_bytes.len()].copy_from_slice(field_bytes);
        pos += field_bytes.len();

        // write root u64
        write_u64(&mut page.data, pos, *root);
        pos += 8;
    }

    Ok(page)
}

fn decode_catalog_page(page: &Page)
    -> CatalogResult<(HashMap<String, PageId>, HashMap<(String, String), PageId>)> {
    if &page.data[MAGIC_OFFSET..MAGIC_OFFSET + CATALOG_MAGIC.len()] != CATALOG_MAGIC {
        return Err(CatalogError::InvalidCatalog);
    }

    let count = read_u32(&page.data, COUNT_OFFSET) as usize;
    let mut pos = ENTRIES_OFFSET;
    let mut collections = HashMap::with_capacity(count);

    for _ in 0..count {
        if pos + 2 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let name_len = read_u16(&page.data, pos) as usize;
        pos += 2;

        if pos + name_len + 8 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let name = std::str::from_utf8(&page.data[pos..pos + name_len])
            .map_err(|_| CatalogError::InvalidCatalog)?
            .to_string();
        pos += name_len;

        let root = read_u64(&page.data, pos);
        pos += 8;
        collections.insert(name, root);
    }

    //read indexes
    if pos + 4 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
    let count = read_u32(&page.data, pos) as usize;
    let mut indexes = HashMap::with_capacity(count);
    pos += 4;

    for _ in 0..count {
        // read collection name len + bytes
        if pos + 2 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let name_len = read_u16(&page.data, pos) as usize;
        pos += 2;
        if pos + name_len > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let name: String = read_string(&page.data, pos, name_len)?;
        pos += name_len;

        // read field name len + bytes
        if pos + 2 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let field_len = read_u16(&page.data, pos) as usize;
        pos += 2;
        if pos + field_len > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let field: String = read_string(&page.data, pos, field_len)?;
        pos += field_len;

        // read root u64
        if pos + 8 > PAGE_SIZE { return Err(CatalogError::InvalidCatalog); }
        let root: PageId = read_u64(&page.data, pos);
        pos += 8;

        //insert the entry
        indexes.insert((name, field), root);
    }
    Ok((collections, indexes))
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

fn write_u64(buf: &mut [u8], offset: usize, val: u64) {
    buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

fn write_u32(buf: &mut [u8], offset: usize, val: u32) {
    buf[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

fn write_u16(buf: &mut [u8], offset: usize, val: u16) {
    buf[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

fn read_string(buf: &[u8], offset: usize, len: usize) -> CatalogResult<String> {
    std::str::from_utf8(&buf[offset..offset + len])
        .map(|s| s.to_string())
        .map_err(|_| CatalogError::InvalidCatalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Value};

    use tempfile::NamedTempFile;

    #[test]
    fn test_create_catalog_reserves_page_zero() {
        let tmp = NamedTempFile::new().unwrap();
        let mut catalog = Catalog::create(tmp.path()).unwrap();
        let users = catalog.create_collection("users").unwrap();

        assert_eq!(catalog.collection_root("users"), Some(users.root()));
        assert_ne!(users.root(), CATALOG_PAGE_ID);
    }

    #[test]
    fn test_create_reopen_collection_by_name() {
        let tmp = NamedTempFile::new().unwrap();
        let saved_id;
        let saved_doc = user_doc(1, "alice");

        {
            let mut catalog = Catalog::create(tmp.path()).unwrap();
            let mut users = catalog.create_collection("users").unwrap();
            saved_id = users.insert(&saved_doc).unwrap();
            users.flush().unwrap();
            catalog.flush().unwrap();
        }

        let catalog = Catalog::open(tmp.path()).unwrap();
        assert_eq!(catalog.collection_names(), vec!["users".to_string()]);

        let mut users = catalog.open_collection("users").unwrap();
        assert_eq!(users.get(saved_id).unwrap(), Some(saved_doc));
    }

    #[test]
    fn test_phase_2_milestone_reopen_and_scan_count() {
        let tmp = NamedTempFile::new().unwrap();
        let count = 5_000;

        {
            let mut catalog = Catalog::create(tmp.path()).unwrap();
            let mut users = catalog.create_collection("users").unwrap();
            for i in 0..count {
                users.insert(&user_doc(i, &format!("user-{i}"))).unwrap();
            }
            assert_eq!(users.scan().unwrap().len(), count as usize);
            users.flush().unwrap();
            catalog.flush().unwrap();
        }

        let catalog = Catalog::open(tmp.path()).unwrap();
        let mut users = catalog.open_collection("users").unwrap();
        assert_eq!(users.scan().unwrap().len(), count as usize);
    }

    #[test]
    fn test_drop_collection_removes_catalog_entry() {
        let tmp = NamedTempFile::new().unwrap();
        let mut catalog = Catalog::create(tmp.path()).unwrap();
        catalog.create_collection("users").unwrap();
        catalog.drop_collection("users").unwrap();

        assert_eq!(catalog.collection_root("users"), None);
        assert!(matches!(
            catalog.open_collection("users"),
            Err(CatalogError::CollectionNotFound(_))
        ));
    }

    #[test]
    fn test_decode_catalog_rejects_bad_index_utf8() {
        let mut page = encode_catalog_page(&HashMap::new(), &HashMap::new()).unwrap();
        let pos = ENTRIES_OFFSET + 4;

        write_u32(&mut page.data, ENTRIES_OFFSET, 1);
        write_u16(&mut page.data, pos, 1);
        page.data[pos + 2] = 0xff;
        write_u16(&mut page.data, pos + 3, 1);
        page.data[pos + 5] = b'x';
        write_u64(&mut page.data, pos + 6, 12);

        assert!(matches!(decode_catalog_page(&page), Err(CatalogError::InvalidCatalog)));
    }

    #[test]
    fn test_encode_catalog_rejects_oversized_index_section() {
        let mut indexes = HashMap::new();
        for i in 0..400 {
            indexes.insert((format!("collection-{i}"), format!("field-{i}")), i as PageId);
        }

        assert!(matches!(
            encode_catalog_page(&HashMap::new(), &indexes),
            Err(CatalogError::CatalogFull)
        ));
    }

    fn user_doc(id: i64, name: &str) -> Document {
        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(id));
        doc.insert("name".to_string(), Value::String(name.to_string()));
        doc
    }
}
