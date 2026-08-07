/*
heap.rs: variable-length records stored in a chain of slotted pages.

PAGE LAYOUT (each page in the chain looks like this)

  byte 0..8       next_page_id: u64  (NO_NEXT sentinel if this is the tail)
  byte 8..10      slot_count  : u16  (number of slots, including tombstones)
  byte 10..12     free_end    : u16  (records grow down from free_end)
  byte 12..       slot_array  : slot[slot_count]
                                  each slot = [offset: u16, length: u16]
                                  length == 0 means the slot was deleted (tombstone)
  free space in the middle
  ...records...   records grow from the end of the page down toward the slot
                  array. record bytes live at offset..offset+length.

INSERT
  walk the chain from root. on the first page with enough free space,
  append the record and add a new slot. if no page has space, allocate a
  new page, link it from the current tail, insert into it.

READ
  parse the slot at (page_id, slot_id), return None for tombstones,
  else copy the record bytes out.

DELETE
  set the slot's length to 0. the record bytes are still in the page but
  unreferenced; a compaction pass could reclaim them later. v1 doesn't
  compact.

SCAN_ALL
  walk the chain from root, yield every non-tombstone slot's bytes.

a doc id (DocId) is the pair (page_id, slot_id). that's what callers hold
onto to reference a specific record.
*/

use super::{BufferPool, Page, PageId, PAGE_SIZE};

pub type SlotId = u16;
pub type DocId = (PageId, SlotId);

// header byte offsets (see layout diagram above)
const HEADER_NEXT_OFFSET: usize = 0;
const HEADER_SLOT_COUNT_OFFSET: usize = 8;
const HEADER_FREE_END_OFFSET: usize = 10;
const HEADER_SIZE: usize = 12;
const SLOT_SIZE: usize = 4;

/*
NO_NEXT: sentinel meaning "this is the last page in the chain".

we can't use 0 because page 0 is a valid page id in v1. u64::MAX is safe,
we will never have that many pages. (4096 bytes * u64::MAX is more than
all the storage in human history many times over.)
*/
const NO_NEXT: PageId = u64::MAX;

/*
max bytes a single record can be. has to fit inside one page along with
the page header and at least one slot entry.

`const fn` would let us compute this at compile time, but a plain const
expression works fine since all inputs are const.
*/
pub const MAX_RECORD_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

/*
HeapFile: a chain of slotted pages anchored at `root`, with `last_page`
cached for O(1) appends.

DOES NOT OWN A BUFFER POOL. Every method that touches storage takes
`bp: &mut BufferPool`. This is the "multiple heaps sharing one pool" change
this comment used to anticipate.

It is not tidiness. When each HeapFile owned a pool over the same file, two
collections got two DiskManagers with independent num_pages counters and
handed out the SAME page id to both, and the per-pool caches hid it until a
restart. tests/multi_collection.rs is the regression test. It is also three
separate performance bugs: opening a pool per document was the dominant cost
in insert, delete, and index fetch.

Historical note, kept because it was true when written: originally owned its
BufferPool for v1. when phase 2 introduces collections (multiple
heaps sharing one pool), this will become a borrow / shared handle.

why cache last_page? without it, every insert walks the chain from root
looking for a page with room, which is O(n) per insert and O(n^2) total
for n inserts. plans.txt asks for 100k-document tests. with the tail
cache, inserts are O(1) amortized.

trade-off: we always append to the tail, never reuse holes left by
delete. that wastes space until compaction. compaction is out of scope
for v1.
*/
pub struct HeapFile {
    root: PageId,
    last_page: PageId,
}

impl HeapFile {
    /*
    create: allocate the root page, initialize it as an empty heap page,
    return a fresh HeapFile.

    `bp.new_page(...)` returns (PageId, R) where R is whatever the closure
    returns. we ignore the unit return with `_` and just keep the page id.
    */
    pub fn create(bp: &mut BufferPool) -> std::io::Result<Self> {
        let (root, _) = bp.new_page(|page| init_heap_page(page))?;
        Ok(HeapFile { root, last_page: root })
    }

    /*
    open: attach to an existing heap whose root page id we already know.

    we walk the chain once on open to find the tail page so future inserts
    are O(1). that walk is O(n in pages), but it happens once at startup,
    which is acceptable.

    note this used to be infallible (returned Self directly). now it does
    IO (chain walk), so it returns Result. callers that already had a
    HeapFile::open(bp, root) need to adapt.
    */
    pub fn open(bp: &mut BufferPool, root: PageId) -> std::io::Result<Self> {
        let last_page = find_last_page(bp, root)?;
        Ok(HeapFile { root, last_page })
    }

    pub fn root(&self) -> PageId {
        self.root
    }

    /*
    insert: append a record to the tail page, return its DocId.

    fast path:
      check if last_page has room. if yes, insert there.

    slow path:
      allocate a new page, link the old tail to it, insert into the new
      page, advance last_page.

    why two trips on the fast path (one read for free space, one write to
    insert) instead of one with_page_mut? because with_page_mut marks the
    page dirty even if we don't actually mutate. spending a cheap read to
    avoid an extra disk flush on a full tail is a good trade.
    */
    pub fn insert(&mut self, bp: &mut BufferPool, data: &[u8]) -> std::io::Result<DocId> {
        assert!(!data.is_empty(), "empty records are not supported");
        assert!(
            data.len() <= MAX_RECORD_SIZE,
            "record larger than MAX_RECORD_SIZE"
        );
        let needed = data.len() + SLOT_SIZE;

        // fast path: tail page has room
        let tail = self.last_page;
        let space = bp.with_page(tail, page_free_space)?;
        if space >= needed {
            let slot = bp.with_page_mut(tail, |page| {
                try_insert_in_page(page, data).expect("space check said it fits")
            })?;
            return Ok((tail, slot));
        }

        /*
        Another handle to this heap may have extended the chain since this
        handle cached last_page. Catch up while the caller holds the database's
        sole pool before linking a new page, or a stale handle can overwrite a
        newer tail link and orphan committed records.
        */
        let current_tail = find_last_page(bp, tail)?;
        self.last_page = current_tail;
        if current_tail != tail {
            let space = bp.with_page(current_tail, page_free_space)?;
            if space >= needed {
                let slot = bp.with_page_mut(current_tail, |page| {
                    try_insert_in_page(page, data).expect("space check said it fits")
                })?;
                return Ok((current_tail, slot));
            }
        }

        // slow path: extend the current tail
        let (new_id, slot) = bp.new_page(|page| {
            init_heap_page(page);
            try_insert_in_page(page, data).expect("fresh page must fit")
        })?;
        bp.with_page_mut(current_tail, |page| {
            write_u64(&mut page.data, HEADER_NEXT_OFFSET, new_id);
        })?;
        self.last_page = new_id;
        Ok((new_id, slot))
    }

    /*
    read: fetch a record by DocId, or None if the slot is a tombstone.

    returns Vec<u8> (owned copy) rather than &[u8] because we can't hand
    out a reference into a buffer pool frame across function boundaries
    (see with_page docs in buffer_pool.rs for why).
    */
    pub fn read(&self, bp: &mut BufferPool, doc_id: DocId) -> std::io::Result<Option<Vec<u8>>> {
        let (pid, slot_id) = doc_id;
        bp.with_page(pid, |page| read_slot(page, slot_id))
    }

    /*
    delete: tombstone a slot. the underlying bytes stay until a future
    compaction (not implemented in v1).

    if the slot is already a tombstone, this is a no-op.
    */
    pub fn delete(&self, bp: &mut BufferPool, doc_id: DocId) -> std::io::Result<()> {
        let (pid, slot_id) = doc_id;
        bp.with_page_mut(pid, |page| {
            tombstone_slot(page, slot_id);
        })
    }

    /*
    scan_all: walk the whole chain and return every live (DocId, bytes) pair.

    Vec<(DocId, Vec<u8>)> materializes the whole thing in memory. fine for
    v1 and for tests. phase 5 will swap this for a streaming iterator
    (Volcano model) so we don't pay memory for large scans.

    `while let Some(pid) = current` is the loop form of `if let`. it keeps
    running as long as the pattern matches, here as long as `current` has a
    Some(PageId).
    */
    /*
    scan_page: read ONE page of the chain, returning its live records and the
    id of the next page.

    This is scan_all's loop body, exposed so a caller can drive the walk itself
    instead of receiving the whole collection at once. That is what lets the
    query executor stream: `from events | limit 25` over a million documents
    stops after the first page rather than materialising a million records.

    A PAGE at a time rather than a RECORD at a time is deliberate. The unit of
    IO is a page, so fetching one record per call would re-pin and re-read the
    same frame for every slot on it. A page of records is a small, bounded
    buffer and it keeps the pin held exactly once.

    Returns None for the next page when this is the tail.
    */
    pub fn scan_page(
        &self,
        bp: &mut BufferPool,
        page_id: PageId,
    ) -> std::io::Result<(Vec<(DocId, Vec<u8>)>, Option<PageId>)> {
        bp.with_page(page_id, |page| {
            let mut recs: Vec<(DocId, Vec<u8>)> = Vec::new();
            let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET);
            for s in 0..slot_count {
                if let Some(bytes) = read_slot(page, s) {
                    recs.push(((page_id, s), bytes));
                }
            }
            let next = read_u64(&page.data, HEADER_NEXT_OFFSET);
            (recs, if next == NO_NEXT { None } else { Some(next) })
        })
    }

    pub fn scan_all(&self, bp: &mut BufferPool) -> std::io::Result<Vec<(DocId, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut current: Option<PageId> = Some(self.root);
        while let Some(pid) = current {
            let (records, next) = bp.with_page(pid, |page| {
                let mut recs: Vec<(DocId, Vec<u8>)> = Vec::new();
                let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET);
                for s in 0..slot_count {
                    if let Some(bytes) = read_slot(page, s) {
                        recs.push(((pid, s), bytes));
                    }
                }
                let next = read_u64(&page.data, HEADER_NEXT_OFFSET);
                (recs, next)
            })?;
            out.extend(records);
            current = if next == NO_NEXT { None } else { Some(next) };
        }
        Ok(out)
    }

    /*
    flush: push all dirty frames to disk. call before drop if you want
    explicit error handling (Drop in BufferPool swallows errors).
    */
    pub fn flush(&self, bp: &mut BufferPool) -> std::io::Result<()> {
        bp.flush_all()
    }
}

/*
================================================================
free helpers below. these don't touch self, just operate on raw page
bytes, which is why they're free functions instead of methods.

keeping these out of impl HeapFile lets us call them inside the closures
we pass to bp.with_page* without borrow-checker drama.
================================================================
*/

/*
find_last_page: walk the chain from `root` until next == NO_NEXT,
return that page id. called once on open.
*/
fn find_last_page(bp: &mut BufferPool, root: PageId) -> std::io::Result<PageId> {
    let mut current = root;
    loop {
        let next = bp.with_page(current, |page| read_u64(&page.data, HEADER_NEXT_OFFSET))?;
        if next == NO_NEXT {
            return Ok(current);
        }
        current = next;
    }
}

/*
init_heap_page: write the empty heap page header.
called on every freshly-allocated page in a heap file chain.
*/
fn init_heap_page(page: &mut Page) {
    write_u64(&mut page.data, HEADER_NEXT_OFFSET, NO_NEXT);
    write_u16(&mut page.data, HEADER_SLOT_COUNT_OFFSET, 0);
    write_u16(&mut page.data, HEADER_FREE_END_OFFSET, PAGE_SIZE as u16);
}

/*
page_free_space: bytes available between the slot array (growing up) and
the record area (growing down).

if the two have crossed (shouldn't happen with correct accounting),
saturating_sub returns 0 instead of underflowing. that's a defensive belt
on top of the assert in try_insert_in_page.
*/
fn page_free_space(page: &Page) -> usize {
    let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET) as usize;
    let free_end = read_u16(&page.data, HEADER_FREE_END_OFFSET) as usize;
    let slot_array_end = HEADER_SIZE + slot_count * SLOT_SIZE;
    free_end.saturating_sub(slot_array_end)
}

/*
try_insert_in_page: append a new record + slot, return the new slot id.
returns None if the record + slot won't fit.

the slot id is just the index of the slot in the slot array, which is
the slot_count BEFORE we incremented it.
*/
fn try_insert_in_page(page: &mut Page, data: &[u8]) -> Option<SlotId> {
    let needed = data.len() + SLOT_SIZE;
    if page_free_space(page) < needed {
        return None;
    }
    let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET);
    let free_end = read_u16(&page.data, HEADER_FREE_END_OFFSET) as usize;
    let new_offset = free_end - data.len();

    // write the record bytes
    page.data[new_offset..new_offset + data.len()].copy_from_slice(data);

    // write the slot entry
    let slot_pos = HEADER_SIZE + slot_count as usize * SLOT_SIZE;
    write_u16(&mut page.data, slot_pos, new_offset as u16);
    write_u16(&mut page.data, slot_pos + 2, data.len() as u16);

    // bump header counters
    write_u16(&mut page.data, HEADER_SLOT_COUNT_OFFSET, slot_count + 1);
    write_u16(&mut page.data, HEADER_FREE_END_OFFSET, new_offset as u16);

    Some(slot_count)
}

/*
read_slot: pull the record bytes for `slot_id`, or None if tombstoned
or out of range.
*/
fn read_slot(page: &Page, slot_id: SlotId) -> Option<Vec<u8>> {
    let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET);
    if slot_id >= slot_count {
        return None;
    }
    let slot_pos = HEADER_SIZE + slot_id as usize * SLOT_SIZE;
    let offset = read_u16(&page.data, slot_pos) as usize;
    let length = read_u16(&page.data, slot_pos + 2) as usize;
    if length == 0 {
        return None; // tombstone
    }
    Some(page.data[offset..offset + length].to_vec())
}

/*
tombstone_slot: mark a slot as deleted by zeroing its length.
silently no-ops on out-of-range or already-tombstoned slots.
*/
fn tombstone_slot(page: &mut Page, slot_id: SlotId) {
    let slot_count = read_u16(&page.data, HEADER_SLOT_COUNT_OFFSET);
    if slot_id >= slot_count {
        return;
    }
    let slot_pos = HEADER_SIZE + slot_id as usize * SLOT_SIZE;
    write_u16(&mut page.data, slot_pos + 2, 0);
}

/*
byte-level read / write helpers.

`u64::from_le_bytes` takes a [u8; 8] and reinterprets it as a little-endian
u64. we go through `try_into().unwrap()` to turn a `&[u8]` slice into a
fixed-size array reference. unwrap is safe because we always slice exactly
the right number of bytes.

little-endian is the convention pretty much every modern db uses because
x86 and arm are both little-endian, so on those platforms it's a no-op
load. on a big-endian machine we'd pay a byte swap, which is fine.
*/
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

fn write_u64(buf: &mut [u8], offset: usize, val: u64) {
    buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
}

fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

fn write_u16(buf: &mut [u8], offset: usize, val: u16) {
    buf[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    use tempfile::NamedTempFile;

    /*
    Returns the pool alongside the heap now that HeapFile does not own one. The
    NamedTempFile must stay bound in the caller too: dropping it deletes the
    file out from under the pool.
    */
    fn make_heap(capacity: usize) -> (HeapFile, BufferPool, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let disk = DiskManager::open(tmp.path()).unwrap();
        let mut bp = BufferPool::new(disk, capacity);
        let heap = HeapFile::create(&mut bp).unwrap();
        (heap, bp, tmp)
    }

    #[test]
    fn test_insert_read_one() {
        let (mut heap, mut bp, _tmp) = make_heap(4);
        let id = heap.insert(&mut bp, b"hello world").unwrap();
        let got = heap.read(&mut bp, id).unwrap();
        assert_eq!(got.as_deref(), Some(&b"hello world"[..]));
    }

    #[test]
    #[should_panic(expected = "empty records are not supported")]
    fn test_insert_empty_record_panics() {
        let (mut heap, mut bp, _tmp) = make_heap(4);
        heap.insert(&mut bp, b"").unwrap();
    }

    #[test]
    fn test_insert_many_single_page() {
        let (mut heap, mut bp, _tmp) = make_heap(4);
        let mut ids = Vec::new();
        for i in 0u32..50 {
            let payload = format!("record-{}", i);
            ids.push((heap.insert(&mut bp, payload.as_bytes()).unwrap(), payload));
        }
        for (id, expected) in &ids {
            let got = heap.read(&mut bp, *id).unwrap().unwrap();
            assert_eq!(&got[..], expected.as_bytes());
        }
    }

    /*
    pump enough data to fill multiple pages and confirm the chain works:
    every record we inserted comes back from scan_all.
    */
    #[test]
    fn test_multi_page_chain() {
        let (mut heap, mut bp, _tmp) = make_heap(4);
        // ~500 bytes each, 1000 records -> well past one page
        let payload = vec![0xCDu8; 500];
        let mut ids = Vec::new();
        for _ in 0..1000 {
            ids.push(heap.insert(&mut bp, &payload).unwrap());
        }
        let all = heap.scan_all(&mut bp).unwrap();
        assert_eq!(all.len(), 1000);
        // every record should equal `payload`
        for (_, bytes) in &all {
            assert_eq!(bytes, &payload);
        }
    }

    #[test]
    fn test_delete_tombstones() {
        let (mut heap, mut bp, _tmp) = make_heap(4);
        let a = heap.insert(&mut bp, b"alpha").unwrap();
        let b = heap.insert(&mut bp, b"bravo").unwrap();
        let c = heap.insert(&mut bp, b"charlie").unwrap();

        heap.delete(&mut bp, b).unwrap();

        assert_eq!(heap.read(&mut bp, a).unwrap().as_deref(), Some(&b"alpha"[..]));
        assert_eq!(heap.read(&mut bp, b).unwrap(), None);
        assert_eq!(heap.read(&mut bp, c).unwrap().as_deref(), Some(&b"charlie"[..]));

        let all = heap.scan_all(&mut bp).unwrap();
        assert_eq!(all.len(), 2);
    }

    /*
    persistence smoke test: insert, flush, drop the heap, re-open from the
    same disk file and same root id, scan, confirm same records.

    this is the phase 1 milestone from plans.txt:
      "Insert a raw byte blob, retrieve it by (PageId, SlotId),
       restart the process, retrieve it again. Identical bytes."
    */
    #[test]
    fn test_persistence_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();

        // session 1: create heap, insert, flush, drop
        let root;
        let saved_ids;
        {
            let disk = DiskManager::open(tmp.path()).unwrap();
            let mut bp = BufferPool::new(disk, 4);
            let mut heap = HeapFile::create(&mut bp).unwrap();
            root = heap.root();
            saved_ids = vec![
                heap.insert(&mut bp, b"persist me").unwrap(),
                heap.insert(&mut bp, b"and me too").unwrap(),
            ];
            heap.flush(&mut bp).unwrap();
        }

        // session 2: reopen same file, same root
        let disk = DiskManager::open(tmp.path()).unwrap();
        let mut bp = BufferPool::new(disk, 4);
        let mut heap = HeapFile::open(&mut bp, root).unwrap();
        assert_eq!(heap.read(&mut bp, saved_ids[0]).unwrap().as_deref(), Some(&b"persist me"[..]));
        assert_eq!(heap.read(&mut bp, saved_ids[1]).unwrap().as_deref(), Some(&b"and me too"[..]));

        // a fresh insert after reopen should land on the recovered tail page
        // and be readable like any other record.
        let new_id = heap.insert(&mut bp, b"post-reopen").unwrap();
        assert_eq!(heap.read(&mut bp, new_id).unwrap().as_deref(), Some(&b"post-reopen"[..]));
    }

    /*
    plans.txt phase 1 unit test target: lots of variable-length docs,
    scan returns all, delete some, scan reflects deletions.
    we scale down from 100k to keep the unit test fast; the algorithm
    is identical at any N.
    */
    #[test]
    fn test_bulk_insert_delete_scan() {
        let (mut heap, mut bp, _tmp) = make_heap(8);
        let mut ids = Vec::new();
        for i in 0u32..2000 {
            // variable length 1..=200 bytes, content varies with i
            let len = 1 + (i as usize % 200);
            let payload: Vec<u8> = (0..len).map(|j| ((i + j as u32) & 0xff) as u8).collect();
            ids.push((heap.insert(&mut bp, &payload).unwrap(), payload));
        }
        // delete every 7th record
        for (idx, (id, _)) in ids.iter().enumerate() {
            if idx % 7 == 0 {
                heap.delete(&mut bp, *id).unwrap();
            }
        }
        let alive = heap.scan_all(&mut bp).unwrap();
        let expected_alive = ids.iter().enumerate().filter(|(i, _)| i % 7 != 0).count();
        assert_eq!(alive.len(), expected_alive);
    }

    #[test]
    #[ignore = "phase 1 stress test: inserts 100k variable-length records"]
    fn stress_insert_100k_variable_records_delete_subset_scan() {
        let (mut heap, mut bp, _tmp) = make_heap(128);
        let mut ids = Vec::with_capacity(100_000);

        for i in 0u32..100_000 {
            let payload = stress_payload(i);
            let id = heap.insert(&mut bp, &payload).unwrap();
            ids.push((id, payload));
        }

        for (idx, (id, _)) in ids.iter().enumerate() {
            if idx % 7 == 0 {
                heap.delete(&mut bp, *id).unwrap();
            }
        }

        let alive = heap.scan_all(&mut bp).unwrap();
        let expected_alive = ids.iter().enumerate().filter(|(i, _)| i % 7 != 0).count();
        assert_eq!(alive.len(), expected_alive);

        for (idx, (id, expected)) in ids.iter().enumerate().step_by(997) {
            let got = heap.read(&mut bp, *id).unwrap();
            if idx % 7 == 0 {
                assert_eq!(got, None);
            } else {
                assert_eq!(got.as_deref(), Some(expected.as_slice()));
            }
        }
    }

    fn stress_payload(i: u32) -> Vec<u8> {
        let len = 1 + (i as usize % 200);
        (0..len).map(|j| ((i.wrapping_mul(31) + j as u32) & 0xff) as u8).collect()
    }
}
