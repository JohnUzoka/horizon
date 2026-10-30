//! O8: a CPU boost while the game loads. libnx's `appletSetCpuBoostMode(ApmCpuBoostMode_FastLoad)`
//! raises the CPU from 1020 to 1785 MHz and throttles the GPU to its minimum
//! (switch/services/apm.h). Loading on the Switch is CPU-bound: in log14 the two big loader threads
//! were on a core 88% and 90% of their lives, and with the boost (log16) resources loaded in 88 s
//! instead of 174 s. But at the GPU's minimum the menus draw at about 8.5 fps and feel dead to the
//! pad (log16: 8.4-8.9 fps for the first 160 s, until the world). So `init` switches the boost on,
//! and it goes off as soon as the player touches a control (a pad button or stick, or the touch
//! screen; checked once a frame from `process_applet_messages`) or the game grabs the cursor,
//! which it does once the player is in a world (`horizon_nx_entered_world`), whichever comes first.
//! `HORIZON_NX_CPU_BOOST=0` in env.txt keeps the normal clocks, to compare. Only an Application
//! (title takeover) may boost; an applet (album) logs why and loads at the normal clocks.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

use crate::logging::write_log_line;

/// libnx `ApmCpuBoostMode` (switch/services/apm.h): only these two are allowed with
/// `appletSetCpuBoostMode`.
const BOOST_NORMAL: u32 = 0;
const BOOST_FAST_LOAD: u32 = 1;
/// libnx `AppletType_Application` (switch/services/applet.h).
const APPLET_TYPE_APPLICATION: i32 = 0;

unsafe extern "C" {
    fn appletGetAppletType() -> i32;
    fn appletSetCpuBoostMode(mode: u32) -> u32;
    fn padConfigureInput(max_players: u32, style_set: u32);
    fn padInitializeWithMask(pad: *mut PadState, pad_mask: u64);
    fn padUpdate(pad: *mut PadState);
    fn hidGetTouchScreenStates(states: *mut TouchScreenState, count: usize) -> usize;
}

/// The boost is on: set by `start_from_env`, cleared by the first input or world.
static BOOSTED: AtomicBool = AtomicBool::new(false);
/// When the boost went on (forced by `start_from_env` as it does), for the line that ends it.
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// libnx `HidAnalogStickState`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StickState {
    x: i32,
    y: i32,
}

/// libnx `PadState` (`switch/runtime/pad.h`), its layout asserted below.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PadState {
    id_mask: u8,
    active_id_mask: u8,
    read_handheld: bool,
    active_handheld: bool,
    style_set: u32,
    attributes: u32,
    _pad: u32,
    buttons_cur: u64,
    buttons_old: u64,
    sticks: [StickState; 2],
    gc_triggers: [u32; 2],
}

const _: () = {
    assert!(core::mem::size_of::<PadState>() == 56);
    assert!(core::mem::offset_of!(PadState, buttons_cur) == 16);
    assert!(core::mem::offset_of!(PadState, sticks) == 32);
};

/// libnx `HidTouchScreenState`: the sampling number and the touch count lead it; the touches
/// that follow (16 of 0x28 bytes) are not read here.
#[repr(C)]
struct TouchScreenState {
    sampling_number: u64,
    count: i32,
    reserved: u32,
    touches: [[u8; 0x28]; 16],
}

/// libnx `HidNpadStyleSet_NpadStandard`, and player 1 plus the handheld controls.
const STYLE_SET_STANDARD: u32 = 0x1F;
const PAD_MASK: u64 = (1 << 0) | (1 << 32);
/// How far a stick must tilt (of 32767) to count as the player touching it.
const STICK_TOUCHED: i32 = 8000;

thread_local! {
    /// The pad read while boosted; only the main thread (the game loop) asks.
    static PAD: RefCell<Option<PadState>> = const { RefCell::new(None) };
}

/// Switches the boost on for the loading ahead, unless `HORIZON_NX_CPU_BOOST=0`. `init` calls it
/// once, after env.txt is applied.
pub(crate) fn start_from_env() {
    if std::env::var("HORIZON_NX_CPU_BOOST").is_ok_and(|v| v.trim() == "0") {
        write_log_line(format_args!("[Boost] CPU boost off (HORIZON_NX_CPU_BOOST=0)\n"));
        return;
    }
    // SAFETY: plain libnx applet calls; libnx sets up the applet service before `main`.
    let kind = unsafe { appletGetAppletType() };
    if kind != APPLET_TYPE_APPLICATION {
        write_log_line(format_args!(
            "[Boost] no CPU boost: running as applet type {kind}, not an Application; loading at the normal clocks\n"
        ));
        return;
    }
    // SAFETY: as above.
    let rc = unsafe { appletSetCpuBoostMode(BOOST_FAST_LOAD) };
    if rc == 0 {
        LazyLock::force(&STARTED);
        BOOSTED.store(true, Ordering::SeqCst);
        write_log_line(format_args!(
            "[Boost] CPU boost on while loading (CPU 1785 MHz, GPU at its minimum) until the first input or the world\n"
        ));
    } else {
        write_log_line(format_args!(
            "[Boost] appletSetCpuBoostMode(FastLoad) failed: 0x{rc:x}; loading at the normal clocks\n"
        ));
    }
}

/// Once a frame, from `process_applet_messages`: while boosted, a pressed pad button, a tilted
/// stick or a finger on the touch screen ends the boost, so the menus get the GPU back.
pub(crate) fn end_on_input() {
    if !BOOSTED.load(Ordering::Relaxed) {
        return;
    }
    let pad_touched = PAD.with(|pad| {
        let mut pad = pad.borrow_mut();
        let state = pad.get_or_insert_with(|| {
            let mut state = PadState::default();
            // SAFETY: libnx's pad setup; the controllers' own reader does the same, which is
            // harmless twice. `state` is a plain struct libnx fills.
            unsafe {
                padConfigureInput(1, STYLE_SET_STANDARD);
                padInitializeWithMask(&mut state, PAD_MASK);
            }
            state
        });
        // SAFETY: set up above; padUpdate only writes it.
        unsafe { padUpdate(state) };
        state.buttons_cur != 0
            || state
                .sticks
                .iter()
                .any(|s| s.x.abs() > STICK_TOUCHED || s.y.abs() > STICK_TOUCHED)
    });
    let touched = pad_touched || {
        let mut state = core::mem::MaybeUninit::<TouchScreenState>::zeroed();
        // SAFETY: libnx writes at most one state into the zeroed buffer.
        let n = unsafe { hidGetTouchScreenStates(state.as_mut_ptr(), 1) };
        n > 0 && unsafe { state.assume_init_ref() }.count > 0
    };
    if touched {
        end("input in the menus");
    }
}

/// The winit backend calls this whenever the game grabs the cursor; the first call means the player
/// is in a world, so the clocks go back to normal and the GPU runs at full speed for play. Every
/// later call returns at once.
#[unsafe(no_mangle)]
pub extern "C" fn horizon_nx_entered_world() {
    end("the player is in a world");
}

/// Ends the boost (once) and says why.
fn end(why: &str) {
    if !BOOSTED.swap(false, Ordering::SeqCst) {
        return;
    }
    // SAFETY: as in `start_from_env`.
    let rc = unsafe { appletSetCpuBoostMode(BOOST_NORMAL) };
    let secs = STARTED.elapsed().as_secs_f64();
    write_log_line(format_args!(
        "[Boost] CPU boost off: {why}, {secs:.1} s after it went on (result 0x{rc:x})\n"
    ));
}
