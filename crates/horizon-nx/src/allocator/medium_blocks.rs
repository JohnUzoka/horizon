//! A cache of Rust's medium blocks in front of newlib's malloc.
//!
//! The small cache (`small_blocks.rs`) stops at 8 KiB and the top-down region (`allocator.rs`)
//! takes 256 KiB and up, so blocks between go straight to newlib's malloc: Doug Lea's 2.6.5 behind
//! one lock, which coalesces a freed block with its neighbours and sorts it into a size bin there
//! and then. The Switch port's CPU profile (log8, `cpu-profile8.txt`) shows that band is where the
//! freezes are: the world-open thread spent 95% of its CPU in `_free_r` and an LOD mesher thread
//! 48%, and four 0.7-3.2 fps freezes of 22-59 s each each begin as a new mesher thread spawns. An
//! LOD drop frees a whole `Lod` at once - thousands of `Column` and mesh vectors of 8-256 KiB - and
//! dlmalloc sorts every one back in, only for the next LOD's allocations to sort them back out.
//!
//! Here a freed block of that band goes onto its size class's free list, and the next allocation of
//! the class takes it back without touching malloc, so after warm-up dlmalloc sees only the blocks
//! the cache hasn't absorbed. A cached block is a malloc block of its class's size, exactly as in
//! the small cache, so it always goes back to `free` and `malloc_usable_size` reads its real
//! header. That is why routing is by the layout's size and no block is ever inspected to decide
//! where it goes, and why no cached block can reach `System.realloc`.
//!
//! Unlike the small cache, a class has one free list, not magazines in shards: these blocks are big
//! and far rarer, so eight shards' worth of two-block magazines of the larger classes would cost
//! more memory than it saves. The list is bounded by a byte budget, [`BUDGET`], instead of a
//! magazine count: log8's live set from 64 KiB up is 7.5 MiB, but an LOD drop frees a burst far
//! larger than the steady state, and the small cache's fixed four-magazine depot is what sent 633k
//! magazines back to malloc. The budget lets a list grow to absorb the burst; past it a freed block
//! goes to malloc as before. The locks are flags only ever tried, never waited for, as in the small
//! cache: a thread that finds a list held falls back to malloc, because Horizon runs each core's
//! highest-priority ready thread and spinning on a flag held by a lower-priority thread on the same
//! core would starve it.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::small_blocks::{hold_patiently, try_hold, Backend, MAX_ALIGN};

/// The smallest block this cache takes. At or below it the small cache handles the block.
pub(crate) const MIN: usize = (8 << 10) + 1;
/// The largest block this cache takes. At or above [`super::LARGE`] the region takes the block, so
/// this is the largest size below it; the top class is [`super::LARGE`] itself.
pub(crate) const MAX: usize = super::LARGE - 1;

/// The size classes, in KiB, from just above the small cache's largest to the region's smallest.
/// Coarse is fine: these blocks are big, so a little rounding wastes little, and fewer classes means
/// less contention and less bookkeeping.
const CLASS_KIB: [usize; 11] = [12, 16, 24, 32, 48, 64, 96, 128, 192, 224, 256];
const CLASSES: usize = CLASS_KIB.len();

/// The most the cache holds, in bytes. log8's live set from 64 KiB up is 7.5 MiB; the budget leaves
/// room for an LOD drop's burst to be absorbed rather than dumped into malloc. A cached block is a
/// malloc block it counts as in use, and malloc has hundreds of MiB free (log8: 682 MiB in holes),
/// so this is a small slice of the ~3 GiB heap.
const BUDGET: usize = 48 << 20;

/// The class a block of `size` bytes goes in, or `None` if this cache doesn't take it (too small,
/// too large, or over the alignment malloc offers).
#[inline]
pub(crate) fn class_of(size: usize, align: usize) -> Option<usize> {
    if size < MIN || size > MAX || align > MAX_ALIGN {
        return None;
    }
    // The first class at least as big as `size`. The table is short and sorted; a linear scan is
    // cheaper than a binary search's branches and runs once per medium block.
    (0..CLASSES).find(|&c| CLASS_KIB[c] << 10 >= size)
}

/// The size of a class's blocks.
#[inline]
const fn class_size(class: usize) -> usize {
    CLASS_KIB[class] << 10
}

const SIZES: [usize; CLASSES] = {
    let mut sizes = [0; CLASSES];
    let mut class = 0;
    while class < CLASSES {
        sizes[class] = class_size(class);
        class += 1;
    }
    sizes
};

/// A class's free blocks and the lock over them.
#[repr(align(64))]
struct Slot {
    locked: AtomicBool,
    /// Free blocks, each holding the next in its first word; null-terminated. Only read or written
    /// with `locked` held; a cell so `stats` can read the count lock-free.
    head: UnsafeCell<*mut u8>,
    /// Blocks on `head`. Written only with `locked` held; atomic so `stats` can read it.
    len: AtomicUsize,
}

impl Slot {
    const fn new() -> Self {
        Self { locked: AtomicBool::new(false), head: UnsafeCell::new(null_mut()), len: AtomicUsize::new(0) }
    }
}

/// What the cache has done, for the `[Memory]` line.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stats {
    pub(crate) enabled: bool,
    /// Allocations the cache served from a free block.
    pub(crate) hits: usize,
    /// Allocations it had no block for, which malloc served.
    pub(crate) misses: usize,
    /// Allocations that found the class's list held, which malloc served.
    pub(crate) busy_allocs: usize,
    /// Frees that found the list held, were over budget, or came after the cache was turned off,
    /// and went to malloc.
    pub(crate) busy_frees: usize,
    /// Frees of blocks smaller than their class, which went to malloc: blocks allocated while the
    /// cache was off, or C's.
    pub(crate) rejected: usize,
    /// Times malloc ran out with blocks in the cache, which gave them back.
    pub(crate) drains: usize,
    /// Bytes in cached blocks.
    pub(crate) cached: usize,
}

pub(crate) struct Cache<B> {
    enabled: AtomicBool,
    slots: [Slot; CLASSES],
    /// Bytes in cached blocks, compared against [`BUDGET`] before a block is kept.
    cached_bytes: AtomicUsize,
    hits: AtomicUsize,
    misses: AtomicUsize,
    busy_allocs: AtomicUsize,
    busy_frees: AtomicUsize,
    rejected: AtomicUsize,
    drains: AtomicUsize,
    backend: PhantomData<B>,
}

// SAFETY: a slot's list is only touched with its flag held, and the flags' Acquire and Release
// order those accesses between threads. `cached_bytes` is a counter, updated atomically; two
// threads can pass the budget check at once, so it may exceed [`BUDGET`] by a block or two, which
// only bounds memory.
unsafe impl<B> Sync for Cache<B> {}

impl<B: Backend> Cache<B> {
    pub(crate) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            slots: [const { Slot::new() }; CLASSES],
            cached_bytes: AtomicUsize::new(0),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
            busy_allocs: AtomicUsize::new(0),
            busy_frees: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
            drains: AtomicUsize::new(0),
            backend: PhantomData,
        }
    }

    /// The class for a block of `size` bytes aligned to `align`, or `None` if the cache is off or
    /// doesn't take it.
    #[inline]
    pub(crate) fn class_for(&self, size: usize, align: usize) -> Option<usize> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        class_of(size, align)
    }

    /// The bytes a live block can hold.
    #[inline]
    pub(crate) unsafe fn usable_size(&self, ptr: *mut u8) -> usize {
        unsafe { B::usable_size(ptr) }
    }

    /// A block of `class`, the class `class_for` gave for `size` bytes, with those bytes zeroed if
    /// `zeroed`; null if malloc has no room for one.
    ///
    /// # Safety
    /// `class` is below [`CLASSES`].
    #[inline]
    pub(crate) unsafe fn alloc(&self, class: usize, size: usize, zeroed: bool) -> *mut u8 {
        let slot = &self.slots[class];
        if let Some(held) = try_hold(&slot.locked) {
            // Turning the cache off empties each slot with it held; looking again here keeps this
            // from refilling a slot after that.
            if self.enabled.load(Ordering::Relaxed) {
                // SAFETY: the slot is held.
                let head = unsafe { *slot.head.get() };
                if !head.is_null() {
                    unsafe { *slot.head.get() = next(head) };
                    slot.len.fetch_sub(1, Ordering::Relaxed);
                    self.cached_bytes.fetch_sub(SIZES[class], Ordering::Relaxed);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    drop(held);
                    if zeroed {
                        unsafe { head.write_bytes(0, size) };
                    }
                    return head;
                }
            }
            drop(held);
        } else {
            self.busy_allocs.fetch_add(1, Ordering::Relaxed);
        }
        // No free block, or the list is held: malloc serves a block of the class's size, so a later
        // free of it finds a real malloc block of the class's size, which the cache can keep.
        self.misses.fetch_add(1, Ordering::Relaxed);
        unsafe { B::alloc(SIZES[class], zeroed) }
    }

    /// Keeps `ptr`, a block of `class`, or gives it back to malloc.
    ///
    /// # Safety
    /// `ptr` is a live block from the backend, `class` is the class of the size it was allocated
    /// for, and `ptr` isn't used again.
    #[inline]
    pub(crate) unsafe fn free(&self, ptr: *mut u8, class: usize) {
        // A block allocated while the cache was off, or by C, can be smaller than its class.
        if unsafe { B::usable_size(ptr) } < SIZES[class] {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return unsafe { B::free(ptr) };
        }
        // Over budget: give it back rather than grow the cache past its share of the heap.
        if self.cached_bytes.load(Ordering::Relaxed) + SIZES[class] > BUDGET {
            self.busy_frees.fetch_add(1, Ordering::Relaxed);
            return unsafe { B::free(ptr) };
        }
        let slot = &self.slots[class];
        let Some(held) = try_hold(&slot.locked) else {
            self.busy_frees.fetch_add(1, Ordering::Relaxed);
            return unsafe { B::free(ptr) };
        };
        // Turning the cache off empties each slot with it held, so nothing goes in after that.
        if !self.enabled.load(Ordering::Relaxed) {
            drop(held);
            return unsafe { B::free(ptr) };
        }
        // SAFETY: the slot is held.
        unsafe { set_next(ptr, *slot.head.get()) };
        unsafe { *slot.head.get() = ptr };
        slot.len.fetch_add(1, Ordering::Relaxed);
        self.cached_bytes.fetch_add(SIZES[class], Ordering::Relaxed);
    }

    /// Gives every block in a list back to malloc.
    unsafe fn free_list(mut block: *mut u8) {
        while !block.is_null() {
            let following = unsafe { next(block) };
            unsafe { B::free(block) };
            block = following;
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Turns the cache on or off. Turning it off gives its blocks back to malloc, as
    /// [`Cache::drain`] does.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::SeqCst) && !enabled {
            self.empty();
        }
    }

    /// For when malloc has run out: gives it the cache's blocks, except any in a slot that stays
    /// held for 10 ms. False if there were none to give, so asking malloc again is no use.
    pub(crate) fn drain(&self) -> bool {
        let gave = self.empty();
        if gave {
            self.drains.fetch_add(1, Ordering::Relaxed);
        }
        gave
    }

    /// Gives back the cached blocks it can hold the slots of; true if any.
    fn empty(&self) -> bool {
        let mut gave = false;
        for (class, slot) in self.slots.iter().enumerate() {
            let Some(_held) = hold_patiently(&slot.locked) else {
                continue;
            };
            // SAFETY: the slot is held.
            let head = unsafe { &mut *slot.head.get() };
            if !head.is_null() {
                gave = true;
                self.cached_bytes.fetch_sub(slot.len.load(Ordering::Relaxed) * SIZES[class], Ordering::Relaxed);
                unsafe { Self::free_list(*head) };
                *head = null_mut();
            }
            slot.len.store(0, Ordering::Relaxed);
        }
        gave
    }

    /// Doesn't allocate or wait.
    pub(crate) fn stats(&self) -> Stats {
        let relaxed = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
        Stats {
            enabled: self.enabled(),
            hits: relaxed(&self.hits),
            misses: relaxed(&self.misses),
            busy_allocs: relaxed(&self.busy_allocs),
            busy_frees: relaxed(&self.busy_frees),
            rejected: relaxed(&self.rejected),
            drains: relaxed(&self.drains),
            cached: relaxed(&self.cached_bytes),
        }
    }
}

/// The block after `block` in its list.
#[inline]
unsafe fn next(block: *mut u8) -> *mut u8 {
    unsafe { block.cast::<*mut u8>().read() }
}

#[inline]
unsafe fn set_next(block: *mut u8, next: *mut u8) {
    unsafe { block.cast::<*mut u8>().write(next) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_cover_the_band_once() {
        // The band starts just above the small cache's largest and ends just below the region's.
        assert_eq!(class_of(MIN - 1, 8), None);
        assert_eq!(class_of(MAX + 1, 8), None);
        assert_eq!(class_of(MIN, 32), None); // over the alignment malloc offers
        let mut last = 0;
        let mut size = MIN;
        while size <= MAX {
            let class = class_of(size, 8).unwrap();
            assert!(size <= class_size(class), "{size} -> {class}");
            if class > 0 {
                assert!(size > class_size(class - 1), "{size} -> {class}");
            }
            assert!(class == last || class == last + 1);
            last = class;
            size += 1;
        }
        assert_eq!(last, CLASSES - 1);
        assert_eq!(class_size(CLASSES - 1), super::super::LARGE);
        assert!(MIN > super::super::small_blocks::MAX_SIZE);
        for class in 0..CLASSES {
            assert_eq!(class_size(class) % MAX_ALIGN, 0);
        }
    }
}
