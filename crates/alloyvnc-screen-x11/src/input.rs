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
use std::collections::{HashMap, VecDeque};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
// For `sync`, which is what makes a lent keycode's mapping reach the server
// in front of the key event that depends on it.
use x11rb::wrapper::ConnectionExt as _;

use crate::keymap::{self, Key, Shift, shift_for};

/// The detail byte of a faked MotionNotify: 0 puts the pointer at the
/// coordinates given, 1 would move it by them.
const ABSOLUTE: u8 = 0;

/// Shift_L and Shift_R, the two keysyms whose state changes what a
/// character press has to do.
const SHIFT_KEYSYMS: [u32; 2] = [0xffe1, 0xffe2];

/// NoSymbol: what a keycode is set to when it is given back.
const NO_SYMBOL: u32 = 0;

pub struct X11Input {
    conn: RustConnection,
    root: xproto::Window,
    keys: HashMap<u32, Key>,
    /// A keycode that means Shift, for the keysyms the layout only reaches
    /// with it held.
    shift: Option<u8>,
    /// Whether the client is holding Shift itself.
    held: bool,
    /// How many keysyms each keycode carries, which
    /// [`xproto::ConnectionExt::change_keyboard_mapping`] insists on
    /// matching exactly.
    per_keycode: u8,
    /// Keycodes that type nothing on this layout, kept to lend to a keysym
    /// the layout has no key for at all.
    spares: Vec<u8>,
    /// Which keycode each borrowed keysym is bound to, so a key pressed and
    /// released lands on the same keycode both times.
    lent: HashMap<u32, u8>,
    /// The order they were lent in, so the least recently taken is the one
    /// given back when the spares run out.
    order: VecDeque<u32>,
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
        let per_keycode = mapping.keysyms_per_keycode;
        let keys = keymap::build(&mapping.keysyms, per_keycode as usize, min);

        // A keycode whose every level is NoSymbol types nothing, so binding
        // a keysym to it takes nothing away from whoever is at the desk.
        // Every layout leaves a good number free; x11vnc does the same
        // thing, and it is the only way to type a keysym the layout has no
        // key for at all.
        let spares = keymap::spares(&mapping.keysyms, per_keycode as usize, min);

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
            spare_keycodes = spares.len(),
            "input ready"
        );
        Ok(X11Input {
            conn,
            root,
            keys,
            shift,
            held: false,
            per_keycode,
            spares,
            lent: HashMap::new(),
            order: VecDeque::new(),
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

    /// A keycode for a keysym this layout has no key for, bound on the
    /// spot.
    ///
    /// The cost is real and worth stating: changing the mapping sends every
    /// client on the display a MappingNotify, and a client that has not
    /// processed it yet reads the keycode as whatever it used to be. The
    /// sync below makes the server itself see the mapping before the key
    /// event, which is as far as this can be pushed from outside; x11vnc
    /// has the same hole and lives with it. The alternative is that a
    /// keysym off the layout cannot be typed at all.
    fn lend(&mut self, keysym: u32) -> Option<u8> {
        if let Some(&code) = self.lent.get(&keysym) {
            // Already on loan. Move it to the back so the ones in use are
            // the last to be taken away.
            self.order.retain(|&k| k != keysym);
            self.order.push_back(keysym);
            return Some(code);
        }
        let code = match self.spares.pop() {
            Some(code) => code,
            None => {
                // Every spare is out. The oldest goes back: a keysym
                // nothing has pressed for the longest is the safest to
                // take, and a key still held down is at the other end.
                let oldest = self.order.pop_front()?;
                self.lent.remove(&oldest)?
            }
        };
        // Both levels, so the keysym comes out whether or not the client
        // happens to be holding Shift, and NoSymbol above them. Filling
        // every level was tried and the server does not keep it: a modern
        // X server maps the core request onto XKB, which holds four
        // symbols per key at most, and anything past that reads back as
        // NoSymbol.
        let mut filled = vec![0u32; usize::from(self.per_keycode)];
        for level in filled.iter_mut().take(2) {
            *level = keysym;
        }
        let bound = self
            .conn
            .change_keyboard_mapping(1, code, self.per_keycode, &filled)
            .map(drop);
        if let Err(e) = bound {
            self.trouble("could not lend a keycode", e);
            self.spares.push(code);
            return None;
        }
        // The press must not reach the server in front of the mapping that
        // gives it its meaning, or it types whatever the keycode meant a
        // moment ago. A round trip is the guarantee.
        if let Err(e) = self.conn.sync() {
            self.trouble("the X connection would not sync", e);
        }
        tracing::debug!(keysym = format_args!("{keysym:#x}"), code, "lent a spare keycode");
        self.lent.insert(keysym, code);
        self.order.push_back(keysym);
        Some(code)
    }

    fn flush(&mut self) {
        if let Err(e) = self.conn.flush() {
            self.trouble("the X connection would not flush", e);
        }
    }
}

impl Drop for X11Input {
    fn drop(&mut self) {
        // Give the borrowed keycodes back. Left bound, they would keep
        // typing a keysym for the rest of the X session, on a key that
        // typed nothing before this process ran.
        for &code in self.lent.values() {
            // Checked rather than flushed. A flush says the bytes left this
            // process, not that the server acted on them, and the next
            // thing that happens here is the connection closing: the
            // request can still be in flight when it does, and the keycode
            // is left bound for the rest of the X session. Intermittent
            // exactly as that implies, which is how it was found.
            if let Ok(cookie) = self.conn.change_keyboard_mapping(1, code, 1, &[NO_SYMBOL]) {
                let _ = cookie.check();
            }
        }
    }
}

impl Input for X11Input {
    fn key(&mut self, keysym: u32, down: bool) {
        if SHIFT_KEYSYMS.contains(&keysym) {
            self.held = down;
        }
        let key = match self.keys.get(&keysym) {
            Some(&key) => key,
            // Nothing on this layout types it. A keycode that types nothing
            // at all can be lent one, which is how a client on a different
            // layout types anything its server's layout does not have.
            None => {
                // A release of a key never pressed has nothing to land on,
                // and binding a spare for one would leak the loan.
                let code = if down {
                    self.lend(keysym)
                } else {
                    self.lent.get(&keysym).copied()
                };
                match code {
                    Some(code) => Key { code, shift: false },
                    None => {
                        tracing::debug!(
                            keysym = format_args!("{keysym:#x}"),
                            "no key on this layout types that, and no spare to lend"
                        );
                        return;
                    }
                }
            }
        };
        let kind = if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        // Only around the press: the character is produced as the key goes
        // down, so Shift can be put back in the same batch, and the release
        // that follows does no harm with Shift where the client left it.
        // The client's own state is asked first, or a client genuinely
        // holding Shift would have it pressed twice and left stuck down
        // after one release.
        match (down, self.shift, shift_for(key.shift, self.held)) {
            (true, Some(shift), Shift::Press) => {
                self.fake(KEY_PRESS_EVENT, shift, 0, 0);
                self.fake(kind, key.code, 0, 0);
                self.fake(KEY_RELEASE_EVENT, shift, 0, 0);
            }
            (true, Some(shift), Shift::Release) => {
                self.fake(KEY_RELEASE_EVENT, shift, 0, 0);
                self.fake(kind, key.code, 0, 0);
                self.fake(KEY_PRESS_EVENT, shift, 0, 0);
            }
            _ => self.fake(kind, key.code, 0, 0),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A keysym the layout has no key for is typed anyway, on a keycode
    /// borrowed for it and given back afterwards.
    ///
    /// The check mark is not on any keyboard layout, so it is the case the
    /// lending exists for: without it, a client that copies one into a
    /// remote text field gets nothing at all.
    ///
    /// Ignored by default: it needs a display, and it changes the keyboard
    /// mapping on it for the length of the test. With one:
    /// `cargo test -p alloyvnc-screen-x11 --release -- --ignored`
    #[test]
    #[ignore = "needs an X server, and changes its keyboard mapping"]
    fn a_keysym_off_the_layout_borrows_a_keycode() {
        // The Unicode keysym for U+2713, which is how RFB carries anything
        // the old keysym tables never named.
        let keysym = 0x0100_2713;

        // A run killed before its Drop ran leaves the keycode bound, and
        // every run after it would then fail here for the wrong reason:
        // the keysym is on the layout, so nothing needs lending. Clearing
        // first is what makes this test survive its own accidents.
        clear_leftovers(keysym);

        let mut input = X11Input::new(None).expect("an X server on $DISPLAY");
        assert!(!input.spares.is_empty(), "this layout has no free keycodes");
        assert!(
            !input.keys.contains_key(&keysym),
            "a layout that types a check mark would not need lending"
        );

        input.key(keysym, true);
        let code = *input.lent.get(&keysym).expect("a keycode was lent");
        let levels = bound(&input.conn, code);
        assert_eq!(&levels[..2], [keysym, keysym], "both levels type it");
        input.key(keysym, false);
        assert_eq!(input.lent.get(&keysym), Some(&code), "and it stays lent");

        // Asked for again, it is the same keycode: a press and its release
        // landing on different keys would leave one of them stuck down.
        input.key(keysym, true);
        assert_eq!(input.lent.get(&keysym), Some(&code));
        input.key(keysym, false);

        // And the desk gets its keyboard back.
        drop(input);
        let (conn, _) = x11rb::connect(None).expect("an X server");
        assert!(
            bound(&conn, code).iter().all(|&sym| sym == 0),
            "the borrowed keycode types nothing again"
        );
    }

    /// Blank every keycode that types nothing but `keysym`, which can only
    /// be a keycode this test lent and never got back.
    fn clear_leftovers(keysym: u32) {
        let (conn, _) = x11rb::connect(None).expect("an X server on $DISPLAY");
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min, max - min + 1)
            .expect("ask for the mapping")
            .reply()
            .expect("the mapping");
        let per = mapping.keysyms_per_keycode as usize;
        for (i, level) in mapping.keysyms.chunks(per).enumerate() {
            let only_ours =
                level.contains(&keysym) && level.iter().all(|&sym| sym == keysym || sym == NO_SYMBOL);
            if !only_ours {
                continue;
            }
            let code = u8::try_from(usize::from(min) + i).expect("a keycode");
            conn.change_keyboard_mapping(1, code, 1, &[NO_SYMBOL])
                .expect("ask")
                .check()
                .expect("blank the keycode");
        }
        conn.flush().expect("flush");
    }

    /// What one keycode currently types, every level of it.
    fn bound(conn: &RustConnection, code: u8) -> Vec<u32> {
        conn.get_keyboard_mapping(code, 1)
            .expect("ask for the mapping")
            .reply()
            .expect("the mapping")
            .keysyms
    }
}
