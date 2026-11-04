//! The game's `main` on an 8 MiB stack. Upstream gives its main thread 8 MiB on every platform
//! (`crates/horizon-client/build.rs`: macOS and Linux do by default, Windows by a linker flag),
//! and its event loop builds a whole `Renderer` deep inside `about_to_wait`: `loading_renderer`
//! blocks on `Renderer::new`'s future when the title artwork arrives. The homebrew loader's main
//! thread has far less, and run 41 overflowed it right there: a data abort with `far == sp` in
//! that future's prologue, on the main thread.
//!
//! A thread of its own would change more than the stack: pthreads run at priority 0x3B and the
//! main thread at 0x2C, and Horizon never time-slices threads of one priority; the profiler names
//! the main thread "main"; and the game's thread-local destructors would run when it ended. So
//! the main thread stays and only its stack moves: a block from the heap, mapped into the stack
//! region with unmapped guard pages around it the way libnx maps a thread's stack
//! (`threadCreate`: `virtmemFindStack`, then `svcMapMemory`), with `sp` at its top while `main`
//! runs. An overflow faults on the guard pages and the crash report names it, as before.
//!
//! The mapping locks the block's heap pages, so it is undone when `main` returns. A
//! `process::exit` from inside leaves it mapped: the exit's heap check then finds locked pages and
//! leaves through AM to HOME (`lifecycle::release_horizon_services`).

use crate::logging::{
    ffi::{MemoryInfo, svcQueryMemory},
    write_log_line,
};
use core::ffi::c_void;

/// Upstream's main thread stack (`crates/horizon-client/build.rs`).
const STACK_SIZE: usize = 8 << 20;
/// The unmapped room libnx leaves on each side of a thread's stack (`threadCreate`).
const GUARD_SIZE: usize = 0x4000;

unsafe extern "C" {
    fn __libnx_aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    fn __libnx_free(p: *mut c_void);
    fn virtmemLock();
    fn virtmemUnlock();
    fn virtmemFindStack(size: usize, guard_size: usize) -> *mut c_void;
    fn svcMapMemory(dst: *mut c_void, src: *mut c_void, size: u64) -> u32;
    fn svcUnmapMemory(dst: *mut c_void, src: *mut c_void, size: u64) -> u32;
    /// The trampoline below: `f(data)` with `sp` at `top`, then back on the caller's stack.
    fn horizon_nx_on_stack(data: *mut c_void, f: unsafe extern "C" fn(*mut c_void), top: *mut u8);
}

// x0 = data, x1 = f, x2 = top. The frame record stays on the caller's stack and x29 points at it
// while `f` runs on the new one, so frame-pointer walks (the crash report's) and the CFI
// (CFA = x29 + 16, for a DWARF backtrace) both lead back across. Panics abort on the Switch (the
// target spec's `panic-strategy`), so nothing unwinds through it.
core::arch::global_asm!(
    ".pushsection .text.horizon_nx_on_stack,\"ax\",%progbits",
    ".globl horizon_nx_on_stack",
    ".type horizon_nx_on_stack, %function",
    ".p2align 2",
    "horizon_nx_on_stack:",
    ".cfi_startproc",
    "stp x29, x30, [sp, #-16]!",
    ".cfi_def_cfa_offset 16",
    ".cfi_offset w30, -8",
    ".cfi_offset w29, -16",
    "mov x29, sp",
    ".cfi_def_cfa w29, 16",
    "mov sp, x2",
    "blr x1",
    "mov sp, x29",
    ".cfi_def_cfa wsp, 16",
    "ldp x29, x30, [sp], #16",
    ".cfi_def_cfa_offset 0",
    ".cfi_restore w30",
    ".cfi_restore w29",
    "ret",
    ".cfi_endproc",
    ".size horizon_nx_on_stack, . - horizon_nx_on_stack",
    ".popsection",
);

/// On the new stack: runs the game's main.
unsafe extern "C" fn call_main(data: *mut c_void) {
    // SAFETY: `run_on_main_stack` passes its own `fn()`, which outlives this call.
    let main = unsafe { *data.cast::<fn()>() };
    main();
}

/// Runs `main` on this thread, the main one, with an 8 MiB stack; call it once, from `main`. If
/// the stack can't be mapped, `main` runs on the loader's stack as before, and the log says why.
pub fn run_on_main_stack(main: fn()) {
    let loader = loader_stack();
    // SAFETY: libnx's allocator for thread stacks; checked for null below.
    let memory = unsafe { __libnx_aligned_alloc(0x1000, STACK_SIZE) };
    if memory.is_null() {
        write_log_line(format_args!(
            "[Main] no {} MiB for main's stack in the heap: main runs on the loader's stack ({loader})",
            STACK_SIZE >> 20
        ));
        return main();
    }
    // SAFETY: libnx's own recipe for a thread's stack, with the virtual memory manager locked
    // from finding the range until it is mapped.
    let (stack, rc) = unsafe {
        virtmemLock();
        let stack = virtmemFindStack(STACK_SIZE, GUARD_SIZE);
        let rc = if stack.is_null() {
            u32::MAX
        } else {
            svcMapMemory(stack, memory, STACK_SIZE as u64)
        };
        virtmemUnlock();
        (stack, rc)
    };
    if rc != 0 {
        // SAFETY: from `__libnx_aligned_alloc` above, and not mapped.
        unsafe { __libnx_free(memory) };
        write_log_line(format_args!(
            "[Main] mapping main's stack failed ({rc:#x}): main runs on the loader's stack ({loader})"
        ));
        return main();
    }
    write_log_line(format_args!(
        "[Main] main runs on an {} MiB stack at {:#x}, guard pages around it as libnx gives a thread, instead of the loader's ({loader})",
        STACK_SIZE >> 20,
        stack as usize
    ));
    let mut main = main;
    // SAFETY: the mapping is this thread's alone until it is unmapped below, and its top is
    // page-aligned, so 16-byte aligned as AArch64 requires of `sp`.
    unsafe {
        horizon_nx_on_stack((&raw mut main).cast(), call_main, stack.cast::<u8>().add(STACK_SIZE));
    }
    // SAFETY: back on the loader's stack; nothing refers to the new one any more.
    let rc = unsafe { svcUnmapMemory(stack, memory, STACK_SIZE as u64) };
    if rc == 0 {
        // SAFETY: from `__libnx_aligned_alloc` above, no longer mapped.
        unsafe { __libnx_free(memory) };
    } else {
        write_log_line(format_args!(
            "[Main] unmapping main's stack failed ({rc:#x}): its heap pages stay locked"
        ));
    }
}

/// The loader's main thread stack, for the log: the mapping around a local of this frame.
fn loader_stack() -> String {
    let marker = 0u8;
    let mut info = MemoryInfo::default();
    let mut page = 0u32;
    // SAFETY: writes only `info` and `page`.
    let rc = unsafe { svcQueryMemory(&mut info, &mut page, core::ptr::addr_of!(marker) as u64) };
    if rc == 0 {
        format!("{} KiB at {:#x}", info.size >> 10, info.addr)
    } else {
        format!("svcQueryMemory failed: {rc:#x}")
    }
}
