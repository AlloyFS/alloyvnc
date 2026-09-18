//! The client's keys and pointer, injected with SendInput.
//!
//! SendInput puts events at the bottom of the same queue a real keyboard
//! and mouse feed, so every application sees them as ordinary input with no
//! idea where they came from. It is what every Windows remote-desktop
//! server uses, and it carries one limit worth knowing: a window running at
//! a higher integrity level than this process ignores injected input (UIPI,
//! the reason an elevated application is deaf to an unelevated server). The
//! service mode of a later phase is what fixes that.
//!
//! Every batch goes in one SendInput call. The call is atomic against other
//! processes injecting input, so a shifted character cannot have somebody
//! else's keystroke land between the Shift and the key.

use std::mem::size_of;

use alloyvnc_screen::Input;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
    VkKeyScanW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::keysym::{self, Modifier};

/// One wheel notch, as Windows counts them.
const NOTCH: i32 = 120;

/// VK_LSHIFT, for the presses a character needs and the client did not ask for.
const VK_LSHIFT: u16 = 0xa0;

fn bit(m: Modifier) -> u8 {
    match m {
        Modifier::Shift => 1,
        Modifier::Control => 2,
        Modifier::Alt => 4,
        Modifier::Super => 8,
    }
}

pub struct WinInput {
    /// The picture's top-left corner in desktop coordinates. Picture
    /// coordinates start at zero; the desktop's do not, because a monitor
    /// placed above or to the left of the primary one has negative ones.
    origin: (i32, i32),
    /// The modifiers the client is holding, as [`bit`] packs them.
    mods: u8,
    /// The button mask of the last pointer event, so a new one can be read
    /// as the presses and releases between the two.
    buttons: u8,
    /// Whether a refused injection has already been reported, so a blocked
    /// window costs one line rather than one per keystroke.
    reported: bool,
}

impl WinInput {
    /// `origin` is the picture's top-left corner in desktop coordinates:
    /// the top-left of the DXGI outputs' union.
    pub fn new(origin: (i32, i32)) -> WinInput {
        WinInput {
            origin,
            mods: 0,
            buttons: 0,
            reported: false,
        }
    }

    fn send(&mut self, events: &[INPUT]) {
        if events.is_empty() {
            return;
        }
        // SAFETY: SendInput reads `events.len()` structures of the size it
        // is told, from a slice that outlives the call. The size argument
        // is what tells the kernel which ABI the caller was built against,
        // so it has to be this INPUT's own size and not a constant.
        let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
        if sent as usize != events.len() {
            if !self.reported {
                self.reported = true;
                tracing::warn!(
                    sent,
                    wanted = events.len(),
                    "input refused; a window at a higher integrity level blocks injection"
                );
            } else {
                tracing::debug!(sent, wanted = events.len(), "input refused");
            }
        }
    }

    /// One key event for a virtual key, with the scancode the active layout
    /// gives it. Applications that read the scancode rather than the
    /// virtual key (games, a terminal in raw mode) get nothing without it.
    fn key_event(vk: u16, extended: bool, down: bool) -> INPUT {
        // SAFETY: a pure lookup against the calling thread's layout, no
        // pointers involved.
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// One key event carrying a UTF-16 unit rather than a key: the layout is
    /// bypassed and the character is delivered as typed. The route for
    /// anything the layout cannot reach, and for everything outside the BMP,
    /// which arrives as its two surrogates in a row.
    fn unicode_event(unit: u16, down: bool) -> INPUT {
        let mut flags = KEYEVENTF_UNICODE;
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn mouse_event(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    // A signed delta in an unsigned field: a wheel notch
                    // backwards is 0xffffff88, not a large number.
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// A character, through the active layout where it can go and through
    /// the unicode path where it cannot.
    fn character(&mut self, ch: char, down: bool) {
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        if units.len() == 1 {
            // SAFETY: a lookup of one UTF-16 unit against the calling
            // thread's layout; -1 means the layout cannot type it.
            let scan = unsafe { VkKeyScanW(units[0]) };
            if scan != -1 {
                let vk = (scan & 0xff) as u16;
                // The high byte is the modifier state the layout wants:
                // bit 0 Shift, bit 1 Control, bit 2 Alt. Control or Alt
                // means AltGr, and holding AltGr down with SendInput is
                // layout-specific guesswork, so those go the unicode way.
                let wants = (scan >> 8) & 0xff;
                if wants & 0b110 == 0 {
                    let event = Self::key_event(vk, false, down);
                    let needs_shift = wants & 1 != 0;
                    if down && needs_shift && self.mods & bit(Modifier::Shift) == 0 {
                        // The character is produced by the key going down,
                        // so Shift can be let go in the same batch. The
                        // release that follows arrives unshifted and does
                        // no harm. A client holding Shift already sends the
                        // shifted keysym, which is why its own state is
                        // asked first: pressing Shift twice would leave it
                        // stuck down after the first release.
                        self.send(&[
                            Self::key_event(VK_LSHIFT, false, true),
                            event,
                            Self::key_event(VK_LSHIFT, false, false),
                        ]);
                    } else {
                        self.send(&[event]);
                    }
                    return;
                }
            }
        }
        let events: Vec<INPUT> = units.iter().map(|u| Self::unicode_event(*u, down)).collect();
        self.send(&events);
    }
}

/// A picture coordinate as SendInput wants it: 0 to 65535 across the whole
/// virtual desktop, whatever part of it the picture covers.
fn absolute(pos: i32, min: i32, size: i32) -> i32 {
    // One short of the size: 65535 has to land on the last pixel rather
    // than one past it. A one-pixel desktop would divide by zero.
    let span = (size - 1).max(1) as i64;
    ((pos - min) as i64 * 65535 / span) as i32
}

impl Input for WinInput {
    fn key(&mut self, keysym: u32, down: bool) {
        if let Some(m) = keysym::modifier(keysym) {
            if down {
                self.mods |= bit(m);
            } else {
                self.mods &= !bit(m);
            }
        }
        if let Some(key) = keysym::special(keysym) {
            let event = Self::key_event(key.vk, key.extended, down);
            self.send(&[event]);
            return;
        }
        match keysym::character(keysym) {
            Some(ch) => self.character(ch, down),
            None => tracing::debug!(keysym = format_args!("{keysym:#x}"), "no key for this keysym"),
        }
    }

    fn pointer(&mut self, x: u16, y: u16, buttons: u8) {
        // Read every time rather than at startup: a monitor plugged in or a
        // resolution change moves the virtual desktop under us, and these
        // are cached reads inside user32.
        //
        // SAFETY: GetSystemMetrics takes an index and returns an integer.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let mut events = Vec::with_capacity(4);
        events.push(Self::mouse_event(
            absolute(x as i32 + self.origin.0, vx, vw),
            absolute(y as i32 + self.origin.1, vy, vh),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ));

        // Buttons are a level in RFB and an edge in Windows, so the two
        // masks are compared and only what changed is sent.
        let changed = buttons ^ self.buttons;
        let pairs = [
            (0u8, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            (1, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        ];
        for (b, press, release) in pairs {
            if changed & (1 << b) != 0 {
                let flags = if buttons & (1 << b) != 0 { press } else { release };
                events.push(Self::mouse_event(0, 0, 0, flags));
            }
        }

        // The wheel is a button in RFB: a notch is a press and a release of
        // button 4 or 5, so only the press edge turns into a notch. Buttons
        // 6 and 7 are the horizontal wheel; positive is to the right on
        // Windows, and button 6 is the one that scrolls left.
        let wheels = [
            (3u8, NOTCH, MOUSEEVENTF_WHEEL),
            (4, -NOTCH, MOUSEEVENTF_WHEEL),
            (5, -NOTCH, MOUSEEVENTF_HWHEEL),
            (6, NOTCH, MOUSEEVENTF_HWHEEL),
        ];
        for (b, delta, flags) in wheels {
            if changed & buttons & (1 << b) != 0 {
                events.push(Self::mouse_event(0, 0, delta, flags));
            }
        }

        self.buttons = buttons;
        self.send(&events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_spans_the_whole_desktop() {
        assert_eq!(absolute(0, 0, 1920), 0);
        assert_eq!(absolute(1919, 0, 1920), 65535);
        // 65535 units over 1919 steps is 34 per pixel, so the pixel after
        // the middle lands just past halfway rather than exactly on it.
        assert_eq!(absolute(960, 0, 1920), 32784);
        // A monitor to the left of the primary one puts the origin negative.
        assert_eq!(absolute(-1920, -1920, 3840), 0);
        assert_eq!(absolute(1919, -1920, 3840), 65535);
        // A degenerate desktop divides by one rather than by zero.
        assert_eq!(absolute(0, 0, 1), 0);
    }
}
