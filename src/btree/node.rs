use crate::storage::{DocId, Page, PageId, SlotId, PAGE_SIZE};

// Leaf Nodes:
// [0]: 0 | [1-2]: get_key_count | [3-4]: free_offset | [5-12]: next_leaf | [13..]: slot array
// Leaf Node Slots:
// [0-1]: key_offset | [2-3]: key_len | [4-13]: DocId

// Internal Nodes:
// [0]: 1 | [1-2]: get_key_count | [3-4]: free_offset | [5-12]: leftmost_child | [13..]: slot array
// Internal Node Slots:
// [0-1]: key_offset | [2-3]: key_len | [4-11]: child PageId

const HEADER_SIZE: usize = 13;
pub const NO_NEXT_LEAF: PageId = u64::MAX;

pub struct LeafNode<'a> {
    page: &'a mut Page,
}

impl<'a> LeafNode<'a> {
    pub fn new(page: &'a mut Page) -> Self {
        LeafNode { page }
    }

    pub fn init(&mut self) {
        self.page.zero();
        let bytes: [u8; 2] = (PAGE_SIZE as u16).to_le_bytes();
        self.page.data[3..5].copy_from_slice(&bytes);
        self.set_next_leaf(NO_NEXT_LEAF);
    }

    pub fn get_doc_id(&self, i: usize) -> DocId {
        let slot_start: usize = self.get_slot_start(i) + 4;
        let page_id: PageId = u64::from_le_bytes(
            self.page.data[slot_start..slot_start+8].try_into().unwrap()
        );
        let slot_id: SlotId = u16::from_le_bytes(
            self.page.data[slot_start+8..slot_start+10].try_into().unwrap()
        );
        (page_id, slot_id)
    }

    pub fn insert_slot(&mut self, key: &[u8], doc_id: DocId) {
        let key_count: usize = self.get_key_count();
        let insert_index: usize = self.lower_bound_key_doc(key, doc_id);
        if insert_index < key_count
            && self.get_key(insert_index) == key
            && self.get_doc_id(insert_index) == doc_id {
            panic!("slot entry already exists");
        }

        // move all slots over
        let src_start: usize = self.get_slot_start(insert_index);
        let src_end: usize = self.get_slot_start(key_count);
        let dst_start: usize = self.get_slot_start(insert_index + 1);
        self.page.data.copy_within(src_start..src_end, dst_start);

        // insert key, update node values
        let free_offset: usize = self.set_key(key);
        self.set_free_offset(free_offset);
        self.set_key_count(key_count + 1);

        // insert slot
        let slot_start: usize = self.get_slot_start(insert_index);
        self.page.data[slot_start..slot_start +2].copy_from_slice(&(free_offset as u16).to_le_bytes());
        self.page.data[slot_start +2..slot_start +4].copy_from_slice(&(key.len() as u16).to_le_bytes());
        self.page.data[slot_start +4..slot_start +12].copy_from_slice(&doc_id.0.to_le_bytes());
        self.page.data[slot_start+12..slot_start+14].copy_from_slice(&doc_id.1.to_le_bytes());
    }

    pub fn get_next_leaf(&self) -> PageId {
        u64::from_le_bytes(self.page.data[5..13].try_into().unwrap())
    }

    pub fn set_next_leaf(&mut self, next_leaf: PageId) {
        let bytes: [u8; 8] = next_leaf.to_le_bytes();
        self.page.data[5..13].copy_from_slice(&bytes);
    }

    pub fn lower_bound_key(&self, key: &[u8]) -> usize {
        lower_bound(self.get_key_count(), |i| self.get_key(i) < key)
    }

    pub fn lower_bound_key_doc(&self, key: &[u8], doc_id: DocId) -> usize {
        lower_bound(self.get_key_count(), |i| {
            let mid_key = self.get_key(i);
            mid_key < key || (mid_key == key && self.get_doc_id(i) < doc_id)
        })
    }

    pub fn entries(&self) -> Vec<(Vec<u8>, DocId)> {
        let mut entries = Vec::with_capacity(self.get_key_count());
        for i in 0..self.get_key_count() {
            entries.push((self.get_key(i).to_vec(), self.get_doc_id(i)));
        }
        entries
    }

    pub fn rebuild(&mut self, entries: &[(Vec<u8>, DocId)], next_leaf: PageId) {
        self.init();
        self.set_next_leaf(next_leaf);
        for (key, doc_id) in entries {
            self.insert_slot(key, *doc_id);
        }
    }
}

pub struct InternalNode<'a> {
    page: &'a mut Page
}

impl<'a> InternalNode<'a> {
    pub fn new(page: &'a mut Page) -> Self {
        InternalNode { page }
    }

    pub fn init(&mut self, leftmost_child: PageId) {
        self.page.zero();
        self.page.data[0] = 1;
        let bytes: [u8; 2] = (PAGE_SIZE as u16).to_le_bytes();
        self.page.data[3..5].copy_from_slice(&bytes);

        let bytes: [u8; 8] = leftmost_child.to_le_bytes();
        self.page.data[5..13].copy_from_slice(&bytes);
    }

    pub fn get_child(&self, i: usize) -> PageId {
        let slot_start = self.get_slot_start(i) + 4;
        u64::from_le_bytes(
            self.page.data[slot_start..slot_start + 8].try_into().unwrap()
        )
    }

    pub fn get_leftmost_child(&self) -> PageId {
        u64::from_le_bytes(self.page.data[5..13].try_into().unwrap())
    }

    pub fn set_leftmost_child(&mut self, child: PageId) {
        let bytes: [u8; 8] = child.to_le_bytes();
        self.page.data[5..13].copy_from_slice(&bytes);
    }

    pub fn insert_slot(&mut self, key: &[u8], child: PageId) {
        let key_count: usize = self.get_key_count();
        let insert_index: usize = self.lower_bound_key(key);
        if insert_index < key_count
            && self.get_key(insert_index) == key
            && self.get_child(insert_index) == child {
            panic!("slot entry already exists");
        }

        // move all slots over
        let src_start: usize = self.get_slot_start(insert_index);
        let src_end: usize = self.get_slot_start(key_count);
        let dst_start: usize = self.get_slot_start(insert_index + 1);
        self.page.data.copy_within(src_start..src_end, dst_start);

        // insert key, update node values
        let free_offset: usize = self.set_key(key);
        self.set_free_offset(free_offset);
        self.set_key_count(key_count + 1);

        // insert slot
        let slot_start: usize = self.get_slot_start(insert_index);
        self.page.data[slot_start..slot_start + 2].copy_from_slice(&(free_offset as u16).to_le_bytes());
        self.page.data[slot_start + 2..slot_start + 4].copy_from_slice(&(key.len() as u16).to_le_bytes());
        self.page.data[slot_start + 4..slot_start + 12].copy_from_slice(&child.to_le_bytes());
    }

    pub fn find_child_index(&self, child: PageId) -> Option<usize> {
        if self.get_leftmost_child() == child {
            return Some(0);
        }
        for i in 0..self.get_key_count() {
            if self.get_child(i) == child {
                return Some(i + 1);
            }
        }
        None
    }

    pub fn child_for_key(&self, key: &[u8]) -> PageId {
        let index = self.upper_bound_key(key);
        if index == 0 {
            self.get_leftmost_child()
        } else {
            self.get_child(index - 1)
        }
    }

    pub fn child_for_lower_bound(&self, key: &[u8]) -> PageId {
        let index = self.lower_bound_key(key);
        if index == 0 {
            self.get_leftmost_child()
        } else {
            self.get_child(index - 1)
        }
    }

    pub fn lower_bound_key(&self, key: &[u8]) -> usize {
        lower_bound(self.get_key_count(), |i| self.get_key(i) < key)
    }

    pub fn upper_bound_key(&self, key: &[u8]) -> usize {
        lower_bound(self.get_key_count(), |i| self.get_key(i) <= key)
    }

    pub fn entries(&self) -> Vec<(Vec<u8>, PageId)> {
        let mut entries = Vec::with_capacity(self.get_key_count());
        for i in 0..self.get_key_count() {
            entries.push((self.get_key(i).to_vec(), self.get_child(i)));
        }
        entries
    }

    pub fn rebuild(&mut self, leftmost_child: PageId, entries: &[(Vec<u8>, PageId)]) {
        self.init(leftmost_child);
        for (key, child) in entries {
            self.insert_slot(key, *child);
        }
    }

    pub fn child_at(&self, index: usize) -> PageId {
        if index == 0 {
            self.get_leftmost_child()
        } else {
            self.get_child(index - 1)
        }
    }
}

pub enum BNode<'a> {
    Leaf(LeafNode<'a>),
    Internal(InternalNode<'a>),
}

impl<'a> BNode<'a> {
    pub fn from_page(page: &'a mut Page) -> BNode<'a> {
        match page.data[0] {
            0 => BNode::Leaf(LeafNode::new(page)),
            _ => BNode::Internal(InternalNode::new(page)),
        }
    }
}

pub trait BNodeSerializer {
    fn get_page(&self) -> &[u8];
    fn get_page_mut(&mut self) -> &mut [u8];
    fn get_slot_start(&self, i: usize) -> usize;

    fn get_slot_key_offset(&self, i: usize) -> usize {
        let slot_start = self.get_slot_start(i);
        u16::from_le_bytes(
            self.get_page()[slot_start..slot_start+2].try_into().unwrap()
        ) as usize
    }

    fn has_space(&self, key_len: usize) -> bool {
        let free_offset: usize = self.get_free_offset();
        let next_slot_start: usize = self.get_slot_start(self.get_key_count() + 1);
        free_offset >= key_len + next_slot_start
    }

    fn get_key_count(&self) -> usize {
        u16::from_le_bytes(self.get_page()[1..3].try_into().unwrap()) as usize
    }

    fn set_key_count(&mut self, key_count: usize) {
        self.get_page_mut()[1..3].copy_from_slice(&(key_count as u16).to_le_bytes());
    }

    fn get_free_offset(&self) -> usize {
        u16::from_le_bytes(self.get_page()[3..5].try_into().unwrap()) as usize
    }

    fn set_free_offset(&mut self, free_offset: usize) {
        self.get_page_mut()[3..5].copy_from_slice(&(free_offset as u16).to_le_bytes());
    }

    fn get_key(&self, i: usize) -> &[u8] {
        let slot_start: usize = self.get_slot_start(i);
        let offset: u16 = u16::from_le_bytes(
            self.get_page()[slot_start..slot_start+2].try_into().unwrap()
        );
        let key_len: u16 = u16::from_le_bytes(
            self.get_page()[slot_start+2..slot_start+4].try_into().unwrap()
        );
        self.get_key_bytes_at_offset(offset as usize, key_len as usize)
    }

    //returns the offset of the key in the page, so caller can update the free offset
    fn set_key(&mut self, key: &[u8]) -> usize {
        let free_offset: usize = self.get_free_offset();
        let key_len: usize = key.len();
        self.set_key_bytes_at_offset(free_offset - key_len, key);
        free_offset - key_len
    }

    fn get_key_bytes_at_offset(&self, offset: usize, key_len: usize) -> &[u8] {
        &self.get_page()[offset..offset + key_len]
    }

    fn set_key_bytes_at_offset(&mut self, offset: usize, key: &[u8]) {
        self.get_page_mut()[offset..offset + key.len()].copy_from_slice(key);
    }
}

impl<'a> BNodeSerializer for LeafNode<'a> {
    fn get_page(&self) -> &[u8] { &self.page.data }
    fn get_page_mut(&mut self) -> &mut [u8] { &mut self.page.data }
    fn get_slot_start(&self, i: usize) -> usize { HEADER_SIZE + i * 14usize }
}

impl<'a> BNodeSerializer for InternalNode<'a> {
    fn get_page(&self) -> &[u8] { &self.page.data }
    fn get_page_mut(&mut self) -> &mut [u8] { &mut self.page.data }
    fn get_slot_start(&self, i: usize) -> usize { HEADER_SIZE + i * 12usize }
}

fn lower_bound<F>(len: usize, mut is_less: F) -> usize
where
    F: FnMut(usize) -> bool,
{
    let mut left = 0usize;
    let mut right = len;
    while left < right {
        let mid = left + (right - left) / 2;
        if is_less(mid) {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}
