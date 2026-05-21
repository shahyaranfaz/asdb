use super::node::{BNode, BNodeSerializer, InternalNode, LeafNode, NO_NEXT_LEAF};

use crate::storage::{BufferPool, DocId, PageId};

use std::io::Result;

enum SearchStep {
    Found(DocId),
    NotFound,
    GoTo(PageId),
}

enum InsertStep {
    Done,
    GoTo(PageId),
}

enum DeleteStep {
    Done,
    GoTo(PageId),
    NotFound,
}

struct LeafSplitData {
    separator: Vec<u8>,
    lower: Vec<(Vec<u8>, DocId)>,
    upper: Vec<(Vec<u8>, DocId)>,
    old_next: PageId,
}

struct InternalSplitData {
    separator: Vec<u8>,
    leftmost_child: PageId,
    lower: Vec<(Vec<u8>, PageId)>,
    upper: Vec<(Vec<u8>, PageId)>,
    upper_leftmost_child: PageId,
}

enum SplitData {
    Leaf(LeafSplitData),
    Internal(InternalSplitData),
}

pub struct BTree<'a> {
    pub(crate) root: PageId,
    pub(crate) buffer_pool: &'a mut BufferPool,
}

impl<'a> BTree<'a> {
    pub fn new(pool: &'a mut BufferPool) -> Result<Self> {
        let (root, _) = pool.new_page(|page| {
            let mut leaf = LeafNode::new(page);
            leaf.init();
        })?;
        Ok(BTree {
            root,
            buffer_pool: pool,
        })
    }

    pub fn open(root: PageId, pool: &'a mut BufferPool) -> Self {
        BTree {
            root,
            buffer_pool: pool,
        }
    }

    pub fn root(&self) -> PageId {
        self.root
    }

    fn search_step(&mut self, node: PageId, key: &[u8]) -> Result<SearchStep> {
        self.buffer_pool
            .with_page_mut(node, |page| match BNode::from_page(page) {
                BNode::Leaf(leaf) => {
                    let index = leaf.lower_bound_key(key);
                    if index < leaf.get_key_count() && leaf.get_key(index) == key {
                        SearchStep::Found(leaf.get_doc_id(index))
                    } else {
                        SearchStep::NotFound
                    }
                }
                BNode::Internal(internal) => SearchStep::GoTo(internal.child_for_key(key)),
            })
    }

    pub fn search(&mut self, key: &[u8]) -> Result<Option<DocId>> {
        let mut curr = self.root;
        loop {
            match self.search_step(curr, key)? {
                SearchStep::GoTo(next) => curr = next,
                SearchStep::Found(doc_id) => return Ok(Some(doc_id)),
                SearchStep::NotFound => return Ok(None),
            }
        }
    }

    pub fn insert(&mut self, key: &[u8], doc_id: DocId) -> Result<()> {
        if self.is_full(self.root, key.len())? {
            self.split_node(self.root, None)?;
        }

        let mut curr = self.root;
        loop {
            let step = self.buffer_pool.with_page_mut(curr, |page| {
                match BNode::from_page(page) {
                    BNode::Leaf(mut leaf) => {
                        leaf.insert_slot(key, doc_id);
                        InsertStep::Done
                    }
                    BNode::Internal(internal) => InsertStep::GoTo(internal.child_for_key(key)),
                }
            })?;

            match step {
                InsertStep::Done => return Ok(()),
                InsertStep::GoTo(child) => {
                    if self.is_full(child, key.len())? {
                        self.split_node(child, Some(curr))?;
                    } else {
                        curr = child;
                    }
                }
            }
        }
    }

    fn is_full(&mut self, node: PageId, key_len: usize) -> Result<bool> {
        self.buffer_pool
            .with_page_mut(node, |page| match BNode::from_page(page) {
                BNode::Leaf(leaf) => !leaf.has_space(key_len),
                BNode::Internal(internal) => !internal.has_space(key_len),
            })
    }

    fn split_node(&mut self, node: PageId, parent: Option<PageId>) -> Result<()> {
        // step 1: read the split point and collect keys/values to move
        let split_data =
            self.buffer_pool
                .with_page_mut(node, |page| match BNode::from_page(page) {
                    BNode::Leaf(leaf) => {
                        let entries = leaf.entries();
                        let mid = entries.len() / 2;
                        let separator = entries[mid].0.clone();
                        let lower = entries[..mid].to_vec();
                        let upper = entries[mid..].to_vec();
                        let old_next = leaf.get_next_leaf();
                        SplitData::Leaf(LeafSplitData { separator, lower, upper, old_next })
                    }
                    BNode::Internal(internal) => {
                        let entries = internal.entries();
                        let mid = entries.len() / 2;
                        let separator = entries[mid].0.clone();
                        let leftmost_child = internal.get_leftmost_child();
                        let upper_leftmost_child = entries[mid].1;
                        let lower = entries[..mid].to_vec();
                        let upper = entries[mid + 1..].to_vec();
                        SplitData::Internal(InternalSplitData {
                            separator,
                            leftmost_child,
                            lower,
                            upper,
                            upper_leftmost_child,
                        })
                    }
                })?;

        // step 2: allocate sibling
        let sibling_id = match &split_data {
            SplitData::Leaf(data) => {
                let (sibling_id, _) = self.buffer_pool.new_page(|page| {
                    let mut sibling = LeafNode::new(page);
                    sibling.init();
                    for (key, doc_id) in &data.upper {
                        sibling.insert_slot(key, *doc_id);
                    }
                })?;
                sibling_id
            }
            SplitData::Internal(data) => {
                let (sibling_id, _) = self.buffer_pool.new_page(|page| {
                    let mut sibling = InternalNode::new(page);
                    sibling.init(data.upper_leftmost_child);
                    for (key, child) in &data.upper {
                        sibling.insert_slot(key, *child);
                    }
                })?;
                sibling_id
            }
        };

        // step 3: update offset, truncate keys, update key count
        self.buffer_pool
            .with_page_mut(node, |page| match (BNode::from_page(page), &split_data) {
                (BNode::Leaf(mut leaf), SplitData::Leaf(data)) => {
                    leaf.rebuild(&data.lower, sibling_id);
                }
                (BNode::Internal(mut internal), SplitData::Internal(data)) => {
                    internal.rebuild(data.leftmost_child, &data.lower);
                }
                _ => panic!("split data does not match node type"),
            })?;

        if let SplitData::Leaf(data) = &split_data {
            self.buffer_pool.with_page_mut(sibling_id, |page| {
                LeafNode::new(page).set_next_leaf(data.old_next);
            })?;
        }

        let separator = match &split_data {
            SplitData::Leaf(data) => &data.separator,
            SplitData::Internal(data) => &data.separator,
        };

        // step 4: update parent or create new root
        if let Some(parent) = parent {
            self.buffer_pool
                .with_page_mut(parent, |page| match BNode::from_page(page) {
                    BNode::Internal(mut internal) => {
                        internal.insert_slot(&separator, sibling_id);
                    }
                    _ => panic!("parent must be an internal node"),
                })?;
            return Ok(());
        }

        let (new_root, _) = self.buffer_pool.new_page(|page| {
            let mut root = InternalNode::new(page);
            root.init(node);
            root.insert_slot(&separator, sibling_id);
        })?;
        self.root = new_root;
        Ok(())
    }

    pub fn range_scan(&mut self, low: &[u8], high: &[u8]) -> Result<Vec<DocId>> {
        let mut curr = self.find_leaf(low)?;
        let mut result = Vec::new();
        loop {
            let next =
                self.buffer_pool
                    .with_page_mut(curr, |page| match BNode::from_page(page) {
                        BNode::Leaf(leaf) => {
                            let key_len: usize = leaf.get_key_count();
                            let mut left = leaf.lower_bound_key(low);
                            let mut done = false;
                            let mut collected = Vec::new();
                            while left < key_len {
                                let k = leaf.get_key(left);
                                if k > high {
                                    done = true;
                                    break;
                                }
                                collected.push(leaf.get_doc_id(left));
                                left += 1;
                            }
                            (collected, leaf.get_next_leaf(), done)
                        }
                        BNode::Internal(_) => panic!("expected a leaf node"),
                    })?;
            result.extend(next.0);
            if next.2 || next.1 == NO_NEXT_LEAF {
                break Ok(result);
            } else {
                curr = next.1;
            }
        }
    }

    fn find_leaf(&mut self, key: &[u8]) -> Result<PageId> {
        let mut curr = self.root;
        loop {
            let next =
                self.buffer_pool
                    .with_page_mut(curr, |page| match BNode::from_page(page) {
                        BNode::Leaf(_) => None,
                        BNode::Internal(internal) => Some(internal.child_for_lower_bound(key)),
                    })?;

            match next {
                None => return Ok(curr),
                Some(child) => curr = child,
            }
        }
    }

    fn delete_step(&mut self, node: PageId, key: &[u8], doc_id: DocId) -> Result<DeleteStep> {
        self.buffer_pool
            .with_page_mut(node, |page| match BNode::from_page(page) {
                BNode::Leaf(mut leaf) => {
                    let index = leaf.lower_bound_key_doc(key, doc_id);
                    if index < leaf.get_key_count()
                        && leaf.get_key(index) == key
                        && leaf.get_doc_id(index) == doc_id
                    {
                        let mut entries = leaf.entries();
                        entries.remove(index);
                        let next_leaf = leaf.get_next_leaf();
                        leaf.rebuild(&entries, next_leaf);
                        DeleteStep::Done
                    } else {
                        DeleteStep::NotFound
                    }
                }
                BNode::Internal(internal) => DeleteStep::GoTo(internal.child_for_key(key)),
            })
    }

    pub fn delete(&mut self, key: &[u8], doc_id: DocId) -> Result<()> {
        let mut curr = self.root;
        let mut path: Vec<PageId> = Vec::new();

        loop {
            match self.delete_step(curr, key, doc_id)? {
                DeleteStep::GoTo(child) => {
                    path.push(curr);
                    curr = child;
                }

                DeleteStep::Done => {
                    break;
                }

                DeleteStep::NotFound => {
                    return Ok(());
                }
            }
        }

        while let Some(parent) = path.pop() {
            if curr == self.root || self.key_count(curr)? > 0 {
                break;
            }
            self.fix_underflow(curr, parent)?;
            curr = parent;
        }

        self.collapse_empty_root()?;
        Ok(())
    }

    fn key_count(&mut self, node: PageId) -> Result<usize> {
        self.buffer_pool
            .with_page_mut(node, |page| match BNode::from_page(page) {
                BNode::Leaf(leaf) => leaf.get_key_count(),
                BNode::Internal(internal) => internal.get_key_count(),
            })
    }

    fn collapse_empty_root(&mut self) -> Result<()> {
        let replacement = self.buffer_pool.with_page_mut(self.root, |page| {
            match BNode::from_page(page) {
                BNode::Leaf(_) => None,
                BNode::Internal(internal) => {
                    if internal.get_key_count() == 0 {
                        Some(internal.get_leftmost_child())
                    } else {
                        None
                    }
                }
            }
        })?;
        if let Some(root) = replacement {
            self.root = root;
        }
        Ok(())
    }

    fn fix_underflow(&mut self, node: PageId, parent: PageId) -> Result<()> {
        let (child_index, left, right) = self.buffer_pool.with_page_mut(parent, |page| {
            match BNode::from_page(page) {
                BNode::Internal(internal) => {
                    let child_index = internal.find_child_index(node).expect("child must exist");
                    let left = if child_index > 0 {
                        Some(internal.child_at(child_index - 1))
                    } else {
                        None
                    };
                    let right = if child_index < internal.get_key_count() {
                        Some(internal.child_at(child_index + 1))
                    } else {
                        None
                    };
                    (child_index, left, right)
                }
                _ => panic!("parent must be an internal node"),
            }
        })?;

        if let Some(left) = left {
            if self.try_redistribute_from_left(left, node, parent, child_index - 1)? {
                return Ok(());
            }
        }
        if let Some(right) = right {
            if self.try_redistribute_from_right(node, right, parent, child_index)? {
                return Ok(());
            }
        }
        if let Some(left) = left {
            self.merge_nodes(left, node, parent, child_index - 1)
        } else if let Some(right) = right {
            self.merge_nodes(node, right, parent, child_index)
        } else {
            Ok(())
        }
    }

    fn try_redistribute_from_left(&mut self, left: PageId, node: PageId,
        parent: PageId, separator_index: usize) -> Result<bool> {

        let left_entries = self.leaf_entries(left)?;
        if left_entries.len() <= 1 { return Ok(false); }

        let mut node_entries = self.leaf_entries(node)?;
        let moved = left_entries.last().unwrap().clone();

        let new_left = left_entries[..left_entries.len() - 1].to_vec();
        node_entries.insert(0, moved);

        let left_next = self.leaf_next(left)?;
        let node_next = self.leaf_next(node)?;
        self.rebuild_leaf(left, &new_left, left_next)?;
        self.rebuild_leaf(node, &node_entries, node_next)?;
        self.replace_parent_separator(parent, separator_index, node_entries[0].0.clone())?;
        Ok(true)
    }

    fn try_redistribute_from_right(&mut self, node: PageId, right: PageId,
        parent: PageId, separator_index: usize) -> Result<bool> {

        let right_entries = self.leaf_entries(right)?;
        if right_entries.len() <= 1 { return Ok(false); }

        let mut node_entries = self.leaf_entries(node)?;
        let moved = right_entries[0].clone();

        let new_right = right_entries[1..].to_vec();
        node_entries.push(moved);

        let node_next = self.leaf_next(node)?;
        let right_next = self.leaf_next(right)?;
        self.rebuild_leaf(node, &node_entries, node_next)?;
        self.rebuild_leaf(right, &new_right, right_next)?;
        self.replace_parent_separator(parent, separator_index, new_right[0].0.clone())?;
        Ok(true)
    }

    fn merge_nodes(&mut self, left: PageId, right: PageId, parent: PageId,
                   separator_index: usize) -> Result<()> {
        let mut merged = self.leaf_entries(left)?;
        merged.extend(self.leaf_entries(right)?);
        let right_next = self.leaf_next(right)?;
        self.rebuild_leaf(left, &merged, right_next)?;
        self.remove_parent_separator(parent, separator_index)
    }

    fn leaf_entries(&mut self, node: PageId) -> Result<Vec<(Vec<u8>, DocId)>> {
        self.buffer_pool.with_page_mut(node, |page| match BNode::from_page(page) {
            BNode::Leaf(leaf) => leaf.entries(),
            _ => panic!("expected a leaf node"),
        })
    }

    fn leaf_next(&mut self, node: PageId) -> Result<PageId> {
        self.buffer_pool.with_page_mut(node, |page| match BNode::from_page(page) {
            BNode::Leaf(leaf) => leaf.get_next_leaf(),
            _ => panic!("expected a leaf node"),
        })
    }

    fn rebuild_leaf(&mut self, node: PageId, entries: &[(Vec<u8>, DocId)],
                    next_leaf: PageId) -> Result<()> {
        self.buffer_pool.with_page_mut(node, |page| match BNode::from_page(page) {
            BNode::Leaf(mut leaf) => leaf.rebuild(entries, next_leaf),
            _ => panic!("expected a leaf node"),
        })
    }

    fn replace_parent_separator(&mut self, parent: PageId,
                                separator_index: usize, key: Vec<u8>) -> Result<()> {
        self.buffer_pool.with_page_mut(parent, |page| match BNode::from_page(page) {
            BNode::Internal(mut internal) => {
                let leftmost = internal.get_leftmost_child();
                let mut entries = internal.entries();
                let child = entries[separator_index].1;
                entries[separator_index] = (key, child);
                internal.rebuild(leftmost, &entries);
            }
            _ => panic!("parent must be an internal node"),
        })
    }

    fn remove_parent_separator(&mut self, parent: PageId, separator_index: usize) -> Result<()> {
        self.buffer_pool.with_page_mut(parent, |page| match BNode::from_page(page) {
            BNode::Internal(mut internal) => {
                let leftmost = internal.get_leftmost_child();
                let mut entries = internal.entries();
                entries.remove(separator_index);
                internal.rebuild(leftmost, &entries);
            }
            _ => panic!("parent must be an internal node"),
        })
    }

    pub fn free(&mut self) -> Result<()> {
        let mut stack = vec![self.root];

        while let Some(page_id) = stack.pop() {
            let children = self.buffer_pool.with_page_mut(page_id, |page| {
                match BNode::from_page(page) {
                    BNode::Leaf(_) => Vec::new(),
                    BNode::Internal(internal) => {
                        let mut children = Vec::with_capacity(internal.get_key_count() + 1);
                        children.push(internal.get_leftmost_child());
                        for i in 0..internal.get_key_count() {
                            children.push(internal.get_child(i));
                        }
                        children
                    }
                }
            })?;

            stack.extend(children);
            self.buffer_pool.delete_page(page_id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;
    use tempfile::NamedTempFile;

    fn make_tree(capacity: usize) -> (BTree<'static>, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let disk = DiskManager::open(tmp.path()).unwrap();
        let bp = Box::new(BufferPool::new(disk, capacity));
        let bp = Box::leak(bp);
        (BTree::new(bp).unwrap(), tmp)
    }

    fn key(i: u32) -> Vec<u8> {
        format!("key-{i:04}").into_bytes()
    }

    fn doc(i: u32) -> DocId {
        (i as PageId + 10, i as u16)
    }

    #[test]
    fn test_empty_search() {
        let (mut tree, _tmp) = make_tree(8);
        assert_eq!(tree.search(b"missing").unwrap(), None);
    }

    #[test]
    fn test_insert_search_without_split() {
        let (mut tree, _tmp) = make_tree(8);
        tree.insert(b"alpha", doc(1)).unwrap();
        tree.insert(b"bravo", doc(2)).unwrap();
        tree.insert(b"charlie", doc(3)).unwrap();

        assert_eq!(tree.search(b"alpha").unwrap(), Some(doc(1)));
        assert_eq!(tree.search(b"bravo").unwrap(), Some(doc(2)));
        assert_eq!(tree.search(b"charlie").unwrap(), Some(doc(3)));
        assert_eq!(tree.search(b"delta").unwrap(), None);
    }

    #[test]
    fn test_insert_search_across_root_split() {
        let (mut tree, _tmp) = make_tree(16);
        for i in 0..500 {
            tree.insert(&key(i), doc(i)).unwrap();
        }

        for i in 0..500 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
        assert_ne!(tree.root(), 0);
    }

    #[test]
    fn test_range_scan_across_leaf_split() {
        let (mut tree, _tmp) = make_tree(16);
        for i in 0..500 {
            tree.insert(&key(i), doc(i)).unwrap();
        }

        let got = tree.range_scan(&key(120), &key(135)).unwrap();
        let expected: Vec<DocId> = (120..=135).map(doc).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn test_duplicate_keys_are_ordered_by_doc_id() {
        let (mut tree, _tmp) = make_tree(8);
        tree.insert(b"dup", doc(3)).unwrap();
        tree.insert(b"dup", doc(1)).unwrap();
        tree.insert(b"dup", doc(2)).unwrap();

        let got = tree.range_scan(b"dup", b"dup").unwrap();
        assert_eq!(got, vec![doc(1), doc(2), doc(3)]);
    }

    #[test]
    fn test_delete_leaf_entry() {
        let (mut tree, _tmp) = make_tree(8);
        tree.insert(b"alpha", doc(1)).unwrap();
        tree.insert(b"bravo", doc(2)).unwrap();

        tree.delete(b"alpha", doc(1)).unwrap();

        assert_eq!(tree.search(b"alpha").unwrap(), None);
        assert_eq!(tree.search(b"bravo").unwrap(), Some(doc(2)));
    }

    #[test]
    fn test_delete_merge_after_leaf_underflow() {
        let (mut tree, _tmp) = make_tree(16);
        for i in 0..300 {
            tree.insert(&key(i), doc(i)).unwrap();
        }

        for i in 0..150 {
            tree.delete(&key(i), doc(i)).unwrap();
        }

        for i in 0..150 {
            assert_eq!(tree.search(&key(i)).unwrap(), None);
        }
        for i in 150..300 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
    }

    #[test]
    fn test_delete_redistributes_after_leaf_underflow() {
        let (mut tree, _tmp) = make_tree(16);
        for i in 0..500 {
            tree.insert(&key(i), doc(i)).unwrap();
        }

        for i in 90..120 {
            tree.delete(&key(i), doc(i)).unwrap();
        }

        for i in 90..120 {
            assert_eq!(tree.search(&key(i)).unwrap(), None);
        }
        for i in 0..90 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
        for i in 120..500 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
    }

    #[test]
    fn test_delete_many_keys_after_deeper_splits() {
        let (mut tree, _tmp) = make_tree(32);
        for i in 0..2_000 {
            tree.insert(&key(i), doc(i)).unwrap();
        }

        for i in 250..1_750 {
            tree.delete(&key(i), doc(i)).unwrap();
        }

        for i in 0..250 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
        for i in 250..1_750 {
            assert_eq!(tree.search(&key(i)).unwrap(), None);
        }
        for i in 1_750..2_000 {
            assert_eq!(tree.search(&key(i)).unwrap(), Some(doc(i)));
        }
    }
}
