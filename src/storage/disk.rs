use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::storage::page::{Page, PageId, PAGE_SIZE};

pub struct DiskManager {
    file: File,
    num_pages: u64,
}

impl DiskManager {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let num_pages = file.metadata()?.len() / PAGE_SIZE as u64;

        Ok(DiskManager { file, num_pages })
    }

    pub fn read_page(&mut self, page_id: PageId) -> std::io::Result<Page> {
        assert!(page_id < self.num_pages, "page_id out of range");
        let offset = page_id * PAGE_SIZE as u64;
        let mut page = Page::new();
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut page.data)?;
        Ok(page)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> std::io::Result<()> {
        assert!(page_id < self.num_pages, "page_id out of range");
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn allocate_page(&mut self) -> std::io::Result<PageId> {
        let page_id = self.num_pages;
        let empty = Page::new();
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&empty.data)?;
        self.file.flush()?;
        self.num_pages += 1;
        Ok(page_id)
    }

    pub fn num_pages(&self) -> u64 {
        self.num_pages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    //use std::path::PathBuf;
    use tempfile::NamedTempFile;

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
}
