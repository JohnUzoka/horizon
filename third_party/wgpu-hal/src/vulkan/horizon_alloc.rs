//! [bevy-horizon patch] Vulkan allocation callbacks for the Switch (Horizon builds only).
//!
//! Mesa's default allocator is newlib's malloc, and NVK allocates and frees through it for every
//! queue submit, command-buffer reset and meta operation. newlib's `free` was the largest single
//! cost on the game's main thread (profile14b: `_free_r` 27.8% of its running samples, 91% of that
//! called from C: `vk_meta_object_list_reset`, `nvk_descriptor_state_fini`,
//! `vk_common_QueueSubmit`, ...). Given to `vkCreateInstance`, these callbacks send Mesa's
//! allocations through Rust's global allocator, horizon-nx's caches: the device, and every object
//! created with a null allocator, inherit the instance's. `BH_VK_ALLOCATOR=0` keeps Mesa's malloc.

use alloc::alloc::{Layout, alloc, dealloc, realloc};
use ash::vk;
use core::ffi::c_void;
use core::ptr::null_mut;
use std::sync::OnceLock;

/// Each block's total size and alignment sit in the 16 bytes just before the pointer Vulkan gets,
/// which is `align` bytes into the block (`align` is at least 16), so the free and reallocation
/// callbacks can rebuild the block's `Layout`.
const HEADER: usize = 16;

static CALLBACKS: vk::AllocationCallbacks<'static> = vk::AllocationCallbacks {
    p_user_data: null_mut(),
    pfn_allocation: Some(allocation),
    pfn_reallocation: Some(reallocation),
    pfn_free: Some(free),
    pfn_internal_allocation: None,
    pfn_internal_free: None,
    _marker: core::marker::PhantomData,
};

/// The callbacks to create the instance with, or `None` (Mesa's malloc) with `BH_VK_ALLOCATOR=0`.
/// Decided once, so the instance is destroyed with the callbacks it was created with.
pub(super) fn callbacks() -> Option<&'static vk::AllocationCallbacks<'static>> {
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| {
        let on = std::env::var("BH_VK_ALLOCATOR").map_or(true, |v| v.trim() != "0");
        if on {
            log::info!("BH_VK_ALLOCATOR: Mesa's Vulkan allocations go through Rust's allocator");
        } else {
            log::info!("BH_VK_ALLOCATOR=0: Mesa's Vulkan allocations use malloc");
        }
        on
    });
    on.then_some(&CALLBACKS)
}

/// A block holding `size` bytes after `align` bytes of header and padding, or `None` if that
/// overflows or `align` isn't a power of two.
fn block_layout(size: usize, align: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

/// Writes the header of a block allocated with `layout` and returns the pointer Vulkan gets.
///
/// # Safety
/// `base` must hold `layout.size()` bytes, and `layout.align()` must be at least [`HEADER`].
unsafe fn place(base: *mut u8, layout: Layout) -> *mut c_void {
    // SAFETY: the pointer is `align` bytes into the block; the header is the 16 bytes before it.
    unsafe {
        let ptr = base.add(layout.align());
        ptr.cast::<usize>().sub(2).write(layout.size());
        ptr.cast::<usize>().sub(1).write(layout.align());
        ptr.cast()
    }
}

/// The block behind a pointer [`place`] returned, and the layout it was allocated with.
///
/// # Safety
/// `memory` must come from [`place`] and not have been freed.
unsafe fn block(memory: *mut c_void) -> (*mut u8, Layout) {
    // SAFETY: `place` wrote the header from a valid layout.
    unsafe {
        let ptr = memory.cast::<u8>();
        let size = ptr.cast::<usize>().sub(2).read();
        let align = ptr.cast::<usize>().sub(1).read();
        (ptr.sub(align), Layout::from_size_align_unchecked(size, align))
    }
}

unsafe extern "system" fn allocation(
    _user_data: *mut c_void,
    size: usize,
    alignment: usize,
    _scope: vk::SystemAllocationScope,
) -> *mut c_void {
    let Some(layout) = block_layout(size, alignment.max(HEADER)) else {
        return null_mut();
    };
    // SAFETY: the layout's size includes the header, so it isn't zero.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return null_mut();
    }
    // SAFETY: `base` holds `layout`, whose alignment is at least HEADER.
    unsafe { place(base, layout) }
}

unsafe extern "system" fn reallocation(
    user_data: *mut c_void,
    original: *mut c_void,
    size: usize,
    alignment: usize,
    scope: vk::SystemAllocationScope,
) -> *mut c_void {
    if original.is_null() {
        return unsafe { allocation(user_data, size, alignment, scope) };
    }
    if size == 0 {
        unsafe { free(user_data, original) };
        return null_mut();
    }
    // SAFETY: Vulkan hands back a pointer these callbacks returned.
    let (base, old) = unsafe { block(original) };
    // Vulkan reallocates with the original's alignment.
    let Some(new) = block_layout(size, old.align()) else {
        return null_mut();
    };
    // SAFETY: `base` was allocated with `old`; the new size is nonzero. On failure the original
    // stays allocated, as Vulkan requires.
    let base = unsafe { realloc(base, old, new.size()) };
    if base.is_null() {
        return null_mut();
    }
    // SAFETY: `base` now holds `new`, whose alignment is the original's, at least HEADER.
    unsafe { place(base, new) }
}

unsafe extern "system" fn free(_user_data: *mut c_void, memory: *mut c_void) {
    if memory.is_null() {
        return;
    }
    // SAFETY: Vulkan hands back a pointer these callbacks returned, once.
    unsafe {
        let (base, layout) = block(memory);
        dealloc(base, layout);
    }
}
