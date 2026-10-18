//! `TrieMap` (redisearch_rs `trie_rs`): a radix tree over byte strings, its nodes split, merged
//! and sized exactly as the module's, so `TrieMap_MemUsage` (FT.INFO's key table and tag
//! overheads) comes out to the byte.

/// `size_of::<TrieMap<*mut c_void>>()`: the root pointer, key count and memory counter.
pub const MAP_SIZE: usize = 24;

#[derive(Clone, Debug)]
struct Node<D> {
    label: Vec<u8>,
    /// Sorted by first byte.
    children: Vec<Node<D>>,
    data: Option<D>,
}

/// `PtrMetadata::compute` for `Data = *mut c_void`: a 4-byte header, the label, the children's
/// first bytes, the children pointers (8-aligned) and an `Option<Data>` (16 bytes).
const fn node_size(label: usize, children: usize) -> usize {
    (4 + label + children).div_ceil(8) * 8 + 8 * children + 16
}

impl<D> Node<D> {
    fn leaf(label: &[u8], data: D) -> Self {
        Node { label: label.to_vec(), children: Vec::new(), data: Some(data) }
    }

    fn size(&self) -> usize {
        node_size(self.label.len(), self.children.len())
    }

    fn child_index(&self, c: u8) -> Option<usize> {
        self.children.iter().position(|n| n.label[0] == c)
    }

    fn insertion_index(&self, c: u8) -> usize {
        self.children.partition_point(|n| n.label[0] < c)
    }

    /// `split_unchecked`: the label's suffix from `at` moves to a new child (with the
    /// children and data), plus an optional extra child.
    fn split(&mut self, at: usize, extra: Option<Node<D>>, mem: &mut usize) {
        let old = self.size();
        let child = Node { label: self.label[at..].to_vec(), children: std::mem::take(&mut self.children), data: self.data.take() };
        self.label.truncate(at);
        *mem = *mem + node_size(at, 1 + extra.is_some() as usize) + child.size() + extra.as_ref().map_or(0, |c| c.size()) - old;
        self.children.push(child);
        if let Some(e) = extra {
            let i = self.insertion_index(e.label[0]);
            self.children.insert(i, e);
        }
    }

    fn insert_with(&mut self, mut key: &[u8], f: impl FnOnce(Option<D>) -> D, mem: &mut usize) -> bool {
        let mut cur = self;
        loop {
            let common = cur.label.iter().zip(key).take_while(|(a, b)| a == b).count();
            if common < cur.label.len() && common < key.len() {
                if common == 0 {
                    // Only the root can differ from its first byte: a new empty-labelled root.
                    let new_child = Node::leaf(key, f(None));
                    *mem += new_child.size();
                    let old = std::mem::replace(cur, Node { label: Vec::new(), children: Vec::new(), data: None });
                    cur.children = if old.label[0] > key[0] { vec![new_child, old] } else { vec![old, new_child] };
                    *mem += cur.size();
                } else {
                    let new_child = Node::leaf(&key[common..], f(None));
                    cur.split(common, Some(new_child), mem);
                }
                return true;
            }
            if common == key.len() && key.len() < cur.label.len() {
                cur.split(key.len(), None, mem);
                cur.data = Some(f(None));
                return true;
            }
            if key.len() == cur.label.len() {
                let had = cur.data.is_some();
                let old = cur.data.take();
                cur.data = Some(f(old));
                return !had;
            }
            key = &key[cur.label.len()..];
            match cur.child_index(key[0]) {
                Some(i) => cur = &mut cur.children[i],
                None => {
                    let i = cur.insertion_index(key[0]);
                    let child = Node::leaf(key, f(None));
                    *mem = *mem + node_size(cur.label.len(), cur.children.len() + 1) + child.size() - cur.size();
                    cur.children.insert(i, child);
                    return true;
                }
            }
        }
    }

    fn remove_descendant(&mut self, key: &[u8], mem: &mut usize) -> Option<D> {
        let i = self.child_index(*key.first()?)?;
        let child = &mut self.children[i];
        let suffix = key.strip_prefix(child.label.as_slice())?;
        if suffix.is_empty() {
            let data = child.data.take();
            if child.children.is_empty() {
                let removed = child.size();
                *mem = *mem + node_size(self.label.len(), self.children.len() - 1) - self.size() - removed;
                self.children.remove(i);
            } else {
                child.merge_child(mem);
            }
            data
        } else {
            let data = child.remove_descendant(suffix, mem);
            child.merge_child(mem);
            data
        }
    }

    /// `merge_child_if_possible`: a data-less node with one child absorbs it.
    fn merge_child(&mut self, mem: &mut usize) {
        if self.data.is_some() || self.children.len() != 1 {
            return;
        }
        let old_parent = self.size();
        let mut child = self.children.pop().unwrap();
        let old_child = child.size();
        let mut label = std::mem::take(&mut self.label);
        label.extend_from_slice(&child.label);
        child.label = label;
        *mem = *mem + child.size() - old_parent - old_child;
        *self = child;
    }

    fn find(&self, mut key: &[u8]) -> Option<&Node<D>> {
        let mut cur = self;
        loop {
            key = key.strip_prefix(cur.label.as_slice())?;
            let Some(&c) = key.first() else { return Some(cur) };
            cur = &cur.children[cur.child_index(c)?];
        }
    }

    fn find_mut(&mut self, mut key: &[u8]) -> Option<&mut Node<D>> {
        let mut cur = self;
        loop {
            key = key.strip_prefix(cur.label.as_slice())?;
            let Some(&c) = key.first() else { return Some(cur) };
            let i = cur.child_index(c)?;
            cur = &mut cur.children[i];
        }
    }

    fn walk<'a>(&'a self, prefix: &mut Vec<u8>, out: &mut dyn FnMut(&[u8], &'a D) -> bool) -> bool {
        let n = prefix.len();
        prefix.extend_from_slice(&self.label);
        if let Some(d) = &self.data
            && !out(prefix, d)
        {
            prefix.truncate(n);
            return false;
        }
        for c in &self.children {
            if !c.walk(prefix, out) {
                prefix.truncate(n);
                return false;
            }
        }
        prefix.truncate(n);
        true
    }
}

/// A byte-string map ordered lexicographically.
#[derive(Clone, Debug)]
pub struct TrieMap<D> {
    root: Option<Node<D>>,
    len: usize,
    mem: usize,
}

impl<D> Default for TrieMap<D> {
    fn default() -> Self {
        TrieMap { root: None, len: 0, mem: MAP_SIZE }
    }
}

impl<D> TrieMap<D> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `TrieMap_MemUsage`.
    pub fn mem_usage(&self) -> usize {
        self.mem
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// `n_nodes`: the root and its descendants.
    pub fn n_nodes(&self) -> usize {
        fn count<D>(n: &Node<D>) -> usize {
            1 + n.children.iter().map(count).sum::<usize>()
        }
        self.root.as_ref().map_or(0, count)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts or replaces; returns the old value.
    pub fn insert(&mut self, key: &[u8], data: D) -> Option<D> {
        let mut old = None;
        self.insert_with(key, |o| {
            old = o;
            data
        });
        old
    }

    pub fn insert_with(&mut self, key: &[u8], f: impl FnOnce(Option<D>) -> D) {
        let added = match &mut self.root {
            None => {
                let root = Node::leaf(key, f(None));
                self.mem += root.size();
                self.root = Some(root);
                true
            }
            Some(root) => root.insert_with(key, f, &mut self.mem),
        };
        if added {
            self.len += 1;
        }
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<D> {
        let root = self.root.as_mut()?;
        let suffix = key.strip_prefix(root.label.as_slice())?;
        let data = if suffix.is_empty() {
            if root.children.is_empty() {
                self.mem = MAP_SIZE;
                self.root.take().and_then(|n| n.data)
            } else {
                let d = root.data.take();
                root.merge_child(&mut self.mem);
                d
            }
        } else {
            let d = root.remove_descendant(suffix, &mut self.mem);
            root.merge_child(&mut self.mem);
            d
        };
        if data.is_some() {
            self.len -= 1;
        }
        data
    }

    pub fn get(&self, key: &[u8]) -> Option<&D> {
        self.root.as_ref()?.find(key)?.data.as_ref()
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut D> {
        self.root.as_mut()?.find_mut(key)?.data.as_mut()
    }

    /// `TrieMap_FindPrefixes`: the values of every key that is a prefix of `key`, shortest first.
    pub fn find_prefixes(&self, mut key: &[u8]) -> Vec<&D> {
        let mut out = Vec::new();
        let Some(mut cur) = self.root.as_ref() else { return out };
        loop {
            let Some(rest) = key.strip_prefix(cur.label.as_slice()) else { return out };
            if let Some(d) = &cur.data {
                out.push(d);
            }
            let Some(&c) = rest.first() else { return out };
            let Some(i) = cur.child_index(c) else { return out };
            cur = &cur.children[i];
            key = rest;
        }
    }

    /// Every entry in key order; `f` returns false to stop.
    pub fn for_each<'a>(&'a self, mut f: impl FnMut(&[u8], &'a D) -> bool) {
        if let Some(r) = &self.root {
            r.walk(&mut Vec::new(), &mut f);
        }
    }

    /// The entries whose keys start with `prefix`, in key order.
    pub fn for_each_prefix<'a>(&'a self, prefix: &[u8], mut f: impl FnMut(&[u8], &'a D) -> bool) {
        self.for_each(|k, d| if k.starts_with(prefix) { f(k, d) } else { k < prefix });
    }

    pub fn keys(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.len);
        self.for_each(|k, _| {
            out.push(k.to_vec());
            true
        });
        out
    }

    pub fn values_mut(&mut self) -> Vec<&mut D> {
        fn go<'a, D>(n: &'a mut Node<D>, out: &mut Vec<&'a mut D>) {
            if let Some(d) = n.data.as_mut() {
                out.push(d);
            }
            for c in &mut n.children {
                go(c, out);
            }
        }
        let mut out = Vec::new();
        if let Some(r) = self.root.as_mut() {
            go(r, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_sizes() {
        // FT.INFO key_table_size_mb with keys zzd1, zzd2: 120 bytes.
        let mut t = TrieMap::new();
        t.insert(b"zzd1", 1);
        assert_eq!(t.mem_usage(), 24 + 24);
        t.insert(b"zzd2", 2);
        assert_eq!(t.mem_usage(), 120);
        // Tag values "a" and "B": 112 bytes.
        let mut t = TrieMap::new();
        t.insert(b"a", 1);
        t.insert(b"B", 2);
        assert_eq!(t.mem_usage(), 112);
        assert_eq!(t.keys(), vec![b"B".to_vec(), b"a".to_vec()]);
    }

    #[test]
    fn insert_remove_consistent() {
        fn sum<D>(n: &Node<D>) -> usize {
            n.size() + n.children.iter().map(sum).sum::<usize>()
        }
        let words: Vec<String> = (0..300).map(|i| format!("k{}{}", i % 7, i * 31 % 113)).collect();
        let mut t = TrieMap::new();
        for (i, w) in words.iter().enumerate() {
            t.insert(w.as_bytes(), i);
            assert_eq!(t.mem_usage(), MAP_SIZE + t.root.as_ref().map_or(0, sum));
        }
        let mut sorted = words.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(t.len(), sorted.len());
        assert_eq!(t.keys(), sorted.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
        for w in words.iter().step_by(3) {
            t.remove(w.as_bytes());
            assert_eq!(t.mem_usage(), MAP_SIZE + t.root.as_ref().map_or(0, sum));
            assert!(t.get(w.as_bytes()).is_none());
        }
        for w in &words {
            t.remove(w.as_bytes());
        }
        assert_eq!(t.mem_usage(), MAP_SIZE);
        assert!(t.is_empty());
    }
}
