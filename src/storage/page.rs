// constants
pub const PAGE_SIZE: usize = 4096;
// rather then making a struct we alias
pub type PageId = u64;

pub struct Page {
    pub data: [u8; PAGE_SIZE],
}
/* 
impl page: methods to add to struct
*/
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
/* 
default constructor - why seperate from impl Page?

Unlike C++ or Java, Rust has no special constructor keyword. new() is just a 
convention, i.e. a regular function that happens to be named new. The language 
doesn't treat it specially at all.

This is what the standard library defines:

pub trait Default {
    fn default() -> Self;
}

When you impl Default for Page, you're saying "my type plays by this contract", 
which unlocks integrations across the standard library and ecosystem.

tldr impl default for page and u define the contract or use the std lib one above
*/
impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}
