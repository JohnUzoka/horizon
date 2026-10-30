// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! The npad through libnx's pad API (`switch/runtime/pad.h`).
//!
//! One gamepad, id 0: libnx's default pad, which merges player 1 and the handheld Joy-Con, so a
//! Pro Controller, a pair of Joy-Con held together or the console in handheld mode all come
//! through it. Buttons are reported by position (the bottom face button, Nintendo's B, is South; A
//! is East; Y is West; X is North), and the vendor id is Nintendo's, so a game that picks glyphs
//! by vendor shows Nintendo's letters. The sticks run -32767..=32767 with up positive. No SDL
//! mapping applies (the UUID is nil), so gilrs maps these native codes one to one.

use super::FfDevice;
use crate::{AxisInfo, Event, EventType, PlatformError, PowerInfo};
use uuid::Uuid;

use std::collections::VecDeque;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::{Duration, Instant};

/// libnx `HidAnalogStickState`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct StickState {
    x: i32,
    y: i32,
}

/// libnx `PadState` (`switch/runtime/pad.h`); the layout is asserted below so a libnx change that
/// moves it fails the build instead of reading garbage.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
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
    assert!(core::mem::align_of::<PadState>() == 8);
    assert!(core::mem::offset_of!(PadState, attributes) == 8);
    assert!(core::mem::offset_of!(PadState, buttons_cur) == 16);
    assert!(core::mem::offset_of!(PadState, sticks) == 32);
};

extern "C" {
    fn padConfigureInput(max_players: u32, style_set: u32);
    fn padInitializeWithMask(pad: *mut PadState, pad_mask: u64);
    fn padUpdate(pad: *mut PadState);
}

/// libnx `HidNpadStyleSet_NpadStandard`: Pro Controller, handheld, dual Joy-Con, one Joy-Con.
const STYLE_SET_STANDARD: u32 = 0x1F;
/// Player 1 and the handheld controls, libnx `padInitializeDefault`'s mask.
const PAD_MASK: u64 = (1 << 0) | (1 << 32);
/// libnx `HidNpadAttribute_IsConnected`.
const ATTRIBUTE_IS_CONNECTED: u32 = 1 << 0;
/// libnx `HidNpadStyleTag`s that name the controller.
const STYLE_FULL_KEY: u32 = 1 << 0;
const STYLE_HANDHELD: u32 = 1 << 1;
const STYLE_JOY_DUAL: u32 = 1 << 2;
/// Nintendo's USB vendor id, and the Pro Controller's and a Joy-Con pair's product ids.
const NINTENDO: u16 = 0x057e;
const PRO_CONTROLLER: u16 = 0x2009;
const JOY_CON_PAIR: u16 = 0x200e;

/// libnx `HidNpadButton` bits (`switch/services/hid.h`).
mod npad {
    pub const A: u64 = 1 << 0;
    pub const B: u64 = 1 << 1;
    pub const X: u64 = 1 << 2;
    pub const Y: u64 = 1 << 3;
    pub const STICK_L: u64 = 1 << 4;
    pub const STICK_R: u64 = 1 << 5;
    pub const L: u64 = 1 << 6;
    pub const R: u64 = 1 << 7;
    pub const ZL: u64 = 1 << 8;
    pub const ZR: u64 = 1 << 9;
    pub const PLUS: u64 = 1 << 10;
    pub const MINUS: u64 = 1 << 11;
    pub const LEFT: u64 = 1 << 12;
    pub const UP: u64 = 1 << 13;
    pub const RIGHT: u64 = 1 << 14;
    pub const DOWN: u64 = 1 << 15;
}

use self::native_ev_codes as nec;

/// Each npad button and its native code, by position.
const BUTTONS: [(u64, EvCode); 16] = [
    (npad::B, nec::BTN_SOUTH),
    (npad::A, nec::BTN_EAST),
    (npad::Y, nec::BTN_WEST),
    (npad::X, nec::BTN_NORTH),
    (npad::L, nec::BTN_LT),
    (npad::R, nec::BTN_RT),
    (npad::ZL, nec::BTN_LT2),
    (npad::ZR, nec::BTN_RT2),
    (npad::MINUS, nec::BTN_SELECT),
    (npad::PLUS, nec::BTN_START),
    (npad::STICK_L, nec::BTN_LTHUMB),
    (npad::STICK_R, nec::BTN_RTHUMB),
    (npad::UP, nec::BTN_DPAD_UP),
    (npad::DOWN, nec::BTN_DPAD_DOWN),
    (npad::LEFT, nec::BTN_DPAD_LEFT),
    (npad::RIGHT, nec::BTN_DPAD_RIGHT),
];

const BUTTON_CODES: [EvCode; 16] = {
    let mut codes = [nec::BTN_SOUTH; 16];
    let mut i = 0;
    while i < 16 {
        codes[i] = BUTTONS[i].1;
        i += 1;
    }
    codes
};

const AXIS_CODES: [EvCode; 4] = [
    nec::AXIS_LSTICKX,
    nec::AXIS_LSTICKY,
    nec::AXIS_RSTICKX,
    nec::AXIS_RSTICKY,
];

const STICK: AxisInfo = AxisInfo {
    min: -32767,
    max: 32767,
    deadzone: None,
};

#[derive(Debug)]
pub struct Gilrs {
    pad: PadState,
    buttons: u64,
    axes: [i32; 4],
    events: VecDeque<Event>,
    gamepad: Gamepad,
}

impl Gilrs {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        let mut pad = PadState::default();
        // SAFETY: libnx's pad setup; `pad` is a plain struct libnx fills.
        unsafe {
            padConfigureInput(1, STYLE_SET_STANDARD);
            padInitializeWithMask(&mut pad, PAD_MASK);
        }
        Ok(Gilrs {
            pad,
            buttons: 0,
            axes: [0; 4],
            events: VecDeque::new(),
            gamepad: Gamepad::new(),
        })
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        if self.events.is_empty() {
            self.poll();
        }
        self.events.pop_front()
    }

    pub(crate) fn next_event_blocking(&mut self, timeout: Option<Duration>) -> Option<Event> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if let Some(event) = self.next_event() {
                return Some(event);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        (id == 0).then_some(&self.gamepad)
    }

    /// Returns index greater than index of last connected gamepad.
    pub fn last_gamepad_hint(&self) -> usize {
        1
    }

    /// Reads the pad once and queues what changed since the last read.
    fn poll(&mut self) {
        // SAFETY: `self.pad` was set up by padInitializeWithMask; padUpdate only writes it.
        unsafe { padUpdate(&mut self.pad) };
        let connected = self.pad.attributes & ATTRIBUTE_IS_CONNECTED != 0;
        if connected != self.gamepad.connected {
            self.gamepad.update(&self.pad, connected);
            let event = if connected {
                EventType::Connected
            } else {
                EventType::Disconnected
            };
            self.events.push_back(Event::new(0, event));
            if !connected {
                self.buttons = 0;
                self.axes = [0; 4];
            }
        }
        if !connected {
            return;
        }
        let buttons = self.pad.buttons_cur;
        let changed = buttons ^ self.buttons;
        for (bit, code) in BUTTONS {
            if changed & bit != 0 {
                let code = crate::EvCode(code);
                let event = if buttons & bit != 0 {
                    EventType::ButtonPressed(code)
                } else {
                    EventType::ButtonReleased(code)
                };
                self.events.push_back(Event::new(0, event));
            }
        }
        self.buttons = buttons;
        let [left, right] = self.pad.sticks;
        let axes = [left.x, left.y, right.x, right.y];
        for (i, value) in axes.into_iter().enumerate() {
            if value != self.axes[i] {
                let code = crate::EvCode(AXIS_CODES[i]);
                self.events
                    .push_back(Event::new(0, EventType::AxisValueChanged(value, code)));
            }
        }
        self.axes = axes;
    }
}

#[derive(Debug)]
pub struct Gamepad {
    name: &'static str,
    product: Option<u16>,
    connected: bool,
}

impl Gamepad {
    fn new() -> Self {
        Gamepad {
            name: "Nintendo Switch Controller",
            product: None,
            connected: false,
        }
    }

    /// Takes the controller's name and product from the style libnx reports.
    fn update(&mut self, pad: &PadState, connected: bool) {
        self.connected = connected;
        let style = pad.style_set;
        (self.name, self.product) = if style & STYLE_FULL_KEY != 0 {
            ("Nintendo Switch Pro Controller", Some(PRO_CONTROLLER))
        } else if style & (STYLE_HANDHELD | STYLE_JOY_DUAL) != 0 {
            ("Nintendo Switch Joy-Con (L/R)", Some(JOY_CON_PAIR))
        } else {
            ("Nintendo Switch Controller", None)
        };
    }

    pub fn name(&self) -> &str {
        self.name
    }

    pub fn uuid(&self) -> Uuid {
        Uuid::nil()
    }

    pub fn vendor_id(&self) -> Option<u16> {
        Some(NINTENDO)
    }

    pub fn product_id(&self) -> Option<u16> {
        self.product
    }

    pub fn power_info(&self) -> PowerInfo {
        PowerInfo::Unknown
    }

    pub fn is_ff_supported(&self) -> bool {
        true
    }

    /// Creates Ffdevice corresponding to this gamepad.
    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice::new())
    }

    pub fn buttons(&self) -> &[EvCode] {
        &BUTTON_CODES
    }

    pub fn axes(&self) -> &[EvCode] {
        &AXIS_CODES
    }

    pub(crate) fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        AXIS_CODES.contains(&nec).then_some(&STICK)
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }
}

#[cfg_attr(feature = "serde-serialize", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode(u16);

impl EvCode {
    pub fn into_u32(self) -> u32 {
        self.0 as u32
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        self.0.fmt(f)
    }
}

/// The default backend's numbering.
pub mod native_ev_codes {
    use super::EvCode;

    pub const AXIS_LSTICKX: EvCode = EvCode(0);
    pub const AXIS_LSTICKY: EvCode = EvCode(1);
    pub const AXIS_LEFTZ: EvCode = EvCode(2);
    pub const AXIS_RSTICKX: EvCode = EvCode(3);
    pub const AXIS_RSTICKY: EvCode = EvCode(4);
    pub const AXIS_RIGHTZ: EvCode = EvCode(5);
    pub const AXIS_DPADX: EvCode = EvCode(6);
    pub const AXIS_DPADY: EvCode = EvCode(7);
    pub const AXIS_RT: EvCode = EvCode(8);
    pub const AXIS_LT: EvCode = EvCode(9);
    pub const AXIS_RT2: EvCode = EvCode(10);
    pub const AXIS_LT2: EvCode = EvCode(11);

    pub const BTN_SOUTH: EvCode = EvCode(12);
    pub const BTN_EAST: EvCode = EvCode(13);
    pub const BTN_C: EvCode = EvCode(14);
    pub const BTN_NORTH: EvCode = EvCode(15);
    pub const BTN_WEST: EvCode = EvCode(16);
    pub const BTN_Z: EvCode = EvCode(17);
    pub const BTN_LT: EvCode = EvCode(18);
    pub const BTN_RT: EvCode = EvCode(19);
    pub const BTN_LT2: EvCode = EvCode(20);
    pub const BTN_RT2: EvCode = EvCode(21);
    pub const BTN_SELECT: EvCode = EvCode(22);
    pub const BTN_START: EvCode = EvCode(23);
    pub const BTN_MODE: EvCode = EvCode(24);
    pub const BTN_LTHUMB: EvCode = EvCode(25);
    pub const BTN_RTHUMB: EvCode = EvCode(26);

    pub const BTN_DPAD_UP: EvCode = EvCode(27);
    pub const BTN_DPAD_DOWN: EvCode = EvCode(28);
    pub const BTN_DPAD_LEFT: EvCode = EvCode(29);
    pub const BTN_DPAD_RIGHT: EvCode = EvCode(30);
}
