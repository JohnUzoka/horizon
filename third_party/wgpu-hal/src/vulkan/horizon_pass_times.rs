//! The Switch port's per-pass GPU times (Horizon builds, `BH_GPU_PASS_TIMES=1` in env.txt). Four
//! timestamps go in for each render or compute pass: before it begins, right after it began (inside
//! a render pass, after `vkCmdBeginRenderPass`), right before it ends, and after it ended. Once the
//! GPU has run them, `harvest` (from each submit) adds the pass's time, split into its begin (load
//! and clear work), its draws and its end (store work), to the pass's label. horizon-nx's heartbeat
//! asks for the sums every 10 s (`horizon_gpu_pass_report`) and logs them per frame next to
//! `[GPU work]`, so a slow stretch shows which pass its GPU time goes to, and which part of it. Off
//! (the default), each hook is one load of a flag.
//!
//! The stamps are bottom of pipe. NVK writes a top-of-pipe stamp as soon as the command front end
//! reaches it (`PIPELINE_LOCATION_NONE`, mesa `nvk_event.c`), while the passes before it may still
//! be running, so a top-of-pipe begin charged each pass with the tail of the one before (RUN 23c's
//! first-person hand). A bottom-of-pipe stamp is written once all earlier work is done.
//!
//! `BH_GPU_PASS_TIMES=2` also puts a full pipeline barrier before each pass's first and last stamp
//! (outside the pass), so no earlier work can still be running when they are written. RUN 25 ran it:
//! the split stayed as RUN 24's, ~11-12 ms in each of the three passes after the world pass (the
//! second world pass, the hand, the HUD) whose draws are small, against 8.6 ms for the world pass.

use alloc::{borrow::ToOwned, format, string::String, vec, vec::Vec};
use ash::vk;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Stamps per pass: before it begins, after it began, before it ends, after it ended.
const STAMPS: u32 = 4;
/// Queries per command buffer: four for each of up to 32 passes.
const PER_BUFFER: u32 = 128;
/// Command buffers timed at once, recording or waiting for the GPU.
const BUFFERS: u32 = 64;
/// A block whose results never come (its command buffer was dropped unsubmitted) is given up after
/// this long.
const GIVE_UP: Duration = Duration::from_secs(5);

/// 0 off, 1 stamps, 2 stamps behind full barriers (`BH_GPU_PASS_TIMES`).
static MODE: LazyLock<u8> = LazyLock::new(|| {
    match std::env::var("BH_GPU_PASS_TIMES").as_deref().map(str::trim) {
        Ok("1") => 1,
        Ok("2") => 2,
        _ => 0,
    }
});

pub(super) fn enabled() -> bool {
    *MODE != 0
}

/// With `BH_GPU_PASS_TIMES=2`: everything recorded before this point finishes, and its writes are
/// visible, before anything after it starts. Outside any pass.
fn drain(device: &ash::Device, buffer: vk::CommandBuffer) {
    if *MODE != 2 {
        return;
    }
    let barrier = vk::MemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
    // SAFETY: `buffer` is recording, outside any pass (the callers' contract).
    unsafe {
        device.cmd_pipeline_barrier(
            buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[barrier],
            &[],
            &[],
        )
    };
}

/// A command buffer's queries.
struct Block {
    buffer: vk::CommandBuffer,
    index: u32,
    /// Each timed pass's label, and how many of its stamps went in.
    passes: Vec<(String, u32)>,
    ended: Option<Instant>,
}

/// GPU nanoseconds per label: the whole pass, its begin, its draws and its end, and the passes.
#[derive(Clone)]
struct Total {
    label: String,
    ns: [f64; 4],
    passes: u64,
}

struct Timer {
    device: ash::Device,
    pool: vk::QueryPool,
    period_ns: f64,
    free: Vec<u32>,
    recording: Vec<Block>,
    pending: Vec<Block>,
    /// Since launch, and the same at the last report.
    totals: Vec<Total>,
    reported: Vec<Total>,
}

static TIMER: Mutex<Option<Timer>> = Mutex::new(None);

fn timer() -> MutexGuard<'static, Option<Timer>> {
    TIMER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A command buffer begins recording: it gets a block of queries, reset here, outside any pass.
pub(super) fn begin_encoding(device: &ash::Device, period: f32, buffer: vk::CommandBuffer) {
    let mut guard = timer();
    if guard.is_none() {
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(PER_BUFFER * BUFFERS);
        // SAFETY: a plain query pool on the one device; kept for the process's lifetime.
        let Ok(pool) = (unsafe { device.create_query_pool(&info, None) }) else {
            return;
        };
        *guard = Some(Timer {
            device: device.clone(),
            pool,
            period_ns: f64::from(period),
            free: (0..BUFFERS).rev().collect(),
            recording: Vec::new(),
            pending: Vec::new(),
            totals: Vec::new(),
            reported: Vec::new(),
        });
    }
    let Some(timer) = guard.as_mut() else {
        return;
    };
    let Some(index) = timer.free.pop() else {
        return;
    };
    // SAFETY: `buffer` is recording and outside any pass; the queries are this block's.
    unsafe { device.cmd_reset_query_pool(buffer, timer.pool, index * PER_BUFFER, PER_BUFFER) };
    timer.recording.push(Block {
        buffer,
        index,
        passes: Vec::new(),
        ended: None,
    });
}

/// Writes the current pass's stamp number `stamp` (0-3) when its stamps so far are `stamp`, so a
/// pass whose begin found the block full gets none. `begin` starts a new pass with `label`.
fn stamp(
    device: &ash::Device,
    buffer: vk::CommandBuffer,
    stamp: u32,
    begin: Option<&str>,
    outside: bool,
) {
    let mut guard = timer();
    let Some(timer) = guard.as_mut() else {
        return;
    };
    let pool = timer.pool;
    let Some(block) = timer.recording.iter_mut().find(|b| b.buffer == buffer) else {
        return;
    };
    if let Some(label) = begin {
        if (block.passes.len() as u32 + 1) * STAMPS > PER_BUFFER {
            return;
        }
        block.passes.push((label.to_owned(), 0));
    }
    let n = block.passes.len() as u32;
    let Some(pass) = block.passes.last_mut() else {
        return;
    };
    if pass.1 != stamp {
        return;
    }
    if outside {
        drain(device, buffer);
    }
    // SAFETY: the query was reset in this command buffer; a timestamp may be written inside or
    // outside a render pass.
    unsafe {
        device.cmd_write_timestamp(
            buffer,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            pool,
            block.index * PER_BUFFER + (n - 1) * STAMPS + stamp,
        )
    };
    pass.1 += 1;
}

/// Before a pass begins (a render pass: before `vkCmdBeginRenderPass`).
pub(super) fn pass_begin(device: &ash::Device, buffer: vk::CommandBuffer, label: Option<&str>) {
    stamp(device, buffer, 0, Some(label.unwrap_or("(unnamed)")), true);
}

/// Right after a pass began (a render pass: after `vkCmdBeginRenderPass`, inside it).
pub(super) fn pass_started(device: &ash::Device, buffer: vk::CommandBuffer) {
    stamp(device, buffer, 1, None, false);
}

/// Right before a pass ends (a render pass: before `vkCmdEndRenderPass`, inside it).
pub(super) fn pass_ending(device: &ash::Device, buffer: vk::CommandBuffer) {
    stamp(device, buffer, 2, None, false);
}

/// After a pass ends (a render pass: after `vkCmdEndRenderPass`).
pub(super) fn pass_end(device: &ash::Device, buffer: vk::CommandBuffer) {
    stamp(device, buffer, 3, None, true);
}

/// The command buffer is recorded: its stamps wait for the GPU.
pub(super) fn end_encoding(buffer: vk::CommandBuffer) {
    let mut guard = timer();
    let Some(timer) = guard.as_mut() else {
        return;
    };
    let Some(i) = timer.recording.iter().position(|b| b.buffer == buffer) else {
        return;
    };
    let mut block = timer.recording.swap_remove(i);
    if block.passes.is_empty() {
        timer.free.push(block.index);
    } else {
        block.ended = Some(Instant::now());
        timer.pending.push(block);
    }
}

/// The command buffer is thrown away while recording.
pub(super) fn discard(buffer: vk::CommandBuffer) {
    let mut guard = timer();
    let Some(timer) = guard.as_mut() else {
        return;
    };
    if let Some(i) = timer.recording.iter().position(|b| b.buffer == buffer) {
        let block = timer.recording.swap_remove(i);
        timer.free.push(block.index);
    }
}

/// From each submit: adds up the blocks the GPU has finished, without waiting for the rest.
pub(super) fn harvest() {
    let mut guard = timer();
    let Some(timer) = guard.as_mut() else {
        return;
    };
    let now = Instant::now();
    let mut i = 0;
    while i < timer.pending.len() {
        let block = &timer.pending[i];
        let timed = block.passes.iter().take_while(|p| p.1 == STAMPS).count();
        let mut stamps = vec![0u64; STAMPS as usize * timed];
        // SAFETY: the block's queries, written by its command buffer; TYPE_64 without WAIT
        // answers NOT_READY until all of them are in.
        let ready = timed == 0
            || unsafe {
                timer.device.get_query_pool_results(
                    timer.pool,
                    block.index * PER_BUFFER,
                    &mut stamps,
                    vk::QueryResultFlags::TYPE_64,
                )
            }
            .is_ok();
        if ready {
            let block = timer.pending.swap_remove(i);
            let period = timer.period_ns;
            for (k, (label, _)) in block.passes.iter().take(timed).enumerate() {
                let s = &stamps[STAMPS as usize * k..STAMPS as usize * (k + 1)];
                let span = |a: usize, b: usize| s[b].saturating_sub(s[a]) as f64 * period;
                let ns = [span(0, 3), span(0, 1), span(1, 2), span(2, 3)];
                match timer.totals.iter_mut().find(|t| &t.label == label) {
                    Some(total) => {
                        for (sum, add) in total.ns.iter_mut().zip(ns) {
                            *sum += add;
                        }
                        total.passes += 1;
                    }
                    None => timer.totals.push(Total {
                        label: label.clone(),
                        ns,
                        passes: 1,
                    }),
                }
            }
            timer.free.push(block.index);
        } else if block.ended.is_some_and(|t| now.duration_since(t) > GIVE_UP) {
            let block = timer.pending.swap_remove(i);
            timer.free.push(block.index);
        } else {
            i += 1;
        }
    }
}

/// horizon-nx's heartbeat: the GPU time per pass label since the last call, per frame over
/// `frames` frames, written into `buf` (UTF-8, at most `cap` bytes). Returns the length written, 0
/// when timing is off or nothing was timed.
#[unsafe(no_mangle)]
pub extern "C" fn horizon_gpu_pass_report(buf: *mut u8, cap: usize, frames: u64) -> usize {
    if !enabled() || frames == 0 || buf.is_null() {
        return 0;
    }
    let mut guard = timer();
    let Some(timer) = guard.as_mut() else {
        return 0;
    };
    let mut rows: Vec<Total> = timer
        .totals
        .iter()
        .map(|t| {
            let before = timer.reported.iter().find(|r| r.label == t.label);
            let mut row = t.clone();
            if let Some(before) = before {
                for (now, then) in row.ns.iter_mut().zip(before.ns) {
                    *now -= then;
                }
                row.passes -= before.passes;
            }
            row
        })
        .filter(|r| r.passes > 0)
        .collect();
    rows.sort_by(|a, b| b.ns[0].total_cmp(&a.ns[0]));
    let frames = frames as f64;
    let ms = |ns: f64| ns / 1e6 / frames;
    let total: f64 = rows.iter().map(|r| r.ns[0]).sum();
    let mut text = format!("{:.2} ms a frame in timed passes:", ms(total));
    for row in &rows {
        text.push_str(&format!(
            " {} {:.2} ms ({:.1} a frame; begin {:.2}, draws {:.2}, end {:.2});",
            row.label,
            ms(row.ns[0]),
            row.passes as f64 / frames,
            ms(row.ns[1]),
            ms(row.ns[2]),
            ms(row.ns[3]),
        ));
    }
    timer.reported = timer.totals.clone();
    let bytes = text.as_bytes();
    let n = bytes.len().min(cap);
    // SAFETY: the caller gives `cap` writable bytes at `buf`.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n) };
    n
}
