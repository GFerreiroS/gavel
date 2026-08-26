//! A bump allocator, and the reason for it.
//!
//! `cluster-core` needs `alloc` (its event messages and task outputs are
//! `String`s). Rather than pull in a general-purpose allocator, this firmware
//! provides the simplest one that can answer the question actually worth
//! asking: **how much heap does the domain layer need on a real device?**
//!
//! It bumps a pointer and never reclaims. That is a deliberate fit for what
//! this binary does -- boot, run a bounded set of checks, report, halt -- and
//! it is emphatically *not* what a long-running node would use. A node that
//! serves tasks indefinitely needs a real allocator, or better, a domain layer
//! that does not allocate at all. The high-water mark printed at the end is the
//! measurement that tells us how far away that second option is.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Sized to be uncomfortable. If the domain layer does not fit in 32 KB, that
/// is a finding worth surfacing, not a number to quietly raise.
const HEAP_SIZE: usize = 32 * 1024;

static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static USED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static EXHAUSTED: AtomicUsize = AtomicUsize::new(0);

pub struct Bump;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = &raw const HEAP as usize;
        let mut start = 0;

        // Reserve a correctly aligned span, retrying if another context won.
        let reserved = USED.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
            let aligned = (base + used).next_multiple_of(layout.align()) - base;
            let end = aligned.checked_add(layout.size())?;
            if end > HEAP_SIZE {
                return None;
            }
            start = aligned;
            Some(end)
        });

        match reserved {
            Ok(_) => {
                PEAK.fetch_max(start + layout.size(), Ordering::Relaxed);
                (base + start) as *mut u8
            }
            Err(_) => {
                EXHAUSTED.fetch_add(1, Ordering::Relaxed);
                core::ptr::null_mut()
            }
        }
    }

    /// Bump allocators do not reclaim. See the module docs.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

pub fn capacity() -> usize {
    HEAP_SIZE
}

/// Peak bytes handed out. The number this firmware exists to report.
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Allocation failures. Must be zero for the run to count as a pass.
pub fn exhausted() -> usize {
    EXHAUSTED.load(Ordering::Relaxed)
}
