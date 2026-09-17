//! Encoding numbers, from RFC 6143 section 7.7 and the community registry
//! (rfbproto). Negative numbers are pseudo-encodings: a client lists them in
//! SetEncodings to say what it can do, and the server answers with rectangles
//! that carry no pixels.

pub const RAW: i32 = 0;
pub const COPY_RECT: i32 = 1;
pub const RRE: i32 = 2;
pub const HEXTILE: i32 = 5;
pub const ZLIB: i32 = 6;
pub const TIGHT: i32 = 7;
pub const TRLE: i32 = 15;
pub const ZRLE: i32 = 16;
pub const JPEG: i32 = 21;
pub const OPEN_H264: i32 = 50;

pub const PSEUDO_CURSOR: i32 = -239;
pub const PSEUDO_DESKTOP_SIZE: i32 = -223;
pub const PSEUDO_LAST_RECT: i32 = -224;
pub const PSEUDO_POINTER_POS: i32 = -232;
pub const PSEUDO_QEMU_EXTENDED_KEY: i32 = -258;
pub const PSEUDO_TIGHT_PNG: i32 = -260;
pub const PSEUDO_DESKTOP_NAME: i32 = -307;
pub const PSEUDO_EXTENDED_DESKTOP_SIZE: i32 = -308;
pub const PSEUDO_XVP: i32 = -309;
pub const PSEUDO_FENCE: i32 = -312;
pub const PSEUDO_CONTINUOUS_UPDATES: i32 = -313;
pub const PSEUDO_CURSOR_WITH_ALPHA: i32 = -314;
pub const PSEUDO_EXTENDED_MOUSE_BUTTONS: i32 = -316;
pub const PSEUDO_EXTENDED_CLIPBOARD: i32 = 0xc0a1_e5ce_u32 as i32;
pub const PSEUDO_VMWARE_CURSOR: i32 = 0x574d_5664;

/// Tight JPEG quality levels: `-32` is quality 0, `-23` is quality 9.
pub const TIGHT_QUALITY_BASE: i32 = -32;
/// Tight compression levels: `-256` is level 0, `-247` is level 9.
pub const TIGHT_COMPRESS_BASE: i32 = -256;

pub fn tight_quality(encoding: i32) -> Option<u8> {
    level_of(encoding, TIGHT_QUALITY_BASE)
}

pub fn tight_compression(encoding: i32) -> Option<u8> {
    level_of(encoding, TIGHT_COMPRESS_BASE)
}

fn level_of(encoding: i32, base: i32) -> Option<u8> {
    (base..=base + 9)
        .contains(&encoding)
        .then(|| (encoding - base) as u8)
}

/// A name for the log line.
pub fn name(encoding: i32) -> &'static str {
    match encoding {
        RAW => "Raw",
        COPY_RECT => "CopyRect",
        RRE => "RRE",
        HEXTILE => "Hextile",
        ZLIB => "Zlib",
        TIGHT => "Tight",
        TRLE => "TRLE",
        ZRLE => "ZRLE",
        JPEG => "JPEG",
        OPEN_H264 => "OpenH264",
        PSEUDO_CURSOR => "Cursor",
        PSEUDO_DESKTOP_SIZE => "DesktopSize",
        PSEUDO_LAST_RECT => "LastRect",
        PSEUDO_POINTER_POS => "PointerPos",
        PSEUDO_QEMU_EXTENDED_KEY => "QemuExtendedKey",
        PSEUDO_TIGHT_PNG => "TightPNG",
        PSEUDO_DESKTOP_NAME => "DesktopName",
        PSEUDO_EXTENDED_DESKTOP_SIZE => "ExtendedDesktopSize",
        PSEUDO_XVP => "xvp",
        PSEUDO_FENCE => "Fence",
        PSEUDO_CONTINUOUS_UPDATES => "ContinuousUpdates",
        PSEUDO_CURSOR_WITH_ALPHA => "CursorWithAlpha",
        PSEUDO_EXTENDED_MOUSE_BUTTONS => "ExtendedMouseButtons",
        PSEUDO_EXTENDED_CLIPBOARD => "ExtendedClipboard",
        PSEUDO_VMWARE_CURSOR => "VMwareCursor",
        e if tight_quality(e).is_some() => "TightQuality",
        e if tight_compression(e).is_some() => "TightCompression",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(tight_quality(-32), Some(0));
        assert_eq!(tight_quality(-23), Some(9));
        assert_eq!(tight_quality(-22), None);
        assert_eq!(tight_compression(-256), Some(0));
        assert_eq!(tight_compression(-247), Some(9));
        assert_eq!(tight_compression(-246), None);
    }

    #[test]
    fn extended_clipboard_is_the_registered_value() {
        assert_eq!(PSEUDO_EXTENDED_CLIPBOARD, -1_063_131_698);
        assert_eq!(name(PSEUDO_EXTENDED_CLIPBOARD), "ExtendedClipboard");
    }
}
