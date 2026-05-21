/*
imports / use statements

`use` is like `import` in python or `#include` in c++, but only pulls names into
scope. nothing is "imported" at runtime, the compiler already knows what's in
std. these lines just save us from typing `std::fs::File` everywhere.

note we import Read, Seek, Write even though we never write `Read::read_exact(...)`.
these are TRAITS. in rust, a method that lives on a trait (like file.read_exact)
is only callable when that trait is in scope. so importing them is what turns on
the dot-method syntax we use below.
*/

// `crate::` means "from the root of this crate". this is how we reach into our own modules.
use super::{Page, PageId, PAGE_SIZE};

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

/*
DiskManager: owns one file on disk and tracks how many pages live in it.

the database is one big file. every PAGE_SIZE (4096) bytes is one page. page_id
is just the index of the page (page 0 is bytes 0..4096, page 1 is 4096..8192).
no fancy file-per-table layout, one flat file.

free_list is an in-memory stack of page ids that have been freed and can be
recycled by allocate_page. it is NOT persisted to disk yet (real dbs keep the
free list on a special page). on restart, freed pages just look like wasted
space until we add a persistent free list later.

why Vec<PageId> and not a HashSet? we want LIFO behavior, recently-freed pages
get reused first so they're more likely to still be hot in the buffer pool.
*/
pub struct DiskManager {
    file: File,
    num_pages: u64,
    free_list: Vec<PageId>,
}

impl DiskManager {
    /*
    open: open or create the db file.

    std::io::Result<Self> is sugar for Result<Self, std::io::Error>. rust has no
    exceptions, errors are values. every fallible call returns Result and you
    either handle it or propagate it with `?`.

    OpenOptions is the builder pattern. instead of one open() with 12 boolean
    args, you chain the options you want. .create(true) means "create if it
    doesn't exist, otherwise open the existing one".

    the `?` after .open(path) does this: if Err, return Err from THIS function
    immediately. if Ok, unwrap the value and keep going. it's sugar for a match.

    metadata()?.len() shows ? chaining. metadata() returns Result, ? unwraps it
    or bails, .len() runs on the unwrapped value.

    PAGE_SIZE is usize so we cast it `as u64` to divide a u64 cleanly. rust will
    NOT silently convert between integer types, you have to ask.
    */
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let file_len = file.metadata()?.len();
        if file_len % PAGE_SIZE as u64 != 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "database file length is not page-aligned",
            ));
        }
        let num_pages = file_len / PAGE_SIZE as u64;

        // free_list starts empty. note: this means freed pages from previous
        // sessions are forgotten and effectively leaked. that's a v1 trade-off.
        Ok(DiskManager { file, num_pages, free_list: Vec::new() })
    }

    /*
    read_page: pull one page off disk by its id.

    why &mut self and not &self? because seeking moves the file cursor, which is
    state inside the File. any method that mutates the receiver needs &mut. the
    borrow checker uses this to make sure no one else is reading the same file
    handle at the same time.

    assert! is a macro (the ! gives it away). it panics if the condition is
    false. we use it here for a programmer bug, not a user error. a bad page_id
    means our code is wrong, not that the user did something dumb.

    SeekFrom::Start(offset) is an enum variant. Seek is the trait, SeekFrom is
    the enum it takes. other variants are End and Current. enums in rust carry
    data, so each variant is like its own tagged struct.

    read_exact fills the buffer completely or errors. plain `read` might give
    you fewer bytes than you asked for (think: network sockets, partial reads).
    for a known-size page we want all-or-nothing.
    */
    pub fn read_page(&mut self, page_id: PageId) -> std::io::Result<Page> {
        assert!(page_id < self.num_pages, "page_id out of range");
        let offset = page_id * PAGE_SIZE as u64;
        let mut page = Page::new();
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut page.data)?;
        Ok(page)
    }

    /*
    write_page: overwrite one page on disk.

    `page: &Page` is a shared (immutable) borrow. we don't need to mutate the
    page, we just need to read its bytes. compare with &mut self above. one
    function can take a mutable borrow of self AND an immutable borrow of
    something else at the same time, rust is fine with that.

    flush() forces the OS to actually push pending writes. without it, your
    write might sit in a kernel buffer and vanish on crash. this is a HUGE
    topic in databases (durability, fsync, write-ahead logs). we're keeping it
    simple, flush after every write. slow but safe.

    Result<()> means "ok with no value" or "err with std::io::Error". () is
    rust's unit type, like void but it IS a real value of size 0.
    */
    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> std::io::Result<()> {
        assert!(page_id < self.num_pages, "page_id out of range");
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.flush()?;
        Ok(())
    }

    /*
    allocate_page: reuse a freed page if any, else append a fresh zeroed one.

    flow:
      1. if free_list has an id, pop it, zero the page on disk, hand it back.
      2. else: id = current count, write zeros at the end, bump the count.

    `if let Some(id) = ...` is rust's pattern-match-and-bind. Vec::pop returns
    Option<T>, Some(x) when non-empty, None when empty. this one liner handles
    both cases in one shot, which is why iterators and Option are so nice in
    rust.

    we write the empty page explicitly instead of just calling set_len, because
    on most filesystems set_len creates a sparse file (hole) which reads back
    as zeros but doesn't actually allocate disk blocks until a write hits them.
    writing the zeros up front means the disk space is real.
    */
    pub fn allocate_page(&mut self) -> std::io::Result<PageId> {
        // try the free list first so freed pages get reused.
        if let Some(page_id) = self.free_list.pop() {
            let empty = Page::new();
            let offset = page_id * PAGE_SIZE as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&empty.data)?;
            self.file.flush()?;
            return Ok(page_id);
        }
        let page_id = self.num_pages;
        let empty = Page::new();
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&empty.data)?;
        self.file.flush()?;
        self.num_pages += 1;
        Ok(page_id)
    }

    /*
    free_page: mark a page as recyclable.

    we don't touch the bytes on disk, we just remember the id. the next
    allocate_page call will pop it back out and zero it then.

    note this can grow without bound (in memory) if you free a lot and never
    allocate. for v1 that's fine. a real db would have a persistent on-disk
    free list, usually a linked list of free pages where each free page itself
    stores the id of the next free page.
    */
    pub fn free_page(&mut self, page_id: PageId) -> std::io::Result<()> {
        assert!(page_id < self.num_pages, "page_id out of range");
        if self.free_list.contains(&page_id) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "page is already free",
            ));
        }
        self.free_list.push(page_id);
        Ok(())
    }

    /*
    num_pages: read-only accessor.

    &self (no mut) because we're only reading. returning u64 by value is fine,
    integers are Copy so this is just a register move, no borrow drama.

    in c++ this would be a const member function. rust says the same thing with
    &self vs &mut self.
    */
    pub fn num_pages(&self) -> u64 {
        self.num_pages
    }
}

/*
#[cfg(test)]: conditional compilation attribute.

this whole module only gets compiled when you run `cargo test`. in release
builds it doesn't exist, so test code adds zero bytes to your binary. this is
a really nice pattern, tests sit next to the code they test.

`mod tests { use super::*; }`: tests is a child module. `super` means "one
module up", so super::* pulls in everything from disk.rs into the test scope.
that's how the tests get access to DiskManager without re-importing it.
*/
#[cfg(test)]
mod tests {
    use super::*;
    //use std::path::PathBuf;
    // tempfile is a dev-dependency in Cargo.toml. NamedTempFile makes a file
    // that gets deleted when it goes out of scope. perfect for tests that need
    // a real path on disk without leaving litter behind.
    use tempfile::NamedTempFile;

    /*
    #[test]: marks this fn as a test case. cargo test discovers all #[test]
    functions and runs each in its own thread.

    .unwrap() is "give me the value or panic". in real code we'd use ? or match,
    but in tests panicking IS the failure signal, so unwrap is idiomatic.

    assert_eq!(a, b) is the test version of assert!, it prints both values when
    they differ so you can see what was wrong.
    */
    #[test]
    fn test_write_read_page() {
        let tmp = NamedTempFile::new().unwrap();
        let mut dm = DiskManager::open(tmp.path()).unwrap();
        let page_id = dm.allocate_page().unwrap();

        let mut page = Page::new();
        page.data[0] = 42;
        page.data[4095] = 99;
        dm.write_page(page_id, &page).unwrap();

        let read_back = dm.read_page(page_id).unwrap();
        assert_eq!(read_back.data[0], 42);
        assert_eq!(read_back.data[4095], 99);
    }

    /*
    free_page should hand the same id back on the next allocate_page,
    and the recycled page should come back zeroed, not with whatever
    junk the previous owner wrote.
    */
    #[test]
    fn test_free_and_reuse() {
        let tmp = NamedTempFile::new().unwrap();
        let mut dm = DiskManager::open(tmp.path()).unwrap();

        let p0 = dm.allocate_page().unwrap();
        let p1 = dm.allocate_page().unwrap();
        assert_eq!(p0, 0);
        assert_eq!(p1, 1);

        // dirty up p0 so we can tell whether reuse zeroed it
        let mut dirty = Page::new();
        dirty.data[0] = 0xAB;
        dm.write_page(p0, &dirty).unwrap();

        dm.free_page(p0).unwrap();

        // next allocate should hand back p0 (LIFO), not p2
        let reused = dm.allocate_page().unwrap();
        assert_eq!(reused, p0);

        let fresh = dm.read_page(reused).unwrap();
        assert_eq!(fresh.data[0], 0, "reused page should be zeroed");
    }

    #[test]
    fn test_rejects_partial_page_file() {
        let tmp = NamedTempFile::new().unwrap();
        tmp.as_file().set_len(PAGE_SIZE as u64 + 1).unwrap();

        let err = match DiskManager::open(tmp.path()) {
            Ok(_) => panic!("expected partial page file to fail"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_double_free_is_error() {
        let tmp = NamedTempFile::new().unwrap();
        let mut dm = DiskManager::open(tmp.path()).unwrap();
        let page_id = dm.allocate_page().unwrap();

        dm.free_page(page_id).unwrap();
        let err = dm.free_page(page_id).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    #[ignore = "phase 1 stress test: writes and verifies 10k pages"]
    fn stress_allocate_10k_pages_write_read_each() {
        let tmp = NamedTempFile::new().unwrap();
        let mut dm = DiskManager::open(tmp.path()).unwrap();
        let mut ids = Vec::with_capacity(10_000);

        for i in 0..10_000u64 {
            let page_id = dm.allocate_page().unwrap();
            assert_eq!(page_id, i);

            let mut page = Page::new();
            page.data[0..8].copy_from_slice(&i.to_le_bytes());
            page.data[PAGE_SIZE - 8..PAGE_SIZE].copy_from_slice(&(!i).to_le_bytes());
            page.data[123] = (i & 0xff) as u8;
            dm.write_page(page_id, &page).unwrap();
            ids.push(page_id);
        }

        assert_eq!(dm.num_pages(), 10_000);

        for (i, page_id) in ids.into_iter().enumerate() {
            let page = dm.read_page(page_id).unwrap();
            let i = i as u64;
            assert_eq!(u64::from_le_bytes(page.data[0..8].try_into().unwrap()), i);
            assert_eq!(
                u64::from_le_bytes(page.data[PAGE_SIZE - 8..PAGE_SIZE].try_into().unwrap()),
                !i
            );
            assert_eq!(page.data[123], (i & 0xff) as u8);
        }
    }
}
