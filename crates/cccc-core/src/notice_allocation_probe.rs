//! Profiling-only test allocator; this file is not part of the shipping patch.
//! Track allocation ownership by pointer, so frees/reallocations are accounted
//! even when they occur outside a notice mutation scope. Allocator bookkeeping
//! is excluded with a recursion guard. Sizes are requested bytes, not RSS or
//! malloc usable sizes.
#![allow(unsafe_code)]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ENTERED: Cell<bool> = const { Cell::new(false) };
    static COUNTER: Cell<Option<&'static AtomicI64>> = const { Cell::new(None) };
}
struct Record {
    size: usize,
    counter: &'static AtomicI64,
}
fn records() -> &'static Mutex<HashMap<usize, Record>> {
    static RECORDS: OnceLock<Mutex<HashMap<usize, Record>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn change(old: usize, next: usize, size: usize) {
    if ENTERED.try_with(|flag| flag.replace(true)).unwrap_or(true) {
        return;
    }
    let selected = ACTIVE
        .try_with(|active| active.get())
        .unwrap_or(false)
        .then(|| COUNTER.try_with(Cell::get).ok().flatten())
        .flatten();
    {
        let mut records = records()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = records.remove(&old);
        let owner = previous.as_ref().map(|record| record.counter).or(selected);
        if let Some(previous) = previous {
            previous
                .counter
                .fetch_sub(previous.size as i64, Ordering::Relaxed);
        }
        if next != 0
            && let Some(counter) = owner
        {
            records.insert(next, Record { size, counter });
            counter.fetch_add(size as i64, Ordering::Relaxed);
        }
    }
    let _ = ENTERED.try_with(|flag| flag.set(false));
}
struct Probe;
#[global_allocator]
static ALLOCATOR: Probe = Probe;
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        change(0, pointer as usize, layout.size());
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        change(0, pointer as usize, layout.size());
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        change(pointer as usize, 0, 0);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            change(pointer as usize, next as usize, size);
        }
        next
    }
}
pub(crate) fn begin() -> &'static AtomicI64 {
    let counter = Box::leak(Box::new(AtomicI64::new(0)));
    COUNTER.with(|current| current.set(Some(counter)));
    counter
}
pub(crate) struct Scope(bool);
pub(crate) fn scope() -> Scope {
    Scope(ACTIVE.with(|active| active.replace(true)))
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.0));
    }
}
