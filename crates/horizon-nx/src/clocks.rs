//! The clocks in the log, and a faster-GPU diagnostic.
//!
//! An application runs in the system's performance configurations: in handheld mode 0x00020003
//! (CPU 1020 MHz, GPU 307.2 MHz, memory 1331.2 MHz). The game ships at those clocks, so they are
//! the default. `[Clocks]` lines log the mode (handheld or docked) and both modes' configurations
//! at launch and whenever the system says the mode changed (`log_clocks`, from
//! `process_applet_messages`), so a log says which clocks its frames had.
//!
//! `HORIZON_NX_GPU_CLOCKS` in env.txt asks apm for faster configurations the system offers games
//! (`apmSetPerformanceConfiguration`; switchbrew "PTM services", PerformanceConfiguration; libnx
//! switch/services/apm.h, where `appletInitialize` sets apm up for an Application), as a
//! diagnostic, not for shipping. `fast` is 0x92220008 in handheld mode (GPU 460.8 MHz, memory still
//! 1331.2 MHz) and 0x00010001 docked (GPU 768 MHz); `max` raises handheld memory to 1600 MHz too
//! (0x92220007). Against a run at the default, `fast` shows how much of a frame's GPU time scales
//! with the GPU's clock (shader and raster work) and how much doesn't (memory traffic). The CPU
//! boost's FastLoad configurations stand in for either only while the boost is on (boost.rs).

use crate::logging::write_log_line;

/// libnx `ApmPerformanceMode`: Normal is the handheld mode, Boost the docked one.
const MODE_HANDHELD: i32 = 0;
const MODE_DOCKED: i32 = 1;
/// libnx `AppletOperationMode_Console`: docked.
const OPERATION_DOCKED: i32 = 1;
/// libnx `AppletType_Application` (switch/services/applet.h).
const APPLET_TYPE_APPLICATION: i32 = 0;

/// Handheld GPU 460.8 MHz with memory at 1331.2 MHz, and with memory at 1600 MHz.
const HANDHELD_FAST: u32 = 0x9222_0008;
const HANDHELD_MAX: u32 = 0x9222_0007;
/// Docked GPU 768 MHz.
const DOCKED_FAST: u32 = 0x0001_0001;

/// switchbrew "PTM services", PerformanceConfiguration: the clocks behind each value.
const CONFIGURATIONS: [(u32, &str); 16] = [
    (0x0001_0000, "CPU 1020 / GPU 384 / memory 1600 MHz"),
    (0x0001_0001, "CPU 1020 / GPU 768 / memory 1600 MHz"),
    (0x0001_0002, "CPU 1224 / GPU 691.2 / memory 1600 MHz"),
    (0x0002_0000, "CPU 1020 / GPU 230.4 / memory 1600 MHz"),
    (0x0002_0001, "CPU 1020 / GPU 307.2 / memory 1600 MHz"),
    (0x0002_0002, "CPU 1224 / GPU 230.4 / memory 1600 MHz"),
    (0x0002_0003, "CPU 1020 / GPU 307.2 / memory 1331.2 MHz"),
    (0x0002_0004, "CPU 1020 / GPU 384 / memory 1331.2 MHz"),
    (0x0002_0005, "CPU 1020 / GPU 307.2 / memory 1065.6 MHz"),
    (0x0002_0006, "CPU 1020 / GPU 384 / memory 1065.6 MHz"),
    (0x9222_0007, "CPU 1020 / GPU 460.8 / memory 1600 MHz"),
    (0x9222_0008, "CPU 1020 / GPU 460.8 / memory 1331.2 MHz"),
    (0x9222_0009, "CPU 1785 / GPU 76.8 / memory 1600 MHz"),
    (0x9222_000A, "CPU 1785 / GPU 76.8 / memory 1331.2 MHz"),
    (0x9222_000B, "CPU 1020 / GPU 76.8 / memory 1600 MHz"),
    (0x9222_000C, "CPU 1020 / GPU 76.8 / memory 1331.2 MHz"),
];

unsafe extern "C" {
    fn appletGetAppletType() -> i32;
    fn appletGetOperationMode() -> i32;
    fn appletGetPerformanceMode() -> i32;
    fn apmSetPerformanceConfiguration(mode: i32, configuration: u32) -> u32;
    fn apmGetPerformanceConfiguration(mode: i32, configuration: *mut u32) -> u32;
}

fn describe(configuration: u32) -> String {
    let clocks = CONFIGURATIONS
        .iter()
        .find(|(value, _)| *value == configuration)
        .map_or("clocks unknown", |(_, clocks)| clocks);
    format!("0x{configuration:08x} ({clocks})")
}

fn is_application() -> bool {
    // SAFETY: a plain libnx call; libnx sets up the applet service before `main`.
    unsafe { appletGetAppletType() == APPLET_TYPE_APPLICATION }
}

/// One mode's configuration, as apm reports it.
fn configuration(mode: i32) -> String {
    let mut value = 0;
    // SAFETY: apm is set up for an Application (checked by the callers); `value` is written.
    let rc = unsafe { apmGetPerformanceConfiguration(mode, &mut value) };
    if rc == 0 {
        describe(value)
    } else {
        format!("unknown (apmGetPerformanceConfiguration: 0x{rc:x})")
    }
}

/// Logs the clocks and, with `HORIZON_NX_GPU_CLOCKS=fast` or `max`, asks for faster ones. `init`
/// calls it once, after env.txt is applied and before the CPU boost goes on.
pub(crate) fn start_from_env() {
    if !is_application() {
        write_log_line(format_args!(
            "[Clocks] not an Application, so no apm session: the system's clocks stay\n"
        ));
        return;
    }
    log_clocks("at launch, as the system set them");
    let choice = std::env::var("HORIZON_NX_GPU_CLOCKS").ok();
    let handheld = match choice.as_deref().map(str::trim) {
        None | Some("system") => return,
        Some("fast") => HANDHELD_FAST,
        Some("max") => HANDHELD_MAX,
        Some(other) => {
            write_log_line(format_args!(
                "[Clocks] HORIZON_NX_GPU_CLOCKS={other:?} isn't fast, max or system; the system's clocks stay\n"
            ));
            return;
        }
    };
    for (mode, name, wanted) in [
        (MODE_HANDHELD, "handheld", handheld),
        (MODE_DOCKED, "docked", DOCKED_FAST),
    ] {
        // SAFETY: apm is set up for an Application (checked above).
        let rc = unsafe { apmSetPerformanceConfiguration(mode, wanted) };
        write_log_line(format_args!(
            "[Clocks] {name}: asked for {} (result 0x{rc:x})\n",
            describe(wanted)
        ));
    }
    log_clocks("after asking");
}

/// Logs whether the console is docked and both modes' configurations. From `start_from_env`, and
/// from `process_applet_messages` when the system says the operation or performance mode changed.
pub(crate) fn log_clocks(when: &str) {
    if !is_application() {
        return;
    }
    // SAFETY: plain libnx getters of the applet state it keeps.
    let (operation, performance) =
        unsafe { (appletGetOperationMode(), appletGetPerformanceMode()) };
    let mode = if operation == OPERATION_DOCKED { "docked" } else { "handheld" };
    write_log_line(format_args!(
        "[Clocks] {when}: {mode} (performance mode {performance}); handheld configuration {}, docked {}\n",
        configuration(MODE_HANDHELD),
        configuration(MODE_DOCKED),
    ));
}
