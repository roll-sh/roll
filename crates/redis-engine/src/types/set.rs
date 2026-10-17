//! The `set` type. (Stub: to be replaced by the full implementation.)

use crate::value::Str;

#[derive(Clone, Debug, Default)]
pub struct Set {
    items: Vec<Str>,
}

impl Set {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<Str>()
    }

    pub fn encoding(&self) -> &'static str {
        "listpack"
    }
}
