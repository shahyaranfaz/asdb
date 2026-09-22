/*
catalog.rs: page-0 metadata for collections.

Phase 1 stored anonymous heap files. If you knew the root PageId, you could
reopen one, but the database itself did not know names like "users".

The catalog fixes that by reserving page 0 for metadata:
  collection name -> heap root page id

Important design choice:
  The Catalog does NOT own a buffer pool. create_collection and open_collection
  take the caller's shared `&mut BufferPool`, and Database owns the single pool
  for the whole engine. It used to open a short-lived DiskManager and
  BufferPool per call, which was both a corruption bug and the dominant cost in
  every write path. See tests/multi_collection.rs.

  The catalog still owns its path, because flush() rewrites page 0 directly,
  deliberately outside the pool. Nothing else ever allocates page 0, so the two
  writers cannot collide over it.

Original note:
  Catalog owns the file path and opens short-lived DiskManager / BufferPool
  handles when it needs to create or open a collection. This keeps Phase 2
  simple while HeapFile still owns its BufferPool. Later, when we want multiple
  active collections and indexes sharing one cache, this should become a
  Database object with one shared BufferPool.
*/

use super::{Collection, CollectionError};

use crate::storage::{BufferPool, DiskManager, HeapFile, Page, PageId, PAGE_SIZE};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

const CATALOG_PAGE_ID: PageId = 0;
const CATALOG_MAGIC: &[u8; 8] = b"ASDBCAT1";

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
// the last bytes of every catalog page hold the id of the page continuing it, or zero
const NEXT_POINTER_SIZE: usize = 8;

#[derive(Debug)]
pub enum CatalogError {
    Io(std::io::Error),
    InvalidCatalog,
    CatalogFull,
    /// A value that a unique index already holds.
    DuplicateKey(String, String),
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
            CatalogError::DuplicateKey(collection, field) => {
                write!(f, "duplicate value for unique index {collection}.{field}")
            }
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
    path: PathBuf,
    collections: HashMap<String, PageId>,
    pub(crate) indexes: HashMap<(String, String), PageId>,
    /*
    Which of those indexes reject a duplicate value on insert.

    A separate set rather than a field on the entry, so the catalog page keeps
    its existing layout and a database written before uniqueness existed still
    decodes: an old page simply ends after the index section, and this comes
    back empty.
    */
    pub(crate) unique_indexes: HashSet<(String, String)>,
    // the pages after page 0 that the catalog spills onto, in order
    overflow: Vec<PageId>,
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
            let bytes = encode_catalog(&HashMap::new(), &HashMap::new(), &HashSet::new())?;
            let mut page = Page::new();
            page.data[..bytes.len()].copy_from_slice(&bytes);
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
        let mut disk = DiskManager::open(path)?;
        if disk.num_pages() == 0 {
            return Err(CatalogError::InvalidCatalog);
        }
        let (bytes, overflow) = read_catalog_chain(&mut disk)?;
        let (collections, indexes, unique_indexes) = decode_catalog(&bytes)?;
        Ok(Catalog {
            path: path.to_path_buf(),
            collections,
            indexes,
            unique_indexes,
            overflow,
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

    pub fn is_unique_index(&self, collection: &str, field: &str) -> bool {
        self.unique_indexes.contains(&(collection.to_string(), field.to_string()))
    }

    // every unique index on this collection, as field names
    pub fn unique_index_fields(&self, collection: &str) -> Vec<String> {
        let mut fields: Vec<String> = self.unique_indexes
            .iter()
            .filter(|(col, _)| col == collection)
            .map(|(_, field)| field.clone())
            .collect();
        fields.sort();
        fields
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

    /*
    create_collection: allocate a new HeapFile root and persist its name.

    This opens a fresh BufferPool over the same database file. Since page 0
    already exists, HeapFile::create will allocate page 1 or later for the root.
    */
    pub fn create_collection(
        &mut self,
        bp: &mut BufferPool,
        name: &str,
    ) -> CatalogResult<Collection> {
        if self.collections.contains_key(name) {
            return Err(CatalogError::CollectionAlreadyExists(name.to_string()));
        }

        let heap = HeapFile::create(bp)?;
        let collection = Collection::create(heap);
        self.collections.insert(name.to_string(), collection.root());
        self.flush()?;
        Ok(collection)
    }

    pub fn open_collection(
        &self,
        bp: &mut BufferPool,
        name: &str,
    ) -> CatalogResult<Collection> {
        let root = self
            .collections
            .get(name)
            .copied()
            .ok_or_else(|| CatalogError::CollectionNotFound(name.to_string()))?;

        let heap = HeapFile::open(bp, root)?;
        Ok(Collection::open(heap))
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

    /*
    flush: write the catalog across as many pages as it needs.

    Page 0 holds the first chunk, and the LAST eight bytes of every catalog page
    hold the page id of the next chunk, or zero for none. Zero is safe as "none"
    because page 0 is the catalog itself and can never be a continuation.

    A catalog written before this existed occupies one page whose trailer is
    zeros, so it reads back as a single chunk, unchanged.

    Overflow pages are allocated once and kept in self.overflow, so repeated
    flushes reuse them rather than leaking a page per write.
    */
    pub fn flush(&mut self) -> CatalogResult<()> {
        let bytes = encode_catalog(&self.collections, &self.indexes, &self.unique_indexes)?;
        let mut disk = DiskManager::open(&self.path)?;

        let chunk_size = PAGE_SIZE - NEXT_POINTER_SIZE;
        let chunks: Vec<&[u8]> = bytes.chunks(chunk_size).collect();
        while self.overflow.len() < chunks.len().saturating_sub(1) {
            let page_id = disk.allocate_page()?;
            self.overflow.push(page_id);
        }

        for (i, chunk) in chunks.iter().enumerate() {
            let page_id = if i == 0 { CATALOG_PAGE_ID } else { self.overflow[i - 1] };
            let next = if i + 1 < chunks.len() { self.overflow[i] } else { 0 };

            let mut page = Page::new();
            page.data[..chunk.len()].copy_from_slice(chunk);
            write_u64(&mut page.data, PAGE_SIZE - NEXT_POINTER_SIZE, next);
            disk.write_page(page_id, &page)?;
        }
        Ok(())
    }
}

/*
encode_catalog: the whole catalog as bytes, however long that is.

It was one fixed page, and "too big" was CatalogFull. The bytes are now split
across pages by flush(), so the only limit left is the file. The layout inside
is unchanged, which is what keeps a single-page catalog written earlier
readable.
*/
/*
GrowBuf: a byte buffer that grows to fit whatever is written into it.

The encoder addresses bytes by position, as it did when writing into a fixed
page. This keeps that shape while removing the one-page limit: writing past the
end extends the buffer instead of failing.
*/
struct GrowBuf {
    data: Vec<u8>,
}

impl GrowBuf {
    // sized up front from the entries it is about to hold, so writing by position is safe
    fn sized(collections: &HashMap<String, PageId>,
             indexes: &HashMap<(String, String), PageId>,
             unique_indexes: &HashSet<(String, String)>) -> Self {
        let mut size = ENTRIES_OFFSET;
        for name in collections.keys() {
            size += 2 + name.len() + 8;
        }
        size += 4;
        for (collection, field) in indexes.keys() {
            size += 2 + collection.len() + 2 + field.len() + 8;
        }
        size += 4;
        for (collection, field) in unique_indexes {
            size += 2 + collection.len() + 2 + field.len();
        }
        GrowBuf { data: vec![0u8; size] }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

fn encode_catalog(collections: &HashMap<String, PageId>,
                  indexes: &HashMap<(String, String), PageId>,
                  unique_indexes: &HashSet<(String, String)>) -> CatalogResult<Vec<u8>> {
    let mut page = GrowBuf::sized(collections, indexes, unique_indexes);
    page.data[MAGIC_OFFSET..MAGIC_OFFSET + CATALOG_MAGIC.len()].copy_from_slice(CATALOG_MAGIC);
    write_u32(&mut page.data, COUNT_OFFSET, collections.len() as u32);

    let mut entries: Vec<(&String, &PageId)> = collections.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut pos = ENTRIES_OFFSET;
    for (name, root) in entries {
        let name_bytes = name.as_bytes();
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


    /*
    The unique section. Written last so a decoder that predates it stops after
    the indexes and simply never reads these bytes.
    */
    write_u32(&mut page.data, pos, unique_indexes.len() as u32);
    pos += 4;

    let mut unique: Vec<&(String, String)> = unique_indexes.iter().collect();
    unique.sort();
    for (collection, field) in unique {
        let collection_bytes = collection.as_bytes();
        let field_bytes = field.as_bytes();
        write_u16(&mut page.data, pos, collection_bytes.len() as u16);
        pos += 2;
        page.data[pos..pos + collection_bytes.len()].copy_from_slice(collection_bytes);
        pos += collection_bytes.len();

        write_u16(&mut page.data, pos, field_bytes.len() as u16);
        pos += 2;
        page.data[pos..pos + field_bytes.len()].copy_from_slice(field_bytes);
        pos += field_bytes.len();
    }

    Ok(page.into_bytes())
}

type DecodedCatalog = (
    HashMap<String, PageId>,
    HashMap<(String, String), PageId>,
    HashSet<(String, String)>,
);

/*
read_catalog_chain: page 0 and every page it points at, joined back into one
buffer, plus the ids of those continuation pages so flush can reuse them.
*/
fn read_catalog_chain(disk: &mut DiskManager) -> CatalogResult<(Vec<u8>, Vec<PageId>)> {
    let mut bytes = Vec::new();
    let mut overflow = Vec::new();
    let mut page_id = CATALOG_PAGE_ID;

    loop {
        let page = disk.read_page(page_id)?;
        bytes.extend_from_slice(&page.data[..PAGE_SIZE - NEXT_POINTER_SIZE]);
        let next = read_u64(&page.data, PAGE_SIZE - NEXT_POINTER_SIZE);
        if next == 0 {
            break;
        }
        if next >= disk.num_pages() || overflow.contains(&next) {
            return Err(CatalogError::InvalidCatalog);
        }
        overflow.push(next);
        page_id = next;
    }
    Ok((bytes, overflow))
}

struct CatalogBytes<'a> {
    data: &'a [u8],
}

fn decode_catalog(bytes: &[u8]) -> CatalogResult<DecodedCatalog> {
    let page = CatalogBytes { data: bytes };
    if page.data.len() < ENTRIES_OFFSET
        || &page.data[MAGIC_OFFSET..MAGIC_OFFSET + CATALOG_MAGIC.len()] != CATALOG_MAGIC
    {
        return Err(CatalogError::InvalidCatalog);
    }

    let count = read_u32(&page.data, COUNT_OFFSET) as usize;
    let mut pos = ENTRIES_OFFSET;
    let mut collections = HashMap::with_capacity(count);

    for _ in 0..count {
        if pos + 2 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let name_len = read_u16(&page.data, pos) as usize;
        pos += 2;

        if pos + name_len + 8 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let name = std::str::from_utf8(&page.data[pos..pos + name_len])
            .map_err(|_| CatalogError::InvalidCatalog)?
            .to_string();
        pos += name_len;

        let root = read_u64(&page.data, pos);
        pos += 8;
        collections.insert(name, root);
    }

    //read indexes
    if pos + 4 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
    let count = read_u32(&page.data, pos) as usize;
    let mut indexes = HashMap::with_capacity(count);
    pos += 4;

    for _ in 0..count {
        // read collection name len + bytes
        if pos + 2 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let name_len = read_u16(&page.data, pos) as usize;
        pos += 2;
        if pos + name_len > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let name: String = read_string(&page.data, pos, name_len)?;
        pos += name_len;

        // read field name len + bytes
        if pos + 2 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let field_len = read_u16(&page.data, pos) as usize;
        pos += 2;
        if pos + field_len > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let field: String = read_string(&page.data, pos, field_len)?;
        pos += field_len;

        // read root u64
        if pos + 8 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
        let root: PageId = read_u64(&page.data, pos);
        pos += 8;

        //insert the entry
        indexes.insert((name, field), root);
    }

    /*
    The unique section, written after the indexes.

    A page from before uniqueness existed has nothing here, and its trailing
    bytes are zeros, so a count of zero and "no room left" both mean the same
    thing: no unique indexes. That is what keeps old database files readable.
    */
    let mut unique_indexes = HashSet::new();
    if pos + 4 <= page.data.len() {
        let count = read_u32(&page.data, pos) as usize;
        pos += 4;
        for _ in 0..count {
            if pos + 2 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
            let name_len = read_u16(&page.data, pos) as usize;
            pos += 2;
            if pos + name_len > page.data.len() { return Err(CatalogError::InvalidCatalog); }
            let name: String = read_string(&page.data, pos, name_len)?;
            pos += name_len;

            if pos + 2 > page.data.len() { return Err(CatalogError::InvalidCatalog); }
            let field_len = read_u16(&page.data, pos) as usize;
            pos += 2;
            if pos + field_len > page.data.len() { return Err(CatalogError::InvalidCatalog); }
            let field: String = read_string(&page.data, pos, field_len)?;
            pos += field_len;

            unique_indexes.insert((name, field));
        }
    }

    Ok((collections, indexes, unique_indexes))
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

    /*
    Catalog no longer owns a pool, so these build the pair the way Database
    does: catalog FIRST (it allocates page 0 through its own short-lived
    handle), THEN the pool, so the pool's DiskManager sees the real file
    length. Reversing that order reintroduces the page-0 collision.
    */
    fn open_catalog(path: &std::path::Path) -> (Catalog, BufferPool) {
        let catalog = Catalog::create(path).unwrap();
        let disk = DiskManager::open(path).unwrap();
        (catalog, BufferPool::new(disk, 64))
    }

    fn reopen_catalog(path: &std::path::Path) -> (Catalog, BufferPool) {
        let catalog = Catalog::open(path).unwrap();
        let disk = DiskManager::open(path).unwrap();
        (catalog, BufferPool::new(disk, 64))
    }

    #[test]
    fn test_create_catalog_reserves_page_zero() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut bp) = open_catalog(tmp.path());
        let users = catalog.create_collection(&mut bp, "users").unwrap();

        assert_eq!(catalog.collection_root("users"), Some(users.root()));
        assert_ne!(users.root(), CATALOG_PAGE_ID);
    }

    #[test]
    fn test_create_reopen_collection_by_name() {
        let tmp = NamedTempFile::new().unwrap();
        let saved_id;
        let saved_doc = user_doc(1, "alice");

        {
            let (mut catalog, mut bp) = open_catalog(tmp.path());
            let mut users = catalog.create_collection(&mut bp, "users").unwrap();
            saved_id = users.insert(&mut bp, &saved_doc).unwrap();
            users.flush(&mut bp).unwrap();
            catalog.flush().unwrap();
        }

        let (catalog, mut bp) = reopen_catalog(tmp.path());
        assert_eq!(catalog.collection_names(), vec!["users".to_string()]);

        let users = catalog.open_collection(&mut bp, "users").unwrap();
        assert_eq!(users.get(&mut bp, saved_id).unwrap(), Some(saved_doc));
    }

    #[test]
    fn test_phase_2_milestone_reopen_and_scan_count() {
        let tmp = NamedTempFile::new().unwrap();
        let count = 5_000;

        {
            let (mut catalog, mut bp) = open_catalog(tmp.path());
            let mut users = catalog.create_collection(&mut bp, "users").unwrap();
            for i in 0..count {
                users.insert(&mut bp, &user_doc(i, &format!("user-{i}"))).unwrap();
            }
            assert_eq!(users.scan(&mut bp).unwrap().len(), count as usize);
            users.flush(&mut bp).unwrap();
            catalog.flush().unwrap();
        }

        let (catalog, mut bp) = reopen_catalog(tmp.path());
        let users = catalog.open_collection(&mut bp, "users").unwrap();
        assert_eq!(users.scan(&mut bp).unwrap().len(), count as usize);
    }

    #[test]
    fn test_drop_collection_removes_catalog_entry() {
        let tmp = NamedTempFile::new().unwrap();
        let (mut catalog, mut bp) = open_catalog(tmp.path());
        catalog.create_collection(&mut bp, "users").unwrap();
        catalog.drop_collection("users").unwrap();

        assert_eq!(catalog.collection_root("users"), None);
        assert!(matches!(
            catalog.open_collection(&mut bp, "users"),
            Err(CatalogError::CollectionNotFound(_))
        ));
    }

    #[test]
    fn test_decode_catalog_rejects_bad_index_utf8() {
        let mut bytes = encode_catalog(&HashMap::new(), &HashMap::new(), &HashSet::new()).unwrap();
        bytes.resize(PAGE_SIZE, 0);
        let pos = ENTRIES_OFFSET + 4;

        write_u32(&mut bytes, ENTRIES_OFFSET, 1);
        write_u16(&mut bytes, pos, 1);
        bytes[pos + 2] = 0xff;
        write_u16(&mut bytes, pos + 3, 1);
        bytes[pos + 5] = b'x';
        write_u64(&mut bytes, pos + 6, 12);

        assert!(matches!(decode_catalog(&bytes), Err(CatalogError::InvalidCatalog)));
    }

    /*
    A catalog larger than one page used to be CatalogFull. It now spans pages,
    so the test is that it survives the round trip rather than that it fails.
    */
    #[test]
    fn test_catalog_larger_than_a_page_round_trips() {
        let mut indexes = HashMap::new();
        for i in 0..400 {
            indexes.insert((format!("collection-{i}"), format!("field-{i}")), i as PageId);
        }

        let bytes = encode_catalog(&HashMap::new(), &indexes, &HashSet::new()).unwrap();
        assert!(bytes.len() > PAGE_SIZE, "this catalog is meant to need more than one page");

        let (_, decoded, _) = decode_catalog(&bytes).unwrap();
        assert_eq!(decoded, indexes);
    }

    fn user_doc(id: i64, name: &str) -> Document {
        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(id));
        doc.insert("name".to_string(), Value::String(name.to_string()));
        doc
    }
}
