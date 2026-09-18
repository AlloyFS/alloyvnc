//! The desk's clipboard on Windows, polled.
//!
//! There is a notification route (`AddClipboardFormatListener`), and it is
//! not usable from here: it delivers `WM_CLIPBOARDUPDATE` to a window, which
//! means a window, a message pump and a thread dedicated to running it. The
//! capture thread already wakes on a timer, so asking a counter whether
//! anything changed costs one call on a wakeup that was happening anyway.
//!
//! `GetClipboardSequenceNumber` is that counter: the kernel bumps it on
//! every change by anybody, and reading it opens nothing and blocks nobody.
//! Measured on this laptop it is 1.14 us a call (see
//! `clipboard_poll_costs_nothing`). At the 100 ms cadence that is eleven
//! microseconds of every second, or about a thousandth of one per cent of
//! one core: a known price, and a very small one.
//!
//! Text only, and `CF_UNICODETEXT` only. A clipboard holding a picture or a
//! file list reports no change, which is right: there is nothing to send.

use alloyvnc_screen::Clipboard;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// Unicode text, from winuser.h. The constant lives behind a windows-rs
/// feature that exists for OLE and pulls in a great deal beside it.
const CF_UNICODETEXT: u32 = 13;

/// Attempts at opening the clipboard before giving up until the next poll.
///
/// Only one process may have it open at a time, and every application that
/// so much as looks at it holds it for a moment. A refusal is ordinary and
/// means "somebody else is mid-copy", not "something is wrong": the next
/// poll is 100 ms away and the sequence number will still be there.
const TRIES: u32 = 8;

pub struct WinClipboard {
    /// The sequence number as of the last look. Anything else means
    /// somebody changed the clipboard in between.
    seen: u32,
    /// Whether a refusal has already been reported, so a program that sits
    /// on the clipboard costs one line rather than one every tenth second.
    reported: bool,
}

impl Default for WinClipboard {
    fn default() -> WinClipboard {
        WinClipboard::new()
    }
}

impl WinClipboard {
    pub fn new() -> WinClipboard {
        WinClipboard {
            // Whatever is on the clipboard at startup is not news: nobody
            // copied it during this session, and pushing it at the first
            // client to connect would be a surprise rather than a service.
            //
            // SAFETY: no arguments, no pointers, no clipboard opened.
            seen: unsafe { GetClipboardSequenceNumber() },
            reported: false,
        }
    }

    /// Hold the clipboard open for the length of one operation.
    fn open(&mut self) -> Option<Held> {
        for _ in 0..TRIES {
            // SAFETY: opening with no owner window, which associates the
            // clipboard with this thread until it is closed. `Held` closes
            // it on drop, including on the way out of a panic.
            if unsafe { OpenClipboard(None) }.is_ok() {
                self.reported = false;
                return Some(Held);
            }
            std::thread::yield_now();
        }
        if !self.reported {
            self.reported = true;
            tracing::warn!("another program is holding the clipboard open");
        }
        None
    }
}

/// The open clipboard, closed when this goes out of scope.
struct Held;

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: closes what `open` opened, on the same thread.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Read `CF_UNICODETEXT` off an already-open clipboard.
///
/// # Safety
///
/// The clipboard must be open on this thread.
unsafe fn read_text() -> Option<String> {
    // SAFETY: the caller holds the clipboard open. The handle belongs to
    // whoever put the data there and must not be freed here; it is only
    // valid until the clipboard is closed, which is why the string is
    // copied out before the guard drops.
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT).ok()?;
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let global = HGLOBAL(handle.0);
        let ptr = GlobalLock(global) as *const u16;
        if ptr.is_null() {
            return None;
        }
        // The block is null-terminated UTF-16. Its allocated size is not
        // the string's length, so the terminator is what decides.
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(global);
        Some(text)
    }
}

impl Clipboard for WinClipboard {
    fn changed(&mut self) -> Option<String> {
        // SAFETY: no arguments, no pointers, no clipboard opened.
        let now = unsafe { GetClipboardSequenceNumber() };
        if now == self.seen {
            return None;
        }
        // Counted as seen whatever comes of the read. A clipboard holding a
        // picture would otherwise be re-read on every poll for as long as
        // it sat there.
        self.seen = now;
        let _held = self.open()?;
        // SAFETY: `_held` holds the clipboard open for this whole block.
        let text = unsafe { read_text() }?;
        // Windows text is CRLF. The wire wants LF, and the plain cut-text
        // message has no opinion, so the lines are cut here rather than in
        // two places further up.
        Some(text.replace("\r\n", "\n"))
    }

    fn set(&mut self, text: &str) {
        // Back to CRLF: a Windows edit control shows LF-only text as one
        // long line with boxes in it.
        let wide: Vec<u16> = text
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let bytes = std::mem::size_of_val(wide.as_slice());

        let Some(held) = self.open() else {
            return;
        };
        // SAFETY: `held` keeps the clipboard open for this whole block.
        // GMEM_MOVEABLE is what SetClipboardData requires; the block is
        // locked to be written and unlocked before it is handed over, and
        // on success it belongs to the clipboard, which is why it is only
        // freed on the paths that do not reach SetClipboardData.
        unsafe {
            if EmptyClipboard().is_err() {
                tracing::warn!("the clipboard would not empty");
                return;
            }
            let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
                tracing::warn!(bytes, "no room for the clipboard text");
                return;
            };
            let ptr = GlobalLock(global) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(global);
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))).is_err() {
                tracing::warn!("the clipboard refused the text");
                let _ = GlobalFree(Some(global));
                return;
            }
        }
        // The sequence number does not move until the clipboard closes:
        // read before the guard drops and it still holds the old value, the
        // write looks like somebody else's, and the client that pasted gets
        // its own text handed back a tenth of a second later. Found by the
        // round-trip test below, which failed on exactly that.
        drop(held);
        // This write came from a client, so it is not news to anybody. The
        // session drops the echo for the client that sent it; forgetting it
        // here stops it reaching the other sessions at all.
        //
        // SAFETY: no arguments, no pointers.
        self.seen = unsafe { GetClipboardSequenceNumber() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason polling is acceptable. A number, not a claim.
    #[test]
    fn clipboard_poll_costs_nothing() {
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            // SAFETY: no arguments, no pointers.
            std::hint::black_box(unsafe { GetClipboardSequenceNumber() });
        }
        let each = started.elapsed() / 100_000;
        println!("GetClipboardSequenceNumber: {each:?} each");
        // 1.14 us measured here. Ten is an order of magnitude of room, and
        // the assertion is to catch a poll that has become a round trip
        // rather than to pin a number that drifts with the machine.
        assert!(each < std::time::Duration::from_micros(10), "{each:?} per poll");
    }

    /// A round trip through the real clipboard, which is the only place
    /// this code can be checked: there is one clipboard per desktop and no
    /// way to stand a second one up.
    ///
    /// Ignored by default because it takes the desk's clipboard away from
    /// whoever is using it. Run it deliberately:
    /// `cargo test -p alloyvnc-screen-dxgi --release -- --ignored`
    #[test]
    #[ignore = "writes to the desk's own clipboard"]
    fn text_goes_out_and_comes_back_whole() {
        let mut clip = WinClipboard::new();
        assert_eq!(clip.changed(), None, "nothing has changed yet");

        // Whoever is at this desk gets their clipboard back afterwards.
        clip.seen = 0;
        let theirs = clip.changed();

        // What a client pasted in is not news back to the server.
        clip.set("from a client\nwith two lines");
        assert_eq!(clip.changed(), None, "our own write is not a change");

        // Somebody at the desk copying is. The sequence number has to be
        // moved by another process for that, so this stands in for it by
        // setting and then forgetting.
        clip.set("héllo ✓ from the desk");
        clip.seen = 0;
        assert_eq!(
            clip.changed().as_deref(),
            Some("héllo ✓ from the desk"),
            "UTF-8 through a UTF-16 clipboard"
        );
        assert_eq!(clip.changed(), None, "and only once");

        if let Some(theirs) = theirs {
            clip.set(&theirs);
        }
    }
}
