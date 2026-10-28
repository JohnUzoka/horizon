//! Nintendo Switch (Horizon OS) runtime for Horizon: the libnx lifecycle hooks, boot/exit
//! logging, crash handler, heap reporting and C shims the `aarch64-nintendo-horizon` target
//! needs, ported from bevy-horizon's `bevy_horizon` crate with the Bevy parts removed.
//!
//! Linking this crate is what makes an NRO work: its `#[no_mangle]` `userAppInit`/`userAppExit`,
//! `__wrap_threadCreate`/`__wrap_fchmod`/`__wrap_rename`/`__wrap_rmdir` (the target spec's
//! `--wrap=` pre-link args), `getrandom`/`_getentropy_r`/`sysconf`/`_init`/`_fini` and exception
//! handlers come from `compat`, and its `build.rs` adds the devkitA64 link-search paths and
//! `-lnx -lsysbase -lc -lm -lgcc` that nothing else in the Horizon graph emits.
//!
//! `init` runs the service setup (romfs, BSD sockets, applet focus mode) and starts the log
//! writer; call it first thing in `main`. `flush_logs` and `process_applet_messages` are for the
//! game loop (Phase 2).

pub mod compat;
pub mod lifecycle;
pub mod logging;

#[cfg(target_os = "horizon")]
mod memory;
#[cfg(all(target_os = "horizon", feature = "top_down_alloc"))]
mod allocator;
#[cfg(all(target_os = "horizon", feature = "vulkan"))]
pub mod nvk_tune;
#[cfg(target_os = "horizon")]
pub mod profiler;

pub use compat::{
    ThreadCorePolicy, set_thread_core_policy, set_thread_logging, thread_core_policy,
    thread_logging,
};
pub use lifecycle::{
    flush_logs, init_horizon_services, process_applet_messages, register_release_hook,
    suspend_until_out_of_background,
};
pub use logging::{flush_switch_logs, init_switch_logging, nro_dir};

/// One line on the malloc heap: what malloc has in use and has taken from the heap, and how
/// much of the heap nvmap objects hold. On the Switch that one heap holds everything: the
/// game's allocations and, with the `vulkan` feature, every GPU buffer and image. Walks the
/// heap with `svcQueryMemory`, so call it at load stages, not every frame.
#[cfg(target_os = "horizon")]
pub fn memory_report() -> String {
    match memory::nvmap_heap() {
        Some((blocks, bytes)) => format!(
            "{}; {:.1} MiB of the heap is mapped for the GPU (nvmap), in {blocks} blocks",
            memory::malloc_report(),
            bytes as f64 / (1024.0 * 1024.0),
        ),
        None => memory::malloc_report(),
    }
}

/// Sets up the Horizon services and logging. Call at the very start of `main`, before anything
/// allocates heavily or opens sockets. Does nothing on other targets.
pub fn init() {
    #[cfg(target_os = "horizon")]
    {
        lifecycle::init_horizon_services();
        lifecycle::configure_memory_line_from_env();
        init_switch_logging("horizon");
        // A homebrew loader leaves the working directory undefined, and the game resolves its
        // game home, options.txt, saves/ and world/ from it. Move into the NRO's own folder so
        // every file the user puts next to the NRO is found. No-op when the loader already
        // starts there (Ryujinx passes the NRO at the SD root, cwd = sdmc:/).
        if let Some(dir) = nro_dir() {
            match std::env::set_current_dir(&dir) {
                Ok(()) => crate::logging::write_log_line(format_args!(
                    "[Boot] working directory -> {} (the NRO's folder)",
                    dir.display()
                )),
                Err(e) => crate::logging::write_log_line(format_args!(
                    "[Boot] could not enter {}: {e}; files must go where the loader's cwd points",
                    dir.display()
                )),
            }
        }
        // Only with HORIZON_CPU_PROFILE set (profiler.rs).
        profiler::start_from_env();
        // BH_ALLOC_CACHE=0 turns the allocator's small-block cache off.
        #[cfg(feature = "top_down_alloc")]
        allocator::configure_from_env();
        // ZCULL direction for reversed depth, unless HORIZON_NX_ZCULL_DIR=less (nvk_tune.rs).
        #[cfg(feature = "vulkan")]
        nvk_tune::configure_from_env();
        // NVK's Switch WSI reads `bh_wsi_switch_image_count` (compat.rs) when it makes a
        // swapchain; set it from the env before any surface exists.
        #[cfg(feature = "vulkan")]
        compat::horizon_stubs::set_swapchain_images();
    }
}
