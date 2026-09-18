//! X11 keysyms to Windows virtual keys.
//!
//! RFB carries a key as an X11 keysym, which names a *character* or a named
//! key rather than a place on the keyboard: the client sends 0x41 for "A"
//! whether it reached it with Shift or with Caps Lock. Windows works the
//! other way round, from a virtual key plus the modifier state to whatever
//! the active layout makes of it. So there are two paths: the named keys
//! (Return, F7, the arrows) are a table, and everything else is a character
//! the layout has to be asked about, which is [`crate::input`]'s job.
//!
//! The table is plain integers rather than the `windows` crate's
//! `VIRTUAL_KEY`, so this module compiles and its tests run on every
//! platform. The Windows name of each code is in the comment beside it.

/// A key as Windows names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// The virtual-key code, VK_*.
    pub vk: u16,
    /// Whether the key's scancode carries the 0xE0 prefix. The pairs that
    /// share a virtual key are told apart by it: the arrow block against
    /// the numeric keypad, right Control and right Alt against the left
    /// ones. Send it wrong and an application reading raw scancodes (a
    /// game, a terminal in raw mode) sees the keypad key instead.
    pub extended: bool,
}

const fn plain(vk: u16) -> Key {
    Key { vk, extended: false }
}

const fn ext(vk: u16) -> Key {
    Key { vk, extended: true }
}

/// The modifiers whose state a character press has to work around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    Super,
}

/// Which modifier a keysym holds down, if it is one.
pub fn modifier(keysym: u32) -> Option<Modifier> {
    match keysym {
        0xffe1 | 0xffe2 => Some(Modifier::Shift),   // Shift_L, Shift_R
        0xffe3 | 0xffe4 => Some(Modifier::Control), // Control_L, Control_R
        0xffe9 | 0xffea => Some(Modifier::Alt),     // Alt_L, Alt_R
        0xffe7 | 0xffe8 | 0xffeb | 0xffec => Some(Modifier::Super), // Meta_L/R, Super_L/R
        _ => None,
    }
}

/// The named keys: the 0xff00 block of keysymdef.h, which is the whole of
/// what a keyboard has beyond the characters it types.
pub fn special(keysym: u32) -> Option<Key> {
    // Three runs are consecutive on both sides, so they are arithmetic
    // rather than forty more table lines.
    if (0xffbe..=0xffd5).contains(&keysym) {
        return Some(plain(0x70 + (keysym - 0xffbe) as u16)); // F1..F24 -> VK_F1..VK_F24
    }
    if (0xffb0..=0xffb9).contains(&keysym) {
        return Some(plain(0x60 + (keysym - 0xffb0) as u16)); // KP_0..KP_9 -> VK_NUMPAD0..9
    }
    if (0xff91..=0xff94).contains(&keysym) {
        return Some(plain(0x70 + (keysym - 0xff91) as u16)); // KP_F1..KP_F4 -> VK_F1..VK_F4
    }
    Some(match keysym {
        0xff08 => plain(0x08), // BackSpace -> VK_BACK
        0xff09 => plain(0x09), // Tab -> VK_TAB
        0xff0b => plain(0x0c), // Clear -> VK_CLEAR
        0xff0d => plain(0x0d), // Return -> VK_RETURN
        0xff13 => plain(0x13), // Pause -> VK_PAUSE
        0xff14 => plain(0x91), // Scroll_Lock -> VK_SCROLL
        0xff1b => plain(0x1b), // Escape -> VK_ESCAPE
        0xff50 => ext(0x24),   // Home -> VK_HOME
        0xff51 => ext(0x25),   // Left -> VK_LEFT
        0xff52 => ext(0x26),   // Up -> VK_UP
        0xff53 => ext(0x27),   // Right -> VK_RIGHT
        0xff54 => ext(0x28),   // Down -> VK_DOWN
        0xff55 => ext(0x21),   // Page_Up -> VK_PRIOR
        0xff56 => ext(0x22),   // Page_Down -> VK_NEXT
        0xff57 => ext(0x23),   // End -> VK_END
        0xff58 => ext(0x24),   // Begin -> VK_HOME
        0xff61 => ext(0x2c),   // Print -> VK_SNAPSHOT
        0xff63 => ext(0x2d),   // Insert -> VK_INSERT
        0xff67 => ext(0x5d),   // Menu -> VK_APPS
        0xff7f => ext(0x90),   // Num_Lock -> VK_NUMLOCK
        0xff80 => plain(0x20), // KP_Space -> VK_SPACE
        0xff89 => plain(0x09), // KP_Tab -> VK_TAB
        0xff8d => ext(0x0d),   // KP_Enter -> VK_RETURN, the one on the keypad
        // The keypad's own navigation keys: the same virtual keys as the
        // arrow block, without the prefix that would make them it.
        0xff95 => plain(0x24),          // KP_Home -> VK_HOME
        0xff96 => plain(0x25),          // KP_Left -> VK_LEFT
        0xff97 => plain(0x26),          // KP_Up -> VK_UP
        0xff98 => plain(0x27),          // KP_Right -> VK_RIGHT
        0xff99 => plain(0x28),          // KP_Down -> VK_DOWN
        0xff9a => plain(0x21),          // KP_Page_Up -> VK_PRIOR
        0xff9b => plain(0x22),          // KP_Page_Down -> VK_NEXT
        0xff9c => plain(0x23),          // KP_End -> VK_END
        0xff9d => plain(0x0c),          // KP_Begin -> VK_CLEAR, the 5 in the middle
        0xff9e => plain(0x2d),          // KP_Insert -> VK_INSERT
        0xff9f => plain(0x2e),          // KP_Delete -> VK_DELETE
        0xffaa => plain(0x6a),          // KP_Multiply -> VK_MULTIPLY
        0xffab => plain(0x6b),          // KP_Add -> VK_ADD
        0xffac => plain(0x6c),          // KP_Separator -> VK_SEPARATOR
        0xffad => plain(0x6d),          // KP_Subtract -> VK_SUBTRACT
        0xffae => plain(0x6e),          // KP_Decimal -> VK_DECIMAL
        0xffaf => ext(0x6f),            // KP_Divide -> VK_DIVIDE
        0xffe1 => plain(0xa0),          // Shift_L -> VK_LSHIFT
        0xffe2 => plain(0xa1),          // Shift_R -> VK_RSHIFT, whose scancode is its own already
        0xffe3 => plain(0xa2),          // Control_L -> VK_LCONTROL
        0xffe4 => ext(0xa3),            // Control_R -> VK_RCONTROL
        0xffe5 | 0xffe6 => plain(0x14), // Caps_Lock, Shift_Lock -> VK_CAPITAL
        0xffe7 | 0xffeb => ext(0x5b),   // Meta_L, Super_L -> VK_LWIN
        0xffe8 | 0xffec => ext(0x5c),   // Meta_R, Super_R -> VK_RWIN
        0xffe9 => plain(0xa4),          // Alt_L -> VK_LMENU
        0xffea => ext(0xa5),            // Alt_R -> VK_RMENU, which is AltGr on the layouts that have one
        0xffff => ext(0x2e),            // Delete -> VK_DELETE
        _ => return None,
    })
}

/// The character a keysym stands for, for everything the table does not
/// name. Two ranges carry them: Latin-1 as itself, and the rest of Unicode
/// tagged with 0x01000000.
///
/// Below 0x20 is skipped. Those keysyms would be control characters, and a
/// client sends the named key (0xff08 BackSpace) for every one of them.
pub fn character(keysym: u32) -> Option<char> {
    let code = match keysym {
        0x20..=0xff => keysym,
        _ if keysym & 0xff00_0000 == 0x0100_0000 => keysym & 0x00ff_ffff,
        _ => return None,
    };
    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runs_land_on_the_right_ends() {
        assert_eq!(special(0xffbe), Some(plain(0x70))); // F1
        assert_eq!(special(0xffc9), Some(plain(0x7b))); // F12
        assert_eq!(special(0xffd5), Some(plain(0x87))); // F24
        assert_eq!(special(0xffd6), None); // one past F24 is not a key
        assert_eq!(special(0xffb0), Some(plain(0x60))); // KP_0
        assert_eq!(special(0xffb9), Some(plain(0x69))); // KP_9
        assert_eq!(special(0xff91), Some(plain(0x70))); // KP_F1
        assert_eq!(special(0xff94), Some(plain(0x73))); // KP_F4
    }

    #[test]
    fn the_extended_flag_separates_the_pairs() {
        // The arrow block is extended; the keypad's own copies are not.
        assert_eq!(special(0xff51), Some(ext(0x25)));
        assert_eq!(special(0xff96), Some(plain(0x25)));
        assert_eq!(special(0xff63), Some(ext(0x2d)));
        assert_eq!(special(0xff9e), Some(plain(0x2d)));
        assert_eq!(special(0xff0d), Some(plain(0x0d)));
        assert_eq!(special(0xff8d), Some(ext(0x0d)));
        // Right Control and right Alt are extended, right Shift is not.
        assert_eq!(special(0xffe3), Some(plain(0xa2)));
        assert_eq!(special(0xffe4), Some(ext(0xa3)));
        assert_eq!(special(0xffe2), Some(plain(0xa1)));
        assert_eq!(special(0xffea), Some(ext(0xa5)));
    }

    #[test]
    fn modifiers_are_named_and_are_also_keys() {
        assert_eq!(modifier(0xffe1), Some(Modifier::Shift));
        assert_eq!(modifier(0xffe4), Some(Modifier::Control));
        assert_eq!(modifier(0xffe9), Some(Modifier::Alt));
        assert_eq!(modifier(0xffec), Some(Modifier::Super));
        assert_eq!(modifier(0x41), None);
        // A modifier is tracked and pressed, so the table has to hold it too.
        for sym in [
            0xffe1, 0xffe2, 0xffe3, 0xffe4, 0xffe7, 0xffe8, 0xffe9, 0xffea, 0xffeb, 0xffec,
        ] {
            assert!(
                special(sym).is_some(),
                "{sym:#x} is a modifier with no virtual key"
            );
        }
    }

    #[test]
    fn characters_come_from_two_ranges() {
        assert_eq!(character(0x41), Some('A'));
        assert_eq!(character(0x20), Some(' '));
        assert_eq!(character(0xe9), Some('\u{e9}')); // Latin-1 e acute
        assert_eq!(character(0x0100_0100), Some('\u{100}')); // unicode, tagged
        assert_eq!(character(0x0100_20ac), Some('\u{20ac}')); // the euro sign
        assert_eq!(character(0x0101_f600), Some('\u{1f600}')); // outside the BMP
        assert_eq!(character(0x08), None); // a control code, never sent as one
        assert_eq!(character(0xff0d), None); // Return is a named key
        assert_eq!(character(0x0100_d800), None); // a lone surrogate is not a character
    }

    #[test]
    fn a_named_key_is_never_also_a_character() {
        for sym in 0xff00u32..=0xffff {
            if special(sym).is_some() {
                assert_eq!(character(sym), None, "{sym:#x} is in both paths");
            }
        }
    }
}
