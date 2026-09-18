//! The desk's clipboard on X11, which is not a clipboard.
//!
//! X11 has no place where copied text is kept. It has selections: an atom,
//! `CLIPBOARD`, whose owner is a window, and the owner is asked for the text
//! each time somebody pastes. So reading it means asking whoever owns it and
//! waiting for an answer, and writing it means becoming the owner and
//! answering everybody else's asking for as long as this process lives.
//!
//! Four extensions' worth of protocol, in the order it happens:
//!
//! - XFixes reports a change of owner (`SelectSelectionInput`), which is the
//!   only notification X offers that somebody copied something. Without it
//!   the only way to notice is to convert the selection on a timer, which
//!   wakes the owning application every time.
//! - `ConvertSelection` asks the owner for `UTF8_STRING`, and the answer
//!   arrives later as a `SelectionNotify` naming a property on this
//!   process's own window, which `GetProperty` then reads and deletes.
//! - `SetSelectionOwner` takes the clipboard over when a client pastes into
//!   it, and every `SelectionRequest` after that has to be answered: the
//!   list of formats for `TARGETS`, the text for `UTF8_STRING`, and the same
//!   text in Latin-1 for the older `STRING` and `TEXT`.
//! - `SelectionClear` says somebody else has taken it, and what this process
//!   was holding is no longer anybody's business.
//!
//! One connection, and it is driven by the capture thread's own wakeups
//! rather than by a thread of its own. That costs up to one wait slice in
//! two places, and both are worth knowing: text copied at the desk is seen
//! on the poll after the one that asked for it, and an application pasting
//! from this process waits for the next poll to be answered. A selection
//! thread blocking in `wait_for_event` would make both immediate, and is on
//! the backlog.

use alloyvnc_screen::{CaptureError, Clipboard};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_FROM_PARENT, CURRENT_TIME, NONE};

/// The largest selection this will take in one go, in 32-bit words, which is
/// what `GetProperty` counts in. A megabyte, the same limit the wire puts on
/// a clipboard message.
const MAX_WORDS: u32 = (1 << 20) / 4;

/// Polls to wait for a selection owner's answer before asking again.
///
/// The capture thread polls about ten times a second, so this is two
/// seconds. An owner that has not answered by then is wedged or gone, and
/// asking again is better than never reading the clipboard again.
const WAIT_POLLS: u32 = 20;

/// The atoms this needs, interned once.
struct Atoms {
    clipboard: xproto::Atom,
    utf8_string: xproto::Atom,
    targets: xproto::Atom,
    text: xproto::Atom,
    incr: xproto::Atom,
    /// The property on this process's own window that a conversion's answer
    /// is written into. A name of its own so nothing else can collide with
    /// it.
    ours: xproto::Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Atoms, CaptureError> {
        let ask = |name: &str| -> Result<xproto::Atom, CaptureError> {
            conn.intern_atom(false, name.as_bytes())
                .map_err(|e| CaptureError::Failed(format!("intern {name}: {e}")))?
                .reply()
                .map(|r| r.atom)
                .map_err(|e| CaptureError::Failed(format!("intern {name}: {e}")))
        };
        Ok(Atoms {
            clipboard: ask("CLIPBOARD")?,
            utf8_string: ask("UTF8_STRING")?,
            targets: ask("TARGETS")?,
            text: ask("TEXT")?,
            incr: ask("INCR")?,
            ours: ask("ALLOYVNC_CLIPBOARD")?,
        })
    }
}

pub struct X11Clipboard {
    conn: RustConnection,
    /// An unmapped window with no pixels, which exists to be a selection
    /// owner and a place for properties to land. X has no other way to
    /// address a client.
    window: xproto::Window,
    atoms: Atoms,
    /// Text a client pasted in, which this process now owns the selection
    /// for and has to keep answering for.
    ours: Option<String>,
    /// The last text read off the desk, so the same selection converted
    /// twice is reported once.
    seen: Option<String>,
    /// Polls left to wait for a conversion's answer. A selection owner
    /// that has hung would otherwise leave this asking for ever and the
    /// clipboard dead for the rest of the session.
    asking: u32,
    /// Whether a refusal has been reported, so a selection owner that will
    /// not answer costs one line rather than one per poll.
    reported: bool,
}

impl X11Clipboard {
    /// Open `display`, or `$DISPLAY` when it is `None`.
    ///
    /// Its own connection, for the same reason the input side has one: the
    /// capture thread is reading damage events off its connection and a
    /// second reader of the same stream would take each other's events.
    pub fn new(display: Option<&str>) -> Result<X11Clipboard, CaptureError> {
        let (conn, screen_num) = x11rb::connect(display)
            .map_err(|e| CaptureError::Failed(format!("connect to the X server for the clipboard: {e}")))?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| CaptureError::Failed(format!("no screen {screen_num}")))?
            .root;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| CaptureError::Failed(format!("the server has no XFixes: {e}")))?
            .reply()
            .map_err(|e| CaptureError::Failed(format!("XFixes version: {e}")))?;

        let atoms = Atoms::intern(&conn)?;
        let window = conn
            .generate_id()
            .map_err(|e| CaptureError::Failed(format!("a window id: {e}")))?;
        // InputOnly: it is never mapped and never drawn into, so it costs
        // the server an entry in a table and nothing else.
        conn.create_window(
            COPY_FROM_PARENT as u8,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| CaptureError::Failed(format!("create the clipboard window: {e}")))?;

        // The one notification X offers that somebody copied something.
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| CaptureError::Failed(format!("watch the clipboard selection: {e}")))?;
        conn.flush()
            .map_err(|e| CaptureError::Failed(format!("flush: {e}")))?;

        tracing::info!(window, "clipboard ready");
        Ok(X11Clipboard {
            conn,
            window,
            atoms,
            ours: None,
            seen: None,
            asking: 0,
            reported: false,
        })
    }

    fn trouble(&mut self, what: &str, e: impl std::fmt::Display) {
        if self.reported {
            tracing::debug!(error = %e, "{what}");
        } else {
            self.reported = true;
            tracing::warn!(error = %e, "{what}");
        }
    }

    /// Ask whoever owns the clipboard for its text. The answer arrives as a
    /// SelectionNotify, which is a later poll's business.
    fn ask(&mut self) {
        if self.asking > 0 {
            // One conversion at a time. A second while the first is out
            // would put two answers in the same property, and an owner that
            // has not answered yet is slow rather than deaf. Slow only goes
            // so far, though: after WAIT_POLLS this asks again.
            return;
        }
        let asked = self
            .conn
            .convert_selection(
                self.window,
                self.atoms.clipboard,
                self.atoms.utf8_string,
                self.atoms.ours,
                CURRENT_TIME,
            )
            .map(drop)
            .map_err(|e| e.to_string());
        match asked {
            Ok(()) => self.asking = WAIT_POLLS,
            Err(e) => self.trouble("could not ask for the clipboard", e),
        }
    }

    /// Read the property a conversion's answer landed in.
    fn collect(&mut self, e: &xproto::SelectionNotifyEvent) -> Option<String> {
        self.asking = 0;
        if e.property == NONE {
            // The owner has no text to give in the format asked for. A
            // clipboard holding a picture answers exactly this way, so it
            // is ordinary rather than a fault.
            tracing::debug!("the clipboard owner has no text");
            return None;
        }
        let reply = self
            .conn
            .get_property(true, self.window, e.property, AtomEnum::ANY, 0, MAX_WORDS)
            .map_err(|e| e.to_string())
            .and_then(|c| c.reply().map_err(|e| e.to_string()));
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                self.trouble("could not read the clipboard property", e);
                return None;
            }
        };
        if reply.type_ == self.atoms.incr {
            // The owner wants to send it a piece at a time, which is a
            // conversation of PropertyNotify events rather than one answer.
            // Not written: every clipboard seen in practice fits in one
            // property well under the megabyte cap. On the backlog.
            tracing::warn!("the clipboard is too large for one transfer; not read");
            return None;
        }
        if reply.value.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&reply.value).into_owned())
    }

    /// Answer somebody asking for what this process owns.
    fn answer(&mut self, e: &SelectionRequestEvent) {
        let Some(text) = self.ours.clone() else {
            self.refuse(e);
            return;
        };
        // A requestor that names no property is an old one, from before the
        // convention existed. The target atom is the property it means.
        let property = if e.property == NONE { e.target } else { e.property };

        let wrote = if e.target == self.atoms.targets {
            // What this owner can produce, as a list of atoms.
            let targets = [
                self.atoms.targets,
                self.atoms.utf8_string,
                u32::from(AtomEnum::STRING),
                self.atoms.text,
            ];
            self.conn
                .change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &targets)
                .map(drop)
                .map_err(|e| e.to_string())
        } else if e.target == self.atoms.utf8_string {
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    self.atoms.utf8_string,
                    text.as_bytes(),
                )
                .map(drop)
                .map_err(|e| e.to_string())
        } else if e.target == u32::from(AtomEnum::STRING) || e.target == self.atoms.text {
            // STRING is Latin-1 by definition, so sending UTF-8 under that
            // name would be a lie that shows up as mojibake in whatever
            // pasted it. Anything outside Latin-1 becomes a question mark,
            // which is what the requestor asked for by asking for STRING.
            let latin1: Vec<u8> = text
                .chars()
                .map(|ch| u8::try_from(u32::from(ch)).unwrap_or(b'?'))
                .collect();
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::STRING,
                    &latin1,
                )
                .map(drop)
                .map_err(|e| e.to_string())
        } else {
            self.refuse(e);
            return;
        };
        if let Err(err) = wrote {
            self.trouble("could not answer a selection request", err);
            return;
        }
        self.tell(e, property);
    }

    /// Say no: the same notification with no property on it.
    fn refuse(&mut self, e: &SelectionRequestEvent) {
        self.tell(e, NONE);
    }

    fn tell(&mut self, e: &SelectionRequestEvent, property: xproto::Atom) {
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property,
        };
        let sent = self
            .conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify)
            .map(drop)
            .map_err(|e| e.to_string());
        if let Err(err) = sent {
            self.trouble("could not answer a selection request", err);
        }
    }

    /// Every event waiting, and the text if one of them carried it.
    fn drain(&mut self) -> Option<String> {
        let mut text = None;
        loop {
            let polled = self.conn.poll_for_event().map_err(|e| e.to_string());
            let event = match polled {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(e) => {
                    self.trouble("the clipboard connection failed", e);
                    break;
                }
            };
            match event {
                // Somebody copied something. Ask them for it; the answer is
                // a later event, usually the next one but not always.
                Event::XfixesSelectionNotify(e) => {
                    if e.owner == self.window {
                        // This process taking the selection over, which is
                        // this process's own paste coming back round.
                        continue;
                    }
                    if e.owner == NONE {
                        // The owner went away and nobody took it on. What
                        // was there is gone rather than changed.
                        continue;
                    }
                    self.ask();
                }
                Event::SelectionNotify(e) if e.requestor == self.window => {
                    if let Some(got) = self.collect(&e) {
                        text = Some(got);
                    }
                }
                Event::SelectionRequest(e) => self.answer(&e),
                Event::SelectionClear(_) => {
                    // Somebody else has the clipboard now. What this
                    // process was holding is no longer its to answer for.
                    self.ours = None;
                }
                // PropertyNotify on this window is the delete of a property
                // already read, and nothing else arrives on an InputOnly
                // window that was never mapped.
                Event::PropertyNotify(e) if e.state == Property::DELETE => {}
                other => tracing::trace!(?other, "clipboard event"),
            }
        }
        // Everything above put requests in the output buffer and nothing
        // has sent them: the conversion this poll asked for, and the answer
        // it owes somebody else. Without this the asking never leaves and
        // the answer never arrives, which is exactly how it failed the
        // first time it was run against a real server.
        let flushed = self.conn.flush().map_err(|e| e.to_string());
        if let Err(e) = flushed {
            self.trouble("could not flush the clipboard connection", e);
        }
        self.asking = self.asking.saturating_sub(1);
        text
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        // The window goes, which drops the selection with it if this
        // process still owns it. Left behind, it would leave the clipboard
        // owned by a window nobody can answer for and every paste on the
        // desk would hang until its timeout.
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

impl Clipboard for X11Clipboard {
    fn changed(&mut self) -> Option<String> {
        let text = self.drain()?;
        if self.seen.as_deref() == Some(text.as_str()) {
            return None;
        }
        // The text this process put there, handed back by a conversion this
        // process asked for. Not news.
        if self.ours.as_deref() == Some(text.as_str()) {
            self.seen = Some(text);
            return None;
        }
        self.seen = Some(text.clone());
        Some(text)
    }

    fn set(&mut self, text: &str) {
        self.ours = Some(text.to_owned());
        // Nobody is told the text now. Ownership is all that changes, and
        // whoever pastes next asks for it, which is what `answer` is for.
        let taken = self
            .conn
            .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)
            .map(drop)
            .map_err(|e| e.to_string());
        if let Err(e) = taken {
            self.trouble("could not take the clipboard over", e);
            return;
        }
        let flushed = self.conn.flush().map_err(|e| e.to_string());
        if let Err(e) = flushed {
            self.trouble("could not flush the clipboard connection", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole selection protocol against a real X server, with this
    /// process on both ends of it.
    ///
    /// Two `X11Clipboard`s are two X clients: one takes the selection over
    /// the way a client's paste does, the other is notified by XFixes,
    /// converts it, and reads the property back. Nothing is mocked, because
    /// there is nothing here worth mocking: every line of this is the X
    /// server's own behaviour.
    ///
    /// One test rather than several, because a display has one clipboard
    /// and cargo runs tests in parallel: split up, they take it off each
    /// other and both fail.
    ///
    /// Ignored by default because it needs a display, and it takes the
    /// desk's clipboard over while it runs. With one:
    /// `cargo test -p alloyvnc-screen-x11 --release -- --ignored`
    #[test]
    #[ignore = "needs an X server, and takes the clipboard over"]
    fn the_selection_goes_across_and_comes_back() {
        let mut desk = X11Clipboard::new(None).expect("an X server on $DISPLAY");
        let mut server = X11Clipboard::new(None).expect("a second connection");

        // Nothing has happened, so neither has anything to say.
        assert_eq!(pump(&mut [&mut desk, &mut server]), None);

        // A client pastes into the session: the server takes the selection
        // over and answers for it from then on.
        server.set("héllo ✓ from a client");
        // The desk's side hears the owner change, asks, and is answered.
        let got = pump(&mut [&mut desk, &mut server]);
        assert_eq!(
            got.as_deref(),
            Some("héllo ✓ from a client"),
            "UTF-8 across a real selection"
        );

        // And back the other way: the desk takes it over, and the server's
        // side sees it.
        desk.set("héllo ✓ from the desk");
        let got = pump(&mut [&mut server, &mut desk]);
        assert_eq!(got.as_deref(), Some("héllo ✓ from the desk"));

        // The owner does not report its own text back as news, however many
        // times it is polled.
        for _ in 0..4 {
            assert_eq!(desk.changed(), None, "the owner's own text is not news");
        }

        // And a second owner takes it away: whoever held it stops answering
        // for text that is no longer theirs.
        server.set("no, mine");
        for _ in 0..50 {
            desk.changed();
            if desk.ours.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(desk.ours, None, "the clipboard was taken away");
    }

    /// Poll both sides until one of them has text, or until it is plainly
    /// not coming. The conversion is three round trips through the server,
    /// so it is never done on the first poll.
    fn pump(sides: &mut [&mut X11Clipboard]) -> Option<String> {
        for _ in 0..50 {
            for side in sides.iter_mut() {
                if let Some(text) = side.changed() {
                    return Some(text);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }
}
