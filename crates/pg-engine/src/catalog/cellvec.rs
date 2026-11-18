//! A vector whose elements a thread may also change through a shared reference, one element
//! by one thread at a time (`CellVec::cell`): a heap model's pages, which the writers of a size
//! model lock's group (`exec::sizing::modellock`) change a page each under its page's latch
//! while the model is shared among them. Everything else reads and writes it as a slice
//! (`Deref`, `DerefMut`): under the model's lock in shared or exclusive mode, when no group
//! holds it, so no element is being changed meanwhile.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};

pub struct CellVec<T>(Vec<UnsafeCell<T>>);

// (Shared access reads elements as a slice only while nobody changes one, and changes one
// through `cell` only while it keeps every other thread off that element: see `cell`.)
unsafe impl<T: Send + Sync> Sync for CellVec<T> {}

impl<T> Default for CellVec<T> {
    fn default() -> CellVec<T> {
        CellVec(Vec::new())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for CellVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> Deref for CellVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        let s: &[UnsafeCell<T>] = &self.0;
        // (UnsafeCell<T> has T's layout. Callers in a group never take the slice: `cell`.)
        unsafe { &*(s as *const [UnsafeCell<T>] as *const [T]) }
    }
}

impl<T> DerefMut for CellVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        let s: &mut [UnsafeCell<T>] = &mut self.0;
        unsafe { &mut *(s as *mut [UnsafeCell<T>] as *mut [T]) }
    }
}

impl<T> From<Vec<T>> for CellVec<T> {
    fn from(v: Vec<T>) -> CellVec<T> {
        CellVec(v.into_iter().map(UnsafeCell::new).collect())
    }
}

impl<T> CellVec<T> {
    /// Element `i`'s cell (None past the end), for a thread that changes it through a shared
    /// reference to the vector. While it does, no other thread may touch that element, nor
    /// take the vector as a slice (`Deref`).
    pub fn cell(&self, i: usize) -> Option<&UnsafeCell<T>> {
        self.0.get(i)
    }

    /// Elements held (without taking the slice).
    pub fn count(&self) -> usize {
        self.0.len()
    }

    pub fn push(&mut self, t: T) {
        self.0.push(UnsafeCell::new(t));
    }

    pub fn truncate(&mut self, n: usize) {
        self.0.truncate(n);
    }

    pub fn resize_with(&mut self, n: usize, mut f: impl FnMut() -> T) {
        self.0.resize_with(n, || UnsafeCell::new(f()));
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub fn reserve(&mut self, n: usize) {
        self.0.reserve(n);
    }
}

impl<'a, T> IntoIterator for &'a CellVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut CellVec<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
