//! The Switch's software keyboard: libnx's `swkbd`, the system's keyboard applet. It takes over
//! the screen until the player confirms or cancels, then hands back the whole text.

use std::ffi::{CString, c_char};

/// libnx's `SwkbdConfig` (`switch/applets/swkbd.h`: 1264 bytes, 8-aligned in the libnx this was
/// built with). Only libnx reads or writes its fields (`swkbdCreate` clears and fills them), so it
/// is an opaque buffer with room to spare for a newer libnx.
#[repr(C, align(16))]
struct SwkbdConfig([u8; 4096]);

unsafe extern "C" {
    fn swkbdCreate(c: *mut SwkbdConfig, max_dictwords: i32) -> u32;
    fn swkbdClose(c: *mut SwkbdConfig);
    fn swkbdConfigMakePresetDefault(c: *mut SwkbdConfig);
    fn swkbdShow(c: *mut SwkbdConfig, out: *mut c_char, out_size: usize) -> u32;
    fn swkbdConfigSetInitialText(c: *mut SwkbdConfig, text: *const c_char);
}

/// The text the keyboard can hand back: 1 KiB of UTF-8, the NUL included.
const TEXT_BYTES: usize = 1024;

/// Shows the system keyboard holding `initial`, the cursor at its end (the default preset's), and
/// blocks the calling thread until the player is done. The whole text they confirmed (maybe
/// empty), or `None` when they cancelled or the keyboard couldn't start.
pub fn software_keyboard(initial: &str) -> Option<String> {
    let initial = CString::new(initial.replace('\0', "")).unwrap_or_default();
    let mut config = Box::new(SwkbdConfig([0; 4096]));
    let config: *mut SwkbdConfig = &mut *config;
    let mut text = [0u8; TEXT_BYTES];
    // SAFETY: `config` is zeroed, aligned and larger than libnx's struct; swkbdCreate initializes
    // it and swkbdClose frees what swkbdCreate allocated. swkbdConfigSetInitialText copies the
    // text into the keyboard's work buffer. swkbdShow writes at most `text.len()` bytes, the NUL
    // included.
    let rc = unsafe {
        if swkbdCreate(config, 0) != 0 {
            return None;
        }
        swkbdConfigMakePresetDefault(config);
        swkbdConfigSetInitialText(config, initial.as_ptr());
        let rc = swkbdShow(config, text.as_mut_ptr().cast(), text.len());
        swkbdClose(config);
        rc
    };
    if rc != 0 {
        return None;
    }
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    Some(String::from_utf8_lossy(&text[..end]).into_owned())
}
