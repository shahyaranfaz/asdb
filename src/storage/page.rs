pub const PAGE_SIZE: usize = 4096;
pub type PageId = u64;

pub struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new() -> Self {
        Page {
            data: [0u8; PAGE_SIZE],
        }
    }

    pub fn zero(&mut self) {
        self.data = [0u8; PAGE_SIZE];
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}
