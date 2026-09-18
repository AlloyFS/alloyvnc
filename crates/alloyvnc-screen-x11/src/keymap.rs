//! The server's keyboard mapping, turned round.
//!
//! RFB already speaks X11 keysyms, so unlike Windows there is nothing to
//! translate: the client sends 0x41 and the server has a keysym 0x41
//! somewhere on the keyboard. What is needed is the other direction, because
//! XTEST injects a *keycode*, the number of a physical key, and the keysym
//! is what that key produces under some set of modifiers.
//!
//! GetKeyboardMapping answers with a flat list: `keysyms_per_keycode` entries
//! for the lowest keycode, then the same for the next, up to the highest.
//! Within one keycode the entries are a list of groups of two, and the first
//! two are the pair this cares about: what the key types on its own, and
//! what it types with Shift.
//!
//! Pure, so it is tested on every platform.

use std::collections::HashMap;

/// Where a keysym lives on the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// The keycode XTEST injects.
    pub code: u8,
    /// Whether the key needs Shift held to produce the keysym.
    pub shift: bool,
}

/// NoSymbol: the keycode types nothing in that position.
const NONE: u32 = 0;

/// What Shift has to do around one keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    /// Nothing: what the client is holding is what the key wants.
    Leave,
    /// Press it for the keystroke and let go after. The keysym is only on
    /// the shifted level of its key and the client is not holding Shift,
    /// which is every capital from a client that sends the shifted keysym
    /// without the key.
    Press,
    /// Let go for the keystroke and press it again after. The keysym is on
    /// the unshifted level and the client is holding Shift, so the key on
    /// its own would give the shifted keysym instead: a client holding
    /// Shift and sending `1` would type `!`.
    Release,
}

pub fn shift_for(wants_shift: bool, held: bool) -> Shift {
    match (wants_shift, held) {
        (true, false) => Shift::Press,
        (false, true) => Shift::Release,
        _ => Shift::Leave,
    }
}

/// The keycodes this layout leaves free.
///
/// A keycode whose every level is NoSymbol types nothing, so binding a
/// keysym to it takes nothing away from whoever is at the desk. They are
/// what [`X11Input`] lends to a keysym the layout has no key for at all.
///
/// [`X11Input`]: crate::X11Input
pub fn spares(keysyms: &[u32], per_keycode: usize, min_keycode: u8) -> Vec<u8> {
    if per_keycode == 0 {
        return Vec::new();
    }
    keysyms
        .chunks(per_keycode)
        .enumerate()
        .filter(|(_, level)| level.iter().all(|&sym| sym == NONE))
        .filter_map(|(i, _)| u8::try_from(usize::from(min_keycode) + i).ok())
        .collect()
}

/// Build the keysym-to-key map from a GetKeyboardMapping reply.
///
/// The unshifted column is walked before the shifted one, so a keysym a
/// layout offers both ways is reached without pressing Shift. Anything past
/// the first two columns is another group (AltGr and the like) and is left
/// alone: switching group with XTEST means driving the group latch, which is
/// layout-specific in a way this does not need to be yet.
pub fn build(keysyms: &[u32], per_keycode: usize, min_keycode: u8) -> HashMap<u32, Key> {
    let mut map = HashMap::new();
    if per_keycode == 0 {
        return map;
    }
    let count = keysyms.len() / per_keycode;
    for shift in [false, true] {
        let column = usize::from(shift);
        if column >= per_keycode {
            break;
        }
        for i in 0..count {
            let Some(code) = min_keycode.checked_add(i as u8) else {
                break;
            };
            let keysym = keysyms[i * per_keycode + column];
            if keysym == NONE {
                continue;
            }
            map.entry(keysym).or_insert(Key { code, shift });
        }
    }

    // X11's case rule. A keycode whose shifted entry is NoSymbol and whose
    // unshifted entry is a lowercase letter is defined to produce the
    // uppercase letter with Shift, and a server is allowed to leave that
    // implied rather than list it. Without this an "A" from the client would
    // find nothing on a keyboard that plainly has one.
    for i in 0..count {
        if per_keycode > 1 && keysyms[i * per_keycode + 1] != NONE {
            continue;
        }
        let Some(code) = min_keycode.checked_add(i as u8) else {
            break;
        };
        let lower = keysyms[i * per_keycode];
        if let Some(upper) = upper_case(lower) {
            map.entry(upper).or_insert(Key { code, shift: true });
        }
    }
    map
}

/// The uppercase of a keysym, for the ranges where the case pair is the
/// Latin-1 one: ASCII letters, and the accented letters of Latin-1 with the
/// two that have no pair left out.
fn upper_case(keysym: u32) -> Option<u32> {
    match keysym {
        0x61..=0x7a => Some(keysym - 0x20),               // a to z
        0xe0..=0xf6 | 0xf8..=0xfe => Some(keysym - 0x20), // agrave to thorn, minus division
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four cases, of which one was missing: a client holding Shift and
    /// asking for a keysym that sits on the unshifted level.
    #[test]
    fn shift_is_pressed_or_let_go_of_to_match_the_keysym() {
        // A capital from a client that sends the shifted keysym without
        // holding the key.
        assert_eq!(shift_for(true, false), Shift::Press);
        // A digit from a client that is holding Shift for its own reasons.
        // Without this the key gives the shifted keysym: `1` types `!`.
        assert_eq!(shift_for(false, true), Shift::Release);
        // And the two that need nothing.
        assert_eq!(shift_for(true, true), Shift::Leave);
        assert_eq!(shift_for(false, false), Shift::Leave);
    }

    /// A keycode is spare only when every level of it is NoSymbol. One
    /// symbol anywhere on the key means somebody at the desk types with it.
    #[test]
    fn spare_keycodes_are_the_ones_that_type_nothing() {
        // Four keycodes from 8, two symbols each: a, A / nothing / nothing
        // but only on the second level / z, Z.
        let keysyms = [
            0x61, 0x41, // 8: a A
            0, 0, // 9: spare
            0, 0x5f, // 10: types an underscore with Shift, so not spare
            0x7a, 0x5a, // 11: z Z
        ];
        assert_eq!(spares(&keysyms, 2, 8), [9]);

        // A layout that reports no symbols per keycode has no spares rather
        // than every keycode: dividing by it would panic.
        assert!(spares(&keysyms, 0, 8).is_empty());

        // The whole keyboard free, which is what an empty mapping looks
        // like, and the keycodes come back in order from the minimum.
        assert_eq!(spares(&[0; 8], 2, 100), [100, 101, 102, 103]);

        // A keycode past 255 cannot be named in the protocol, so a mapping
        // that would run off the end stops rather than wrapping.
        assert_eq!(spares(&[0; 8], 2, 254), [254, 255]);
    }

    /// Where a keysym landed, as a pair, so an assertion is one line.
    fn at(map: &HashMap<u32, Key>, keysym: u32) -> Option<(u8, bool)> {
        map.get(&keysym).map(|k| (k.code, k.shift))
    }

    #[test]
    fn the_unshifted_column_wins() {
        // Two keycodes from 8: the first types 'a' and 'A', the second
        // types 'A' on its own.
        let map = build(&[0x61, 0x41, 0x41, NONE], 2, 8);
        assert_eq!(at(&map, 0x61), Some((8, false)));
        // 'A' is on both, and the one that needs no Shift is preferred.
        assert_eq!(at(&map, 0x41), Some((9, false)));
    }

    #[test]
    fn the_shifted_column_is_reached_with_shift() {
        let map = build(&[0x31, 0x21], 2, 10); // 1 and !
        assert_eq!(at(&map, 0x31), Some((10, false)));
        assert_eq!(at(&map, 0x21), Some((10, true)));
    }

    #[test]
    fn an_implied_uppercase_is_filled_in() {
        // Only the lowercase is listed, as a server is allowed to do.
        let map = build(&[0x71, NONE], 2, 24); // q, nothing
        assert_eq!(at(&map, 0x71), Some((24, false)));
        assert_eq!(at(&map, 0x51), Some((24, true))); // Q
        // A listed pair is not overwritten by the rule.
        assert_eq!(at(&build(&[0x61, 0x41], 2, 38), 0x41), Some((38, true)));
    }

    #[test]
    fn latin_one_keeps_its_case_pairs_and_the_gaps() {
        let map = build(&[0xe9, NONE], 2, 30); // e acute
        assert_eq!(at(&map, 0xc9), Some((30, true)));
        // 0xf7 is division, not a letter, so it has no uppercase.
        assert_eq!(upper_case(0xf7), None);
        assert_eq!(upper_case(0xff0d), None); // Return is not a letter
    }

    #[test]
    fn groups_past_the_first_two_columns_are_left_alone() {
        // Four entries per keycode: a, A, then the second group.
        let map = build(&[0x61, 0x41, 0x100_2018, 0x100_2019], 4, 8);
        assert_eq!(map.len(), 2);
        assert_eq!(at(&map, 0x100_2018), None);
    }

    #[test]
    fn an_empty_or_ragged_mapping_does_not_panic() {
        assert!(build(&[], 0, 8).is_empty());
        assert!(build(&[], 2, 8).is_empty());
        // One column only: the case rule is the only way to reach Shift.
        let map = build(&[0x61, 0x62], 1, 8);
        assert_eq!(at(&map, 0x62), Some((9, false)));
        assert_eq!(at(&map, 0x41), Some((8, true)));
    }
}
