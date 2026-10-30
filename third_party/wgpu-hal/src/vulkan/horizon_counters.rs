//! The Switch port's GPU-work counters (Horizon builds only). horizon-nx's heartbeat reads
//! `HORIZON_GPU_WORK` by symbol every 10 s and logs per-frame averages, so a run shows whether the
//! work the game hands the GPU grows over a session (more far terrain, more mobs) or stays put
//! while the GPU time grows, and whether a slow stretch is spent waiting for the GPU, for the
//! display, or neither (the CPU). One relaxed add per recorded call.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;

/// Totals since launch: render passes, direct draws, the vertices (or indices) times instances
/// those draws ask for, indirect draw calls, the draws those calls hold (`max_count` for the
/// count variants), the bytes buffer-to-buffer copies move (uploads go through them), and the
/// nanoseconds `acquire` waited for the GPU to finish an image's last frame and then for the
/// display to hand an image back.
#[unsafe(no_mangle)]
pub static HORIZON_GPU_WORK: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

fn nanos(waited: Duration) -> u64 {
    u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX)
}

pub(super) fn pass() {
    HORIZON_GPU_WORK[0].fetch_add(1, Relaxed);
}

pub(super) fn direct(elements: u64) {
    HORIZON_GPU_WORK[1].fetch_add(1, Relaxed);
    HORIZON_GPU_WORK[2].fetch_add(elements, Relaxed);
}

pub(super) fn indirect(draws: u32) {
    HORIZON_GPU_WORK[3].fetch_add(1, Relaxed);
    HORIZON_GPU_WORK[4].fetch_add(u64::from(draws), Relaxed);
}

pub(super) fn copied(bytes: u64) {
    HORIZON_GPU_WORK[5].fetch_add(bytes, Relaxed);
}

pub(super) fn gpu_wait(waited: Duration) {
    HORIZON_GPU_WORK[6].fetch_add(nanos(waited), Relaxed);
}

pub(super) fn image_wait(waited: Duration) {
    HORIZON_GPU_WORK[7].fetch_add(nanos(waited), Relaxed);
}
