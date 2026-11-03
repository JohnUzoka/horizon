#[cfg(target_os = "horizon")]
mod ffi {
    use core::ffi::{c_char, c_void};

    unsafe extern "C" {
        pub fn romfsMountSelf(name: *const c_char) -> u32;
        pub fn romfsUnmount(name: *const c_char) -> u32;
        pub fn socketInitialize(config: *const c_void) -> u32;
        pub fn socketExit();
        /// Doesn't wait: fails once no message is waiting.
        pub fn appletGetMessage(message: *mut u32) -> u32;
        /// Updates libnx's focus and operation-mode state and calls the applet hooks. False for
        /// `ExitRequest`.
        pub fn appletProcessMessage(message: u32) -> bool;
        /// libnx's `Event` for the applet message queue, signalled while a message is waiting.
        pub fn appletGetMessageEvent() -> *mut c_void;
        pub fn eventWait(event: *mut c_void, timeout_ns: u64) -> u32;
        pub fn appletGetFocusState() -> u32;
        pub fn appletSetFocusHandlingMode(mode: u32) -> u32;
        /// libnx closes nvdrv when the last reference goes (`NX_GENERATE_SERVICE_GUARD`), and
        /// then waits, with no timeout, for its transfer memory to unlock (`_nvCleanup` →
        /// `tmemWaitForPermission`, libnx `nv.c` and `tmem.c`).
        pub fn nvExit();
        /// libnx's `Service` for the nvdrv session (`nv.c`), zeroed while nvdrv is closed.
        pub fn nvGetServiceSession() -> *const Service;
        /// libnx `applet.c`: when 1, `appletExit` (run by `__appExit` after `userAppExit`) has AM
        /// end the process, as libnx always does for an NSO. An application asks with
        /// `ISelfController::Exit`, a library applet with `ExitProcessAndReturn`.
        pub static mut __nx_applet_exit_mode: u32;
        /// libnx `env.h`: whether the homebrew loader takes an NRO to start after this one, and
        /// setting it (copied into the loader's buffers, as hbmenu does for the NRO picked in it).
        pub fn envHasNextLoad() -> bool;
        pub fn envSetNextLoad(path: *const c_char, argv: *const c_char) -> u32;
    }

    /// libnx `Service` (switch/sf/service.h). `session` is 0 while the service is closed.
    #[repr(C)]
    pub struct Service {
        pub session: u32,
        pub own_handle: u32,
        pub object_id: u32,
        pub pointer_buffer_size: u16,
    }
}

/// `MAKERESULT(Module_Libnx, LibnxError_IoError)`, 2345-0010: what `romfsMountSelf` returns
/// when the NRO has no RomFS (libnx `romfs_dev.c`).
#[cfg(target_os = "horizon")]
const LIBNX_IO_ERROR: u32 = 345 | (10 << 9);

/// libnx `AppletFocusState` and `AppletFocusHandlingMode`.
#[cfg(target_os = "horizon")]
mod focus {
    pub const IN_FOCUS: u32 = 1;
    /// A library applet (keyboard, error, ...) is open over the app, which keeps running.
    pub const OUT_OF_FOCUS: u32 = 2;
    /// The HOME menu is open or the console is going to sleep. Only reported in `NoSuspend`.
    pub const BACKGROUND: u32 = 3;
    /// What libnx sets for an application: HOME and sleep suspend it without a message.
    pub const MODE_SUSPEND_HOME_SLEEP: u32 = 0;
    /// HOME and sleep send `FocusStateChanged` (`BACKGROUND`) and leave the app running.
    pub const MODE_NO_SUSPEND: u32 = 1;
    /// Suspends like `SuspendHomeSleep`, but focus changes still send `FocusStateChanged`.
    pub const MODE_SUSPEND_HOME_SLEEP_NOTIFY: u32 = 2;

    pub fn name(state: u32) -> &'static str {
        match state {
            IN_FOCUS => "in focus",
            OUT_OF_FOCUS => "out of focus (a library applet is open)",
            BACKGROUND => "in the background (HOME menu or sleep)",
            _ => "unknown",
        }
    }
}

/// Set once `romfsMountSelf` succeeds, so `release_horizon_services` knows what to undo.
#[cfg(target_os = "horizon")]
static ROMFS_MOUNTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Set once `socketInitialize` succeeds. Sockets lock 2.2 MiB of the heap until `socketExit`
/// and the socket service has let go of it.
#[cfg(target_os = "horizon")]
static SOCKETS_UP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Set once the app is in `NoSuspend`, so `release_horizon_services` puts libnx's mode back.
#[cfg(target_os = "horizon")]
static NO_SUSPEND_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How long the app has spent suspended, in nanoseconds, for the heartbeat's frame rate.
#[cfg(target_os = "horizon")]
static SUSPENDED_NANOS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Set if switching to `SuspendHomeSleepNotify` once gave `InFocus` back without suspending the
/// app; from then on `suspend_until_out_of_background` waits in `NoSuspend` instead.
#[cfg(target_os = "horizon")]
static SUSPEND_UNRELIABLE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// The most hooks `register_release_hook` keeps.
const MAX_RELEASE_HOOKS: usize = 8;
static RELEASE_HOOKS: [core::sync::atomic::AtomicUsize; MAX_RELEASE_HOOKS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_RELEASE_HOOKS];
static RELEASE_HOOK_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Registers `hook` to run when the NRO ends itself, before bevy_horizon closes its own services
/// and checks the heap for the homebrew loader (see `release_horizon_services`).
///
/// Register anything the app opened that keeps heap pages locked until it is closed: a service's
/// transfer memory, such as nvdrv's for the libnx console (`consoleExit`), audren's
/// (`audrenExit`), or anything else built on `tmemCreate`. Hooks run newest first, once each, on
/// the thread that called `exit`. They should be quick and must not panic. Returns `false` when
/// all 8 slots are taken. Does nothing on other platforms.
pub fn register_release_hook(hook: fn()) -> bool {
    use core::sync::atomic::Ordering;
    let slot = RELEASE_HOOK_COUNT.fetch_add(1, Ordering::SeqCst);
    if slot >= MAX_RELEASE_HOOKS {
        RELEASE_HOOK_COUNT.store(MAX_RELEASE_HOOKS, Ordering::SeqCst);
        return false;
    }
    RELEASE_HOOKS[slot].store(hook as usize, Ordering::SeqCst);
    true
}

#[cfg(target_os = "horizon")]
fn run_release_hooks() {
    use core::sync::atomic::Ordering;
    let count = RELEASE_HOOK_COUNT.load(Ordering::SeqCst).min(MAX_RELEASE_HOOKS);
    for slot in (0..count).rev() {
        let hook = RELEASE_HOOKS[slot].swap(0, Ordering::SeqCst);
        if hook != 0 {
            // SAFETY: only `register_release_hook` stores non-zero values here, and they are `fn()`s.
            let hook: fn() = unsafe { core::mem::transmute::<usize, fn()>(hook) };
            hook();
            crate::logging::write_log_line(format_args!("[Exit] release hook {slot}: done"));
        }
    }
}

#[cfg(target_os = "horizon")]
pub fn init_horizon_services() {
    use crate::logging::ResultCode;
    use core::sync::atomic::Ordering;

    unsafe {
        let name = c"romfs".as_ptr();
        let romfs_res = ffi::romfsMountSelf(name);
        if romfs_res == LIBNX_IO_ERROR {
            log::warn!(
                "romfsMountSelf: {} (libnx IoError): this NRO has no RomFS, which is normal unless elf2nro was given --romfsdir",
                ResultCode(romfs_res)
            );
        } else if romfs_res != 0 {
            log::warn!("romfsMountSelf failed: {}", ResultCode(romfs_res));
        } else {
            ROMFS_MOUNTED.store(true, Ordering::SeqCst);
            log::info!("romfs initialized successfully");
        }

        // libnx makes a 2.2 MiB transfer memory for the socket service out of the malloc heap,
        // with no access left for this process. The kernel keeps those pages locked until
        // socketExit, so `release_horizon_services` must run it before the NRO exits.
        // libnx puts an application in `SuspendHomeSleep`, which suspends it for HOME and sleep
        // without ever sending `FocusStateChanged`, so a game can't tell it was left. So does
        // `SuspendHomeSleepNotify`: on hardware the app was suspended for 5 s with no message.
        // In `NoSuspend`, HOME sends the message and leaves the app running; `check_applet_loop`
        // turns it into Bevy's `WindowFocused` and `AppLifecycle` messages, lets the game run
        // its last frames, then has the system suspend it (`suspend_until_out_of_background`).
        // Library applets (an NRO started from the album) can't change the mode.
        let mode_res = ffi::appletSetFocusHandlingMode(focus::MODE_NO_SUSPEND);
        if mode_res == 0 {
            NO_SUSPEND_SET.store(true, Ordering::SeqCst);
            log::info!(
                "[Applet] focus handling: NoSuspend, so HOME sends FocusStateChanged; the app is suspended after its last frame"
            );
        } else {
            log::info!(
                "[Applet] focus handling left as it was ({}): only an application can change it, so focus changes won't be reported",
                ResultCode(mode_res)
            );
        }

        let socket_res = ffi::socketInitialize(core::ptr::null());
        if socket_res != 0 {
            log::warn!("socketInitialize failed: {}", ResultCode(socket_res));
        } else {
            SOCKETS_UP.store(true, Ordering::SeqCst);
            log::info!("BSD socket layer initialized successfully");
        }
    }
}

/// Asks the homebrew loader to start the NRO at `path` (with `path` as its `argv[0]`) once this
/// one ends, instead of going back to hbmenu. False if the loader takes no next NRO
/// (`envHasNextLoad`). Only an exit through the loader starts it: one through AM to HOME
/// ([`release_horizon_services`], when heap pages are still locked) ends the process, and the
/// NRO has to be started again from hbmenu.
#[cfg(target_os = "horizon")]
pub fn relaunch_on_exit(path: &str) -> bool {
    // libnx's argvSetup splits argv at spaces, except inside quotes.
    let argv = if path.contains(' ') { format!("\"{path}\"") } else { path.to_owned() };
    let (Ok(path), Ok(argv)) = (std::ffi::CString::new(path), std::ffi::CString::new(argv)) else {
        return false;
    };
    // SAFETY: libnx copies both strings before returning.
    unsafe { ffi::envHasNextLoad() && ffi::envSetNextLoad(path.as_ptr(), argv.as_ptr()) == 0 }
}

/// Undoes `init_horizon_services` and checks that the homebrew loader can read the next NRO into
/// the malloc heap. Called from libnx's `userAppExit` hook (`compat.rs`), which runs whenever
/// the NRO ends itself: `main` returning, `std::process::exit`, or an abort. Closing the app
/// from HOME doesn't run it: without `appletLockExit`, the system ends the whole process.
///
/// The homebrew loader reuses the heap for the next NRO. sphaira's loader reads the whole heap
/// in one `fsFileRead`, and the kernel rejects that read with 2001-0106 if any page of it is
/// still locked: by a service's transfer memory until the service is closed (`socketExit`,
/// `nvExit`), or by a thread's stack until the thread is joined or, if detached, closed. Bevy's
/// global task pools never join theirs, so this stops them first (`stop_task_pools`), and it
/// closes detached threads once they exit (`close_detached_threads`), bevy_horizon's log writer
/// among them (`stop_log_writer`).
///
/// Runs once and allocates nothing: an abort in here would come back through `userAppExit`.
///
/// If heap pages are still locked at the end, returning to the loader would crash it, so it
/// sets libnx's `__nx_applet_exit_mode` instead: AM then ends the whole process and the Switch
/// goes back to HOME, as it does when any game exits.
#[cfg(target_os = "horizon")]
pub(crate) fn release_horizon_services() {
    use crate::logging::{sync_log, write_log, write_log_line};
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static RELEASED: AtomicBool = AtomicBool::new(false);
    if RELEASED.swap(true, Ordering::SeqCst) {
        return;
    }

    write_log(b"[Exit] userAppExit: main has returned; releasing services before the loader takes the heap back\n");
    // From here on every log line is written before the next step runs. The writer's stack is
    // closed with the other detached threads' below.
    crate::logging::stop_log_writer();
    // The loader runs in this process until the next NRO's libnx sets the mode again.
    if NO_SUSPEND_SET.swap(false, Ordering::SeqCst) {
        let rc = unsafe { ffi::appletSetFocusHandlingMode(focus::MODE_SUSPEND_HOME_SLEEP) };
        write_log_line(format_args!(
            "[Exit] focus handling back to SuspendHomeSleep: {}",
            if rc == 0 { "done" } else { "failed" }
        ));
    }
    let blocked = crate::memory::check_heap_for_exit("before releasing services");
    run_release_hooks();
    let running = close_detached_threads();
    // `socketExit` frees libnx's socket fd table, and libnx's `bsdExit` is not safe against a
    // concurrent socket call: run 10 crashed when the game's detached Heartbeat thread, still
    // inside an HTTPS ping, reached `close` after the table was gone - a data abort at far=0x18
    // in newlib's `_close_r` (nro+0x1aa4fb0: `ldr x2, [table, fd]; ldr x3, [x2, #0x18]` with a
    // NULL entry), under `ureq::Response::do_from_stream`'s drop. The pinger can block for
    // minutes, so waiting it out is not an option. With any thread still running, leave the
    // sockets open instead: their transfer memory keeps 2.2 MiB of the heap locked, the heap
    // check below fails, and the exit goes through AM, which ends the process and lets the
    // system reclaim everything - as for any game.
    if running == 0 && SOCKETS_UP.swap(false, Ordering::SeqCst) {
        unsafe { ffi::socketExit() };
        write_log(b"[Exit] socketExit: done; the socket sessions and our handle to their transfer memory are closed\n");
    } else if SOCKETS_UP.load(Ordering::SeqCst) {
        write_log_line(format_args!(
            "[Exit] socketExit: skipped, {running} detached thread(s) may still be inside a socket call; the sockets stay open and the exit goes through AM\n"
        ));
    }
    if ROMFS_MOUNTED.swap(false, Ordering::SeqCst) {
        unsafe { ffi::romfsUnmount(c"romfs".as_ptr()) };
        write_log(b"[Exit] romfsUnmount: done\n");
    }
    release_nvdrv();
    // The pages stay locked until the service's own process lets go of its mapping too.
    if matches!(blocked, Some(n) if n > 0) {
        const TIMEOUT: Duration = Duration::from_secs(1);
        match crate::memory::wait_for_heap_unlock(TIMEOUT) {
            Some(waited) => write_log_line(format_args!(
                "[Exit] the heap was unlocked {:.2} ms later, once the services had let go of their transfer memory",
                waited.as_secs_f64() * 1000.0
            )),
            None => write_log_line(format_args!(
                "[Exit] the heap was still locked {} ms later: a service kept its transfer memory, or something else still holds heap pages",
                TIMEOUT.as_millis()
            )),
        }
    }
    let still_blocked = crate::memory::check_heap_for_exit("after releasing services");
    match still_blocked {
        Some(0) => {
            write_log(b"[Exit] PASS: nothing in the heap is locked, so the loader can read the next NRO into it\n");
        }
        Some(_) => {
            // SAFETY: only this thread runs now, and libnx reads the flag later on it, in appletExit.
            unsafe { ffi::__nx_applet_exit_mode = 1 };
            write_log(b"[Exit] WARN: heap pages are still locked, so the loader would crash reading the next NRO (2001-0106); exiting through AM to HOME instead\n");
        }
        None => {
            write_log(b"[Exit] heap check failed: svcQueryMemory returned an error; returning to the loader\n");
        }
    }
    sync_log();
}

/// Closes nvdrv if something left it open, unless the GPU driver still uses it.
///
/// On a clean exit the Vulkan driver has closed nvdrv already: Mesa's horizon runtime holds a
/// reference for each device (`nouveau_horizon_runtime_get`/`_put`), and libnx counts references.
/// When the app ends without dropping the device (a panic, for one), Mesa still has GPU memory
/// and a thread, and `nvExit` would close nvdrv under them and then wait for its transfer memory
/// with no timeout: render_check stopped there after a DeviceLost on hardware (run 5; `nvExit:
/// done` never reached log.txt). So while nvmap objects still hold heap pages, this leaves nvdrv
/// open and exits through AM, which ends the process and lets the system clean up, as for any
/// game.
#[cfg(target_os = "horizon")]
fn release_nvdrv() {
    use crate::logging::{write_log, write_log_line};

    // SAFETY: libnx's static Service; nothing else closes it on this thread.
    let open = unsafe { (*ffi::nvGetServiceSession()).session != 0 };
    if !open {
        write_log(b"[Exit] nvdrv: already closed (the GPU driver released it), so there is nothing for nvExit to do\n");
        return;
    }
    match crate::memory::nvmap_heap() {
        Some((0, _)) => {
            write_log(b"[Exit] nvdrv: still open, but no heap pages are held for the GPU; calling nvExit\n");
            unsafe { ffi::nvExit() };
            write_log(b"[Exit] nvExit: done\n");
        }
        mapped => {
            // SAFETY: only this thread runs bevy_horizon's exit, and libnx reads the flag later on
            // it, in appletExit.
            unsafe { ffi::__nx_applet_exit_mode = 1 };
            match mapped {
                Some((blocks, bytes)) => write_log_line(format_args!(
                    "[Exit] nvdrv: still open, and {blocks} heap blocks ({:.2} MiB) are still held for the GPU, so the GPU driver never shut down; skipping nvExit, which can wait forever in this state, and exiting through AM to HOME",
                    bytes as f64 / (1024.0 * 1024.0)
                )),
                None => {
                    write_log(b"[Exit] nvdrv: still open, and svcQueryMemory failed, so whether the GPU driver still holds heap pages is unknown; skipping nvExit, which could wait forever, and exiting through AM to HOME\n");
                }
            }
        }
    }
}

/// Closes the detached threads that have exited, waiting up to 1 s for the rest, and returns how
/// many were still running when it gave up. std detaches a thread whenever its `JoinHandle` is
/// dropped, and until it is closed its stack keeps 2 MiB of the heap locked (`compat.rs`).
/// `blocking`'s threads, which run Bevy's file reads, are detached and exit after 500 ms without
/// work.
#[cfg(target_os = "horizon")]
fn close_detached_threads() -> u32 {
    use crate::logging::write_log_line;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(1);
    let started = Instant::now();
    let Some(threads) = crate::compat::horizon_stubs::close_detached_threads(TIMEOUT) else {
        write_log_line(format_args!(
            "[Exit] detached threads: another thread was closing them for all of {} ms",
            TIMEOUT.as_millis()
        ));
        // Unknown, and the other closer may still be waiting: assume some are running.
        return 1;
    };
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    write_log_line(format_args!(
        "[Exit] detached threads: closed {} now and {} while the app ran ({} threads were created in all); {} still running after {ms:.2} ms",
        threads.closed,
        threads.closed_total - threads.closed,
        threads.created_total,
        threads.running,
    ));
    if threads.close_failed_total + threads.untracked_total > 0 {
        write_log_line(format_args!(
            "[Exit] detached threads: WARN: {} exited but threadClose failed; {} were detached with all 256 slots in use; their stacks stay locked",
            threads.close_failed_total, threads.untracked_total,
        ));
    }
    // Untracked ones (detached while all 256 slots were taken) are never closed, so whether they
    // have exited is unknown: count them as possibly still inside a socket call too.
    threads.running + threads.untracked_total
}

/// Handles every applet message waiting, as `appletMainLoop` does for one. False once the system
/// has asked the app to exit. The game calls it once a frame, so it also ends the loading CPU
/// boost at the player's first input (boost.rs).
#[cfg(target_os = "horizon")]
pub fn process_applet_messages() -> bool {
    crate::boost::end_on_input();
    // The bound only stops a frame from spinning if messages kept coming.
    for _ in 0..16 {
        let mut message = 0;
        if unsafe { ffi::appletGetMessage(&mut message) } != 0 {
            break;
        }
        if !unsafe { ffi::appletProcessMessage(message) } {
            return false;
        }
        // libnx AppletMessage_OperationModeChanged (30) and _PerformanceModeChanged (31): docked
        // or not, so other clocks (clocks.rs).
        if message == 30 || message == 31 {
            crate::clocks::log_clocks("the system changed the mode");
        }
    }
    true
}

/// Has the system suspend the app, which is in the background and has run its last frame, and
/// returns the focus state once it is back; `None` if the system asked it to exit meanwhile.
///
/// In `NoSuspend` the app would keep running in the background, so this switches to
/// `SuspendHomeSleepNotify`, which suspends it there. eden's AM (`ISelfController::
/// SetFocusHandlingMode`) suspends the process before the call returns. Switching also changes
/// the focus state the app is given, so a `FocusStateChanged` message waits for it when it runs
/// again. If the system didn't suspend it, it waits here on the message event instead, doing
/// nothing until it is out of the background. Then it goes back to `NoSuspend`. switchbrew's
/// deko3d sample framework (`CApplication::run`) handles focus the same way.
///
/// In the `SuspendHomeSleep` modes HOME doesn't change the focus state, so if the system ever
/// gave `InFocus` back straight after the switch without suspending the app, that would be the
/// mode change talking, not the user coming back. Then this sets `SUSPEND_UNRELIABLE`, and from
/// then on it only waits, in `NoSuspend`, which reports focus as it is.
#[cfg(target_os = "horizon")]
pub fn suspend_until_out_of_background() -> Option<u32> {
    use crate::logging::ResultCode;
    use core::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let ask_system = !SUSPEND_UNRELIABLE.load(Ordering::Relaxed);
    if ask_system {
        log::info!(
            "[Applet] the last frame before the background has run; switching to SuspendHomeSleepNotify so the system suspends the app"
        );
    } else {
        log::info!(
            "[Applet] the last frame before the background has run; waiting for focus without frames, since the system didn't suspend the app before"
        );
    }
    // If the app is closed from HOME while suspended, nothing else of it runs.
    crate::logging::flush_switch_logs();
    let started = Instant::now();
    let suspend_rc = if ask_system {
        unsafe { ffi::appletSetFocusHandlingMode(focus::MODE_SUSPEND_HOME_SLEEP_NOTIFY) }
    } else {
        0
    };
    let in_call = started.elapsed();
    let mut waits = 0u32;
    let mut exit = false;
    let mut state = unsafe { ffi::appletGetFocusState() };
    while state == focus::BACKGROUND {
        unsafe { ffi::eventWait(ffi::appletGetMessageEvent(), 100_000_000) };
        waits += 1;
        if !process_applet_messages() {
            exit = true;
            break;
        }
        state = unsafe { ffi::appletGetFocusState() };
    }
    let total = started.elapsed();
    SUSPENDED_NANOS.fetch_add(
        u64::try_from(total.as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    if suspend_rc != 0 {
        log::warn!(
            "[Applet] SetFocusHandlingMode(SuspendHomeSleepNotify) failed ({}): the app waited without being suspended",
            ResultCode(suspend_rc)
        );
    }
    if exit {
        log::info!(
            "[Applet] the system asked the app to exit {:.2} s into the background",
            total.as_secs_f64()
        );
        return None;
    }
    if !ask_system {
        log::info!(
            "[Applet] {} again {:.2} s later, after {waits} wait(s) for the focus message",
            focus::name(state),
            total.as_secs_f64(),
        );
        return Some(state);
    }
    let resume_rc = unsafe { ffi::appletSetFocusHandlingMode(focus::MODE_NO_SUSPEND) };
    // Nobody gets back from HOME in a tenth of a second.
    if total < Duration::from_millis(100) {
        SUSPEND_UNRELIABLE.store(true, Ordering::Relaxed);
        log::warn!(
            "[Applet] {} {:.3} s after SetFocusHandlingMode(SuspendHomeSleepNotify), too soon for anyone to have come back from HOME: the system didn't suspend the app. From now on it waits in the background without frames instead",
            focus::name(state),
            total.as_secs_f64(),
        );
    } else {
        log::info!(
            "[Applet] {} again {:.2} s later: {:.2} s inside SetFocusHandlingMode, then {waits} wait(s) for the focus message",
            focus::name(state),
            total.as_secs_f64(),
            in_call.as_secs_f64(),
        );
    }
    if resume_rc != 0 {
        log::warn!(
            "[Applet] couldn't go back to NoSuspend ({}): HOME won't be reported again",
            ResultCode(resume_rc)
        );
    }
    Some(state)
}

/// How often `flush_logs_system` writes the heartbeat, and how often it has `log.txt` synced and
/// closes exited threads. They go by time, not frames, so a game at 1 fps still gets them.
#[cfg(target_os = "horizon")]
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(target_os = "horizon")]
const SYNC_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// How often, in seconds, a heartbeat adds the `[Memory]` line; 0 for never. 60 unless
/// `BEVY_HORIZON_MEMORY_EVERY` sets it (`configure_memory_line_from_env`). Its figures come from
/// newlib's `mallinfo`, which walks every free block with malloc's lock held. Once Skate 3 had
/// loaded its map that took about 41 ms, a frame every time on this thread, and as long a wait
/// for any other thread that allocated meanwhile (sk8 findings 21). In play, that is a hitch
/// once a minute (sk8 findings 33), so a game can turn the line off.
#[cfg(target_os = "horizon")]
static MEMORY_EVERY_SECS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(60);

/// Applies `BEVY_HORIZON_MEMORY_EVERY`: how often, in seconds, the heartbeat adds its `[Memory]`
/// line, `0` for never. Unset keeps 60; a value that isn't a whole number is ignored.
#[cfg(target_os = "horizon")]
pub fn configure_memory_line_from_env() {
    let Ok(value) = std::env::var("BEVY_HORIZON_MEMORY_EVERY") else {
        return;
    };
    match value.trim().parse::<u64>() {
        Ok(secs) => {
            MEMORY_EVERY_SECS.store(secs, core::sync::atomic::Ordering::Relaxed);
            if secs == 0 {
                log::info!("[Heartbeat] no [Memory] line (BEVY_HORIZON_MEMORY_EVERY=0)");
            } else {
                log::info!("[Heartbeat] a [Memory] line every {secs} s (BEVY_HORIZON_MEMORY_EVERY)");
            }
        }
        Err(_) => log::warn!(
            "BEVY_HORIZON_MEMORY_EVERY={value}: not a whole number of seconds; the [Memory] line stays once a minute"
        ),
    }
}

/// Writes the heartbeat, syncs `log.txt` and closes exited detached threads on a time
/// schedule, so a game that draws at 1 fps still gets them. Call it from the game's frame
/// loop; between beats it only reads two clocks.
#[cfg(target_os = "horizon")]
pub fn flush_logs() {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy)]
    struct Beat {
        first: Option<Instant>,
        frame: u64,
        heartbeat: Option<Instant>,
        sync: Option<Instant>,
        memory: Option<Instant>,
        /// wgpu-hal's GPU-work totals and the frame count at the last heartbeat.
        gpu: [u64; 8],
        gpu_frame: u64,
    }

    thread_local! {
        static BEAT: Cell<Beat> = const {
            Cell::new(Beat {
                first: None,
                frame: 0,
                heartbeat: None,
                sync: None,
                memory: None,
                gpu: [0; 8],
                gpu_frame: 0,
            })
        };
    }

    let now = Instant::now();
    BEAT.with(|beat| {
        let mut b = beat.get();
        b.first = Some(b.first.unwrap_or(now));
        b.frame += 1;
        let due = |last: &mut Option<Instant>, every: Duration| {
            let due = last.is_none_or(|at| now.duration_since(at) >= every);
            if due {
                *last = Some(now);
            }
            due
        };
        let heartbeat = due(&mut b.heartbeat, HEARTBEAT_EVERY);
        let sync = due(&mut b.sync, SYNC_EVERY) || heartbeat;
        let threads = if sync {
            crate::compat::horizon_stubs::close_detached_threads(Duration::ZERO)
        } else {
            None
        };
        if heartbeat {
            let suspended = Duration::from_nanos(
                SUSPENDED_NANOS.load(core::sync::atomic::Ordering::Relaxed),
            );
            let secs = b.first.unwrap().elapsed().saturating_sub(suspended).as_secs_f64();
            let fps = if secs > 0.0 { b.frame as f64 / secs } else { 0.0 };
            if suspended.is_zero() {
                log::info!(
                    "[Heartbeat] frame {}, {secs:.1} s after the first frame, {fps:.1} fps on average",
                    b.frame
                );
            } else {
                log::info!(
                    "[Heartbeat] frame {}, {secs:.1} s after the first frame not counting {:.1} s suspended, {fps:.1} fps on average",
                    b.frame,
                    suspended.as_secs_f64()
                );
            }
            // What the game asked the GPU to do per frame since the last beat, to tell geometry
            // that grows over a session from a GPU that only gets slower at the same work, and
            // what a frame waited for: the GPU (GPU-bound), a free image (display-bound), or
            // neither (CPU-bound).
            #[cfg(feature = "vulkan")]
            {
                let work = gpu_work();
                let frames = b.frame - b.gpu_frame;
                if frames > 0 {
                    let per = |i: usize| work[i].saturating_sub(b.gpu[i]) as f64 / frames as f64;
                    log::info!(
                        "[GPU work] per frame over the last {frames} frames: {:.1} render passes, {:.0} draws of {:.0}k vertices, {:.1} indirect calls holding {:.0} draws, {:.0} KiB copied; acquire waited {:.1} ms for the GPU and {:.1} ms for an image",
                        per(0),
                        per(1),
                        per(2) / 1000.0,
                        per(3),
                        per(4),
                        per(5) / 1024.0,
                        per(6) / 1e6,
                        per(7) / 1e6,
                    );
                    if let Some(passes) = gpu_passes(frames) {
                        log::info!("[GPU passes] {passes}");
                    }
                }
                b.gpu = work;
                b.gpu_frame = b.frame;
            }
            let memory_every = MEMORY_EVERY_SECS.load(core::sync::atomic::Ordering::Relaxed);
            if memory_every > 0 && due(&mut b.memory, Duration::from_secs(memory_every)) {
                log::info!("[Memory] {}", crate::memory::malloc_report());
            }
            if let Some(threads) = threads {
                log::info!(
                    "[Threads] {} created since launch; detached: {} closed after exiting, {} still running",
                    threads.created_total,
                    threads.closed_total,
                    threads.running,
                );
            }
        }
        if sync {
            crate::logging::request_log_sync();
        }
        beat.set(b);
    });
}

/// wgpu-hal's GPU-work totals (`third_party/wgpu-hal/src/vulkan/horizon_counters.rs`, Horizon
/// builds): render passes, direct draws, their vertices × instances, indirect calls, the draws
/// those hold, bytes copied between buffers, and the nanoseconds `acquire` waited for the GPU and
/// for a free image.
#[cfg(all(target_os = "horizon", feature = "vulkan"))]
fn gpu_work() -> [u64; 8] {
    unsafe extern "C" {
        static HORIZON_GPU_WORK: [core::sync::atomic::AtomicU64; 8];
    }
    // SAFETY: wgpu-hal defines the eight atomics for the process lifetime; relaxed loads only.
    core::array::from_fn(|i| unsafe {
        HORIZON_GPU_WORK[i].load(core::sync::atomic::Ordering::Relaxed)
    })
}

/// wgpu-hal's per-pass GPU times since the last call, per frame over `frames` frames
/// (`third_party/wgpu-hal/src/vulkan/horizon_pass_times.rs`), when `BH_GPU_PASS_TIMES=1`.
#[cfg(all(target_os = "horizon", feature = "vulkan"))]
fn gpu_passes(frames: u64) -> Option<String> {
    unsafe extern "C" {
        fn horizon_gpu_pass_report(buf: *mut u8, cap: usize, frames: u64) -> usize;
    }
    let mut buf = vec![0u8; 2048];
    // SAFETY: wgpu-hal writes at most `buf.len()` bytes into `buf` and returns how many.
    let n = unsafe { horizon_gpu_pass_report(buf.as_mut_ptr(), buf.len(), frames) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}
