//! Test-executable-only observer, preserving the daemon's System allocator.

// Rust guideline compliant 2026-02-21

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub allocations: u64,
    pub deallocations: u64,
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<Counts> = const { Cell::new(Counts { allocations: 0, deallocations: 0 }) };
}

fn record(allocation: bool) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNTS.try_with(|cell| {
            let mut value = cell.get();
            if allocation {
                value.allocations = value.allocations.saturating_add(1);
            } else {
                value.deallocations = value.deallocations.saturating_add(1);
            }
            cell.set(value);
        });
    }
}

struct Observer;

// SAFETY: pointer ownership and layout contracts are forwarded unchanged to
// System. Observation uses only constant-initialized, non-allocating TLS cells.
unsafe impl GlobalAlloc for Observer {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(true);
        // SAFETY: the caller provides GlobalAlloc's valid layout contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(true);
        // SAFETY: the caller provides GlobalAlloc's valid layout contract.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(true);
        // SAFETY: the original pointer/layout and new size satisfy GlobalAlloc.
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(false);
        // SAFETY: ownership/layout are passed directly back to System.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Observer = Observer;

pub(crate) struct Window;

impl Window {
    pub fn begin() -> Self {
        assert!(!ACTIVE.with(Cell::get), "nested allocation observation");
        COUNTS.with(|counts| counts.set(Counts::default()));
        ACTIVE.with(|active| active.set(true));
        Self
    }
    pub fn finish(self) -> Counts {
        let counts = COUNTS.with(Cell::get);
        drop(self);
        counts
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::Window;

    #[test]
    fn observes_zeroed_allocation_reallocation_and_free() {
        let layout = std::alloc::Layout::from_size_align(64, 16).unwrap();
        let grown_layout = std::alloc::Layout::from_size_align(128, 16).unwrap();
        let window = Window::begin();
        // SAFETY: both layouts are valid. Only initialized prefix bytes are
        // inspected; reallocation preserves alignment and ownership. A failed
        // realloc frees the original allocation before failing the test.
        unsafe {
            let pointer = std::alloc::alloc_zeroed(layout);
            assert!(!pointer.is_null());
            assert!(
                std::slice::from_raw_parts(pointer, 64)
                    .iter()
                    .all(|byte| *byte == 0)
            );
            let grown = std::alloc::realloc(pointer, layout, 128);
            if grown.is_null() {
                std::alloc::dealloc(pointer, layout);
                panic!("canary realloc failed");
            }
            assert!(
                std::slice::from_raw_parts(grown, 64)
                    .iter()
                    .all(|byte| *byte == 0)
            );
            std::alloc::dealloc(grown, grown_layout);
        }
        let counts = window.finish();
        assert_eq!(counts.allocations, 2);
        assert_eq!(counts.deallocations, 1);
    }

    #[test]
    fn observer_counter_saturates_without_unwinding_the_allocator() {
        let window = Window::begin();
        super::COUNTS.with(|counts| {
            counts.set(super::Counts {
                allocations: u64::MAX,
                deallocations: u64::MAX,
            })
        });
        drop(std::hint::black_box(Box::new([0u8; 64])));
        let counts = window.finish();
        assert_eq!(counts.allocations, u64::MAX);
        assert_eq!(counts.deallocations, u64::MAX);
    }

    #[test]
    fn canary_and_unwind_restore_observation() {
        let window = Window::begin();
        let allocation = std::hint::black_box(Box::new([0u8; 64]));
        drop(allocation);
        let counts = window.finish();
        assert!(counts.allocations > 0 && counts.deallocations > 0);
        let _ = std::panic::catch_unwind(|| {
            let _window = Window::begin();
            panic!("unwind");
        });
        assert_eq!(Window::begin().finish().allocations, 0);
    }
}
