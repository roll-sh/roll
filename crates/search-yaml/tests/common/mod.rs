//! Shared by the integration tests: an allocator that refuses any single allocation over 1 GiB,
//! so code that sizes a buffer from untrusted input (a test file, a response) fails loudly here
//! instead of exhausting memory on a real run.

use std::alloc::{GlobalAlloc, Layout, System};

struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;
