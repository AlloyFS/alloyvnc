//! The client's keys and pointer, injected with XTEST.
//!
//! XTEST puts events into the server at the point a real device would, so
//! every client sees ordinary input and nothing has to be told where it came
//! from. It is a server extension rather than a protocol request, which is
//! why an X server started without it (or with it disabled) cannot be driven
//! at all; the version query at startup is what says so plainly.
//!
//! This holds its own connection. The capture side reads events on the
//! capture thread and this writes from whichever task the session's input
//! lock hands it to, and one connection is not two independent streams: a
//! second one costs a socket and removes the question.

use alloyvnc_screen::{CaptureError, Input};
use std::collections::HashMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keymap::{self, Key};

/// The detail byte of a faked MotionNotify: 0 puts the pointer at the
/// coordinates given, 1 would move it by them.
const ABSOLUTE: u8 = 0;

/// Shift_L and Shift_R, the two keysyms whose state changes what a
/// character press has to do.
const SHIFT_KEYSYMS: [u32; 2] = [0xffe1, 0xffe2];

pub struct X11Input {
    conn: RustConnection,
    root: xproto::Window,
    keys: HashMap<u32, Key>,
    /// A keycode that means Shift, for the keysyms the layout only reaches
    /// with it held.
    shift: Option<u8>,
    /// Whether the client is holding Shift itself.
    held: bool,
    /// The button mask of the last pointer event, so a new one can be read
    /// as the presses and releases between the two.
    buttons: u8,
    /// Whether a refused injection has already been reported.
    reported: bool,
}

impl X11Input {
    /// Open `display`, or `$DISPLAY` when it is `None`.
    pub fn new(display: Option<&str>) -> Result<X11Input, CaptureError> {
        let (conn, screen_num) = x11rb::connect(display)
            .map_err(|e| CaptureError::Failed(format!("connect to the X server for input: {e}")))?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| CaptureError::Failed(format!("no screen {screen_num}")))?
            .root;
        let version = conn
            .xtest_get_version(2, 2)
            .map_err(|e| CaptureError::Failed(format!("the server has no XTEST extension: {e}")))?
            .reply()
            .map_err(|e| CaptureError::Failed(format!("XTEST version: {e}")))?;

        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| CaptureError::Failed(format!("ask for the keyboard mapping: {e}")))?
            .reply()
            .map_err(|e| CaptureError::Failed(format!("the keyboard mapping: {e}")))?;
        let keys = keymap::build(&mapping.keysyms, mapping.keysyms_per_keycode as usize, min);

        // Which physical key is Shift is the keyboard's business, not this
        // crate's: the modifier mapping is eight rows of keycodes, and the
        // first row is whatever the user has bound to Shift.
        let modifiers = conn
            .get_modifier_mapping()
            .map_err(|e| CaptureError::Failed(format!("ask for the modifier mapping: {e}")))?
            .reply()
            .map_err(|e| CaptureError::Failed(format!("the modifier mapping: {e}")))?;
        let per_modifier = modifiers.keycodes.len() / 8;
        let shift = modifiers
            .keycodes
            .iter()
            .take(per_modifier)
            .copied()
            .find(|k| *k != 0);

        tracing::info!(
            xtest = format_args!("{}.{}", version.major_version, version.minor_version),
            keysyms = keys.len(),
            shift = shift.unwrap_or(0),
            "input ready"
        );
        Ok(X11Input {
            conn,
            root,
            keys,
            shift,
            held: false,
            buttons: 0,
            reported: false,
        })
    }

    fn trouble(&mut self, what: &str, e: impl std::fmt::Display) {
        if self.reported {
            tracing::debug!(error = %e, "{what}");
        } else {
            self.reported = true;
            tracing::warn!(error = %e, "{what}; the display may have gone away");
        }
    }

    /// One faked event. Keys and buttons ignore the coordinates; motion
    /// ignores the detail.
    fn fake(&mut self, kind: u8, detail: u8, x: i16, y: i16) {
        // The cookie is dropped rather than checked: the request is already
        // in the connection's buffer and an event nobody waits on has no
        // reply worth a round trip. Dropping it here also ends its borrow
        // of the connection, which the error report needs.
        let sent = self
            .conn
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)
            .map(drop);
        if let Err(e) = sent {
            self.trouble("XTEST refused an event", e);
        }
    }

    fn flush(&mut self) {
        if let Err(e) = self.conn.flush() {
            self.trouble("the X connection would not flush", e);
        }
    }
}

impl Input for X11Input {
    fn key(&mut self, keysym: u32, down: bool) {
        if SHIFT_KEYSYMS.contains(&keysym) {
            self.held = down;
        }
        let Some(&key) = self.keys.get(&keysym) else {
            tracing::debug!(
                keysym = format_args!("{keysym:#x}"),
                "no key on this layout types that"
            );
            return;
        };
        let kind = if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        // Shift only for the press: the character is produced as the key
        // goes down, so Shift can be let go in the same batch, and the
        // release that follows does no harm unshifted. A client holding
        // Shift already sends the shifted keysym, so pressing Shift again
        // would leave it stuck down after the client's own release.
        let wrap = down && key.shift && !self.held;
        if let (true, Some(shift)) = (wrap, self.shift) {
            self.fake(KEY_PRESS_EVENT, shift, 0, 0);
            self.fake(kind, key.code, 0, 0);
            self.fake(KEY_RELEASE_EVENT, shift, 0, 0);
        } else {
            self.fake(kind, key.code, 0, 0);
        }
        self.flush();
    }

    fn pointer(&mut self, x: u16, y: u16, buttons: u8) {
        // The root window's origin is the picture's origin, so a picture
        // coordinate is already a root coordinate; X11 needs none of the
        // virtual-desktop arithmetic Windows does.
        self.fake(MOTION_NOTIFY_EVENT, ABSOLUTE, x as i16, y as i16);

        // Buttons are a level in RFB and an edge in X11, so the two masks
        // are compared and only what changed is sent. The wheel is buttons
        // 4 to 7 on both sides, which is where RFB took the idea from, so
        // there is nothing to translate: a notch is the press and release
        // of button 4 or 5 and it arrives as exactly that.
        let changed = buttons ^ self.buttons;
        for bit in 0..7u8 {
            if changed & (1 << bit) == 0 {
                continue;
            }
            let kind = if buttons & (1 << bit) != 0 {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            };
            self.fake(kind, bit + 1, 0, 0);
        }
        self.buttons = buttons;
        self.flush();
    }
}
