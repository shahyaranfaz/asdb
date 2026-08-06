mod buffer_pool;
mod disk;
mod heap;
mod page;

pub use buffer_pool::BufferPool;
pub use disk::DiskManager;
pub use heap::{DocId, HeapFile, MAX_RECORD_SIZE, SlotId};
pub use page::{Page, PageId, PAGE_SIZE};

pub type SharedBufferPool = std::sync::Arc<std::sync::Mutex<BufferPool>>;
