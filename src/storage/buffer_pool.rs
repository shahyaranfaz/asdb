/*
buffer_pool.rs

the buffer pool sits between the heap / btree code and the DiskManager. its job
is to keep recently-used pages in memory so we don't hit the disk on every
access, and to write dirty pages back to disk when we evict them.

high-level model:
  - we own N "frames" in memory, each frame is one Page-sized slot
  - a page_table (HashMap) tells us which frame currently holds which page id
  - on fetch: if cached, hand it over. else evict an old frame and load the
    page from disk into it.
  - dirty bit tracks "this frame has changes the disk doesn't know about yet"
  - pin count tracks "this frame is in use right now, do not evict it"
  - LRU counter (`last_used`) drives which unpinned frame gets evicted next
*/

use std::collections::HashMap;
use crate::storage::disk::DiskManager;
use crate::storage::page::{Page, PageId};

/*
Frame : one slot in the buffer pool.

page_id is Option<PageId> because a frame can be empty (no page loaded yet).
this matters when the pool is first created or after we explicitly invalidate.
in c++ you'd use a sentinel value like -1; rust's Option is the safer version.
*/
struct Frame {
    page: Page,
    page_id: Option<PageId>,
    is_dirty: bool,
    pin_count: u32,
    last_used: u64,
}

impl Frame {
    fn empty() -> Self {
        Frame {
            page: Page::new(),
            page_id: None,
            is_dirty: false,
            pin_count: 0,
            last_used: 0,
        }
    }
}

/*
BufferPool : owns the DiskManager and a fixed Vec of frames.

note we OWN the DiskManager. this is a phase 1 simplification. later, multiple
heap files / btrees will share one buffer pool, and the buffer pool will still
own the disk manager. only the buffer pool talks to the disk manager directly,
everyone else goes through the pool.

`clock` is a monotonically increasing counter we bump every time a page is
touched. the frame with the smallest `last_used` value is the least recently
used, hence LRU. cheap to maintain, no need for a real LRU queue.
*/
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: HashMap<PageId, usize>,
    disk: DiskManager,
    clock: u64,
}

impl BufferPool {
    /*
    new : build a pool of `capacity` frames around a DiskManager.

    `(0..capacity).map(|_| Frame::empty()).collect()` is the iterator way to
    build a Vec of N things. you could also write a for loop with push, but the
    iterator chain is more idiomatic and the compiler usually optimizes both
    to the same code.

    the |_| is a closure that ignores its argument. we don't care about the
    range index, we just want to call Frame::empty() N times.
    */
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        assert!(capacity > 0, "buffer pool capacity must be > 0");
        BufferPool {
            frames: (0..capacity).map(|_| Frame::empty()).collect(),
            page_table: HashMap::new(),
            disk,
            clock: 0,
        }
    }

    /*
    with_page : pin a page, run the closure with a read-only view, unpin.

    this is the api callers use to read a page through the buffer pool. why a
    closure instead of returning `&Page`? because if we returned a reference,
    callers could hold it across other buffer pool operations, and the borrow
    checker would either reject them or we'd have to use unsafe / RefCell.

    by handing the page to a closure, the borrow is scoped to the closure body
    automatically. the closure runs, returns whatever it computed, and we
    safely unpin. no lifetimes leak out.

    `FnOnce(&Page) -> R` means: takes &Page, returns some R, only callable
    once. FnOnce is the most permissive bound (Fn and FnMut are stricter). use
    FnOnce when you only call the closure one time, which is our case.

    R is a generic, so the caller can return ANY type out of the closure.
    rust monomorphizes this, so each callsite gets its own specialized copy
    at compile time. zero-cost.

    we manually pin (pin_count += 1) and unpin (pin_count -= 1). in this v1 the
    pin is redundant because the closure runs synchronously and nobody else can
    touch the pool while we hold &mut self. but the bookkeeping is correct for
    when we add a concurrent api later.
    */
    pub fn with_page<F, R>(&mut self, page_id: PageId, f: F) -> std::io::Result<R>
    where
        F: FnOnce(&Page) -> R,
    {
        let idx = self.fetch_or_load(page_id)?;
        self.frames[idx].pin_count += 1;
        let result = f(&self.frames[idx].page);
        self.frames[idx].pin_count -= 1;
        self.touch(idx);
        Ok(result)
    }

    /*
    with_page_mut : same as with_page but hands a &mut Page and marks dirty.

    we mark dirty unconditionally even if the closure didn't actually mutate.
    that means we might re-flush an unchanged page (wasted disk io) but never
    lose a real mutation. that's the safe direction to err.

    a fancier api would have the closure return (R, bool) where the bool says
    "i actually changed something". skipped for simplicity.
    */
    pub fn with_page_mut<F, R>(&mut self, page_id: PageId, f: F) -> std::io::Result<R>
    where
        F: FnOnce(&mut Page) -> R,
    {
        let idx = self.fetch_or_load(page_id)?;
        self.frames[idx].pin_count += 1;
        let result = f(&mut self.frames[idx].page);
        self.frames[idx].is_dirty = true;
        self.frames[idx].pin_count -= 1;
        self.touch(idx);
        Ok(result)
    }

    /*
    new_page : allocate a fresh page on disk, load an empty frame for it, run
    the closure to initialize it, return (page_id, R).

    the closure can fill in headers, write initial data, whatever. the page is
    pre-zeroed by Page::new() inside allocate_frame.
    */
    pub fn new_page<F, R>(&mut self, f: F) -> std::io::Result<(PageId, R)>
    where
        F: FnOnce(&mut Page) -> R,
    {
        let page_id = self.disk.allocate_page()?;
        let idx = self.allocate_frame(page_id)?;
        // ensure the in-memory frame is a clean slate even if it was reused
        self.frames[idx].page.zero();
        self.frames[idx].is_dirty = true;
        self.frames[idx].pin_count += 1;
        let result = f(&mut self.frames[idx].page);
        self.frames[idx].pin_count -= 1;
        self.touch(idx);
        Ok((page_id, result))
    }

    /*
    delete_page : drop a page from the pool and free it on disk.

    asserts no one is currently using it (pin_count == 0). if you panic here,
    you have a bug: you're freeing a page someone else is reading.
    */
    pub fn delete_page(&mut self, page_id: PageId) -> std::io::Result<()> {
        if let Some(&idx) = self.page_table.get(&page_id) {
            assert!(self.frames[idx].pin_count == 0, "deleting a pinned page");
            // a dirty page we're about to free doesn't need to be flushed
            self.frames[idx].page_id = None;
            self.frames[idx].is_dirty = false;
            self.frames[idx].last_used = 0;
            self.page_table.remove(&page_id);
        }
        self.disk.free_page(page_id)
    }

    /*
    flush_all : write every dirty frame back to disk.

    call this before shutdown (or before a checkpoint, when we add those). a
    crash between flush_all calls would lose any dirty pages that hadn't been
    evicted, which is the whole reason real dbs have a write-ahead log. out of
    scope for v1 (see plans.txt OUT OF SCOPE).

    `0..self.frames.len()` is a Range. we iterate by index instead of by
    `.iter_mut()` because we need to call `self.disk.write_page` inside the
    loop body, and that needs `&mut self.disk`. iterating with `.iter_mut()`
    would hold a borrow on `self.frames` and the borrow checker would yell.
    indices sidestep that.
    */
    pub fn flush_all(&mut self) -> std::io::Result<()> {
        for i in 0..self.frames.len() {
            if let Some(pid) = self.frames[i].page_id {
                if self.frames[i].is_dirty {
                    self.disk.write_page(pid, &self.frames[i].page)?;
                    self.frames[i].is_dirty = false;
                }
            }
        }
        Ok(())
    }

    /*
    capacity : how many frames the pool has total.
    used by tests, mostly. trivial getter.
    */
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    // ----- internals below -----

    /*
    fetch_or_load : returns the index of the frame that holds page_id, loading
    it from disk if it isn't already cached.

    note we don't pin here. pinning happens in the with_page methods. this
    keeps fetch_or_load simple and lets callers control pin lifecycles.
    */
    fn fetch_or_load(&mut self, page_id: PageId) -> std::io::Result<usize> {
        if let Some(&idx) = self.page_table.get(&page_id) {
            return Ok(idx);
        }
        let idx = self.allocate_frame(page_id)?;
        self.frames[idx].page = self.disk.read_page(page_id)?;
        Ok(idx)
    }

    /*
    allocate_frame : find an empty frame or evict the LRU unpinned frame, then
    register the new page_id in the page table.

    two passes:
      pass 1, look for an unused frame (page_id == None)
      pass 2, find the unpinned frame with the smallest last_used counter
              and evict it (flushing if dirty)

    if every frame is pinned this panics. for v1 with single-threaded access,
    pin_count rises and falls inside one function call, so we should never hit
    the panic in practice. it's a backstop.
    */
    fn allocate_frame(&mut self, page_id: PageId) -> std::io::Result<usize> {
        // pass 1: prefer an actually-empty frame
        for i in 0..self.frames.len() {
            if self.frames[i].page_id.is_none() {
                self.frames[i].page_id = Some(page_id);
                self.frames[i].is_dirty = false;
                self.frames[i].pin_count = 0;
                self.page_table.insert(page_id, i);
                return Ok(i);
            }
        }

        // pass 2: LRU eviction over unpinned frames
        let mut victim: Option<usize> = None;
        let mut victim_time = u64::MAX;
        for i in 0..self.frames.len() {
            let f = &self.frames[i];
            if f.pin_count == 0 && f.last_used < victim_time {
                victim_time = f.last_used;
                victim = Some(i);
            }
        }
        let idx = victim.expect("all frames pinned, cannot evict");

        let old_pid = self.frames[idx].page_id.expect("victim has no page id");
        if self.frames[idx].is_dirty {
            // clone-then-write would be cleaner against the borrow checker,
            // but the disk write only needs &Page so a sub-borrow works.
            self.disk.write_page(old_pid, &self.frames[idx].page)?;
        }
        self.page_table.remove(&old_pid);

        // re-key the frame
        self.frames[idx].page_id = Some(page_id);
        self.frames[idx].is_dirty = false;
        self.frames[idx].pin_count = 0;
        self.page_table.insert(page_id, idx);
        Ok(idx)
    }

    /*
    touch : bump the LRU counter on a frame.

    the clock just keeps incrementing. with u64 it overflows after 18
    quintillion accesses, which is "never" in practice. if we ever ran that
    long, we'd want to renumber timestamps to compress the range.
    */
    fn touch(&mut self, frame_idx: usize) {
        self.clock += 1;
        self.frames[frame_idx].last_used = self.clock;
    }
}

/*
Drop : when a BufferPool goes out of scope, flush any dirty pages.

implementing the Drop trait is rust's version of a destructor, runs
automatically when the value is dropped. perfect place to make sure no dirty
in-memory pages get lost.

we ignore the io error because Drop can't return one. a real db would log or
panic; v1 just swallows it. if you care, call flush_all() yourself before
dropping.
*/
impl Drop for BufferPool {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn make_pool(capacity: usize) -> (BufferPool, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let disk = DiskManager::open(tmp.path()).unwrap();
        (BufferPool::new(disk, capacity), tmp)
    }

    /*
    sanity: allocate via new_page, then fetch it back, get the same bytes.
    */
    #[test]
    fn test_new_and_fetch() {
        let (mut bp, _tmp) = make_pool(4);
        let (pid, _) = bp
            .new_page(|page| {
                page.data[0] = 7;
                page.data[100] = 77;
            })
            .unwrap();
        let val = bp.with_page(pid, |page| (page.data[0], page.data[100])).unwrap();
        assert_eq!(val, (7, 77));
    }

    /*
    fill the pool past capacity and confirm earlier pages get evicted but
    re-fetching them works (because they get re-loaded from disk).
    */
    #[test]
    fn test_eviction_round_trip() {
        let (mut bp, _tmp) = make_pool(2);

        let mut ids = vec![];
        for i in 0..5u8 {
            let (pid, _) = bp
                .new_page(|page| {
                    page.data[0] = i;
                })
                .unwrap();
            ids.push(pid);
        }
        // pool holds 2 frames but we just touched 5 pages, so earlier ones got
        // evicted and (since they were dirty) flushed to disk.
        for (i, pid) in ids.iter().enumerate() {
            let byte = bp.with_page(*pid, |page| page.data[0]).unwrap();
            assert_eq!(byte, i as u8);
        }
    }

    /*
    free + reuse round-trip through the buffer pool.
    delete_page should drop the cache entry AND call disk.free_page.
    */
    #[test]
    fn test_delete_page() {
        let (mut bp, _tmp) = make_pool(2);
        let (pid, _) = bp.new_page(|page| { page.data[0] = 1; }).unwrap();
        bp.delete_page(pid).unwrap();
        // allocating again should reuse pid (free list is LIFO)
        let (pid2, _) = bp.new_page(|page| { page.data[0] = 2; }).unwrap();
        assert_eq!(pid, pid2);
        let v = bp.with_page(pid2, |p| p.data[0]).unwrap();
        assert_eq!(v, 2);
    }
}
