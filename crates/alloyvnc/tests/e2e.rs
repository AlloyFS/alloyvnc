//! The server driven end to end, in process, against the synthetic screen.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use alloyvnc::client::Client;
use alloyvnc::flow::Limits;
use alloyvnc::server::Server;
use alloyvnc::session::SessionConfig;
use alloyvnc::shared::Shared;
use alloyvnc_proto::msg::ClipboardMessage;
use alloyvnc_proto::{encoding, msg};
use alloyvnc_screen::NullInput;
use alloyvnc_screen::synth::{FakeClipboard, Pace, Step, Synth};

struct Rig {
    addr: SocketAddr,
    shared: Arc<Shared>,
    step: Arc<Step>,
    stop: Arc<AtomicBool>,
    /// The desk's clipboard, which a test copies to and reads back.
    clip: FakeClipboard,
}

impl Rig {
    async fn start(password: Option<&str>) -> Rig {
        Rig::start_with(password, false, Limits::default()).await
    }

    /// A rig whose screen reports the way the real backends do: one
    /// rectangle around everything that changed, and no moves at all.
    async fn start_coarse() -> Rig {
        Rig::start_with(None, true, Limits::default()).await
    }

    /// A rig whose clients are given a window too small to hold much, so
    /// the back-pressure shows up within a frame or two.
    async fn start_narrow(initial_window: u64) -> Rig {
        Rig::start_with(
            None,
            false,
            Limits {
                initial_window,
                growth: 4 * 1024,
            },
        )
        .await
    }

    async fn start_with(password: Option<&str>, coarse: bool, flow: Limits) -> Rig {
        let step = Step::new();
        let capture = Synth::new(320, 200, Pace::Manual(step.clone()));
        let capture = if coarse { capture.coarse() } else { capture };
        let clip = FakeClipboard::new();
        let shared = Shared::new("e2e", 320, 200, Box::new(NullInput), Box::new(clip.clone()));
        let session = SessionConfig {
            password: password.map(str::to_owned),
            max_fps: 1000,
            auth_fail_delay: Duration::ZERO,
            flow,
        };
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), session, shared.clone())
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(server.run());
        let stop = Arc::new(AtomicBool::new(false));
        alloyvnc::capture::spawn(shared.clone(), Box::new(capture), stop.clone());
        Rig {
            addr,
            shared,
            step,
            stop,
            clip,
        }
    }

    /// Wait for something the capture thread does on its own clock. The
    /// clipboard is polled once per wait slice, so this is the same shape
    /// as `frames` and for the same reason.
    async fn until(&self, mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("the capture thread never got there");
    }

    /// Draw `n` more frames and wait until the capture thread has applied them.
    async fn frames(&self, n: u64) {
        let want = self.shared.seq() + n;
        self.step.advance(n);
        for _ in 0..500 {
            if self.shared.seq() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("capture thread did not reach frame {want}");
    }

    fn picture(&self) -> Vec<u8> {
        self.shared.fb.read().data().to_vec()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

const ALL: &[i32] = &[
    encoding::RAW,
    encoding::COPY_RECT,
    encoding::PSEUDO_CURSOR_WITH_ALPHA,
    encoding::PSEUDO_CURSOR,
    encoding::PSEUDO_EXTENDED_DESKTOP_SIZE,
    encoding::PSEUDO_DESKTOP_SIZE,
    encoding::PSEUDO_CONTINUOUS_UPDATES,
    encoding::PSEUDO_FENCE,
];

#[tokio::test]
async fn full_then_incremental_updates_reproduce_the_picture() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;

    let mut c = Client::connect(rig.addr, None).await.unwrap();
    assert_eq!(c.name, "e2e");
    assert_eq!((c.fb.width(), c.fb.height()), (320, 200));
    c.set_encodings(&[encoding::RAW]).await.unwrap();

    c.request_all(false).await.unwrap();
    let rects = c.next_update().await.unwrap();
    assert_eq!(
        rects.len(),
        1,
        "a fresh client gets the whole picture in one rectangle"
    );
    assert_eq!(c.fb.data(), rig.picture());

    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    let rects = c.next_update().await.unwrap();
    // Without CopyRect the scrolled band is all damage, so this is most of
    // the picture, but never the whole of it.
    let area: i64 = rects.iter().map(|r| r.rect.area()).sum();
    assert!(
        area < 320 * 200,
        "an incremental update carries only the change: {rects:?}"
    );
    assert!(area > 320 * 76, "the scrolled band is in it: {rects:?}");
    assert!(
        rects.iter().all(|r| r.encoding == encoding::RAW),
        "Raw only, as asked"
    );
    assert_eq!(c.fb.data(), rig.picture());

    // Several frames between requests are coalesced into one update.
    rig.frames(5).await;
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
}

#[tokio::test]
async fn moves_go_out_as_copyrect_when_the_source_is_current() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    // One frame: the band's scroll arrives as a CopyRect, the rest as Raw.
    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    let rects = c.next_update().await.unwrap();
    let copies: Vec<_> = rects
        .iter()
        .filter(|r| r.encoding == encoding::COPY_RECT)
        .collect();
    assert_eq!(copies.len(), 1, "{rects:?}");
    assert_eq!(copies[0].rect.height(), 76, "the band less the scrolled rows");
    assert_eq!(c.fb.data(), rig.picture());

    // Five frames unrequested: only the first scroll is still a valid copy;
    // the later ones land on damaged sources and go out as pixels.
    rig.frames(5).await;
    c.request_all(true).await.unwrap();
    let rects = c.next_update().await.unwrap();
    let copies = rects.iter().filter(|r| r.encoding == encoding::COPY_RECT).count();
    assert_eq!(copies, 1, "{rects:?}");
    assert_eq!(c.fb.data(), rig.picture());

    // Many frames more, always exact.
    for _ in 0..20 {
        rig.frames(3).await;
        c.request_all(true).await.unwrap();
        c.next_update().await.unwrap();
        assert_eq!(c.fb.data(), rig.picture());
    }
}

#[tokio::test]
async fn cursor_and_layout_pseudo_rects() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    let rects = c.next_update().await.unwrap();
    assert!(
        rects
            .iter()
            .any(|r| r.encoding == encoding::PSEUDO_EXTENDED_DESKTOP_SIZE),
        "the layout is announced once the client supports it: {rects:?}"
    );
    assert_eq!(c.screens.len(), 1);
    assert_eq!((c.screens[0].width, c.screens[0].height), (320, 200));

    // A second client that takes only the old Cursor encoding; both watch
    // the same shape change.
    let mut old = Client::connect(rig.addr, None).await.unwrap();
    old.set_encodings(&[encoding::RAW, encoding::PSEUDO_CURSOR])
        .await
        .unwrap();
    old.request_all(false).await.unwrap();
    old.next_update().await.unwrap();

    // The pointer shape of the first frame was published before either
    // client connected; the next shape change brings one to both.
    rig.frames(30).await;
    c.request_all(true).await.unwrap();
    let rects = c.next_update().await.unwrap();
    assert!(
        rects
            .iter()
            .any(|r| r.encoding == encoding::PSEUDO_CURSOR_WITH_ALPHA),
        "{rects:?}"
    );
    let shape = c.cursor.clone().expect("a pointer shape");
    assert_eq!((shape.width, shape.height), (12, 12));
    assert_eq!(c.fb.data(), rig.picture());

    old.request_all(true).await.unwrap();
    let rects = old.next_update().await.unwrap();
    assert!(
        rects.iter().any(|r| r.encoding == encoding::PSEUDO_CURSOR),
        "{rects:?}"
    );
    let masked = old.cursor.clone().expect("a pointer shape");
    assert_eq!((masked.width, masked.height), (12, 12));
    // Same shape through both encodings, alpha reduced to a mask.
    let expect: Vec<u8> = shape
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2], if p[3] >= 128 { 255 } else { 0 }])
        .collect();
    assert_eq!(masked.rgba, expect);

    // A client asking to resize the screen is told no, with the layout.
    c.set_desktop_size(640, 480).await.unwrap();
    let rects = c.next_update().await.unwrap();
    assert_eq!(rects.len(), 1);
    assert_eq!(
        c.resizes.last(),
        Some(&(msg::resize_reason::THIS_CLIENT, msg::resize_status::PROHIBITED))
    );
    assert_eq!((c.fb.width(), c.fb.height()), (320, 200), "not resized");
}

#[tokio::test]
async fn continuous_updates_push_without_requests() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(&[encoding::RAW, encoding::PSEUDO_CONTINUOUS_UPDATES])
        .await
        .unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(
        c.end_of_continuous_updates, 1,
        "support is announced once the client lists the encoding"
    );

    c.enable_continuous_updates(true).await.unwrap();
    for _ in 0..3 {
        rig.frames(1).await;
        c.next_update().await.unwrap();
        assert_eq!(c.fb.data(), rig.picture());
    }
    c.enable_continuous_updates(false).await.unwrap();
    // The acknowledgement arrives on its own. A push for a frame the server
    // saw before it read the disable may still precede it, which the
    // protocol allows; the fence echo marks the point past which nothing is
    // pushed, so read until it arrives.
    rig.frames(1).await;
    c.fence(msg::FENCE_REQUEST | msg::FENCE_SYNC_NEXT, b"ping")
        .await
        .unwrap();
    c.request_all(false).await.unwrap();
    while c.fences.is_empty() {
        c.next_update().await.unwrap();
    }
    assert_eq!(c.end_of_continuous_updates, 2);
    assert_eq!(c.fences, vec![(msg::FENCE_SYNC_NEXT, b"ping".to_vec())]);
    assert_eq!(c.fb.data(), rig.picture());
}

#[tokio::test]
async fn vnc_authentication() {
    let rig = Rig::start(Some("hunter2")).await;
    rig.frames(1).await;
    let err = Client::connect(rig.addr, Some("wrong")).await.unwrap_err();
    assert!(err.to_string().contains("authentication failed"), "{err:#}");
    let err = Client::connect(rig.addr, None).await.unwrap_err();
    assert!(err.to_string().contains("authentication failed"), "{err:#}");
    let mut c = Client::connect(rig.addr, Some("hunter2")).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
}

#[tokio::test]
async fn input_and_cut_text_are_accepted() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.key(0x61, true).await.unwrap();
    c.key(0x61, false).await.unwrap();
    c.pointer(10, 10, 1).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
}

#[tokio::test]
async fn the_compare_pass_finds_the_scroll_a_coarse_backend_hides() {
    let rig = Rig::start_coarse().await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());

    // The screen said only "this rectangle changed" and named no moves, so
    // any CopyRect here was found by comparing hashes, and the damage beside
    // it is narrower than the rectangle the screen reported.
    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    let rects = c.next_update().await.unwrap();
    let copies = rects.iter().filter(|r| r.encoding == encoding::COPY_RECT).count();
    assert_eq!(copies, 1, "{rects:?}");
    assert_eq!(c.fb.data(), rig.picture());

    // And it stays exact however the two get out of step.
    for _ in 0..20 {
        rig.frames(3).await;
        c.request_all(true).await.unwrap();
        c.next_update().await.unwrap();
        assert_eq!(c.fb.data(), rig.picture());
    }
}

/// A fence asking for SyncNext is answered immediately before the next
/// update, with nothing between the two. That is how a client knows which
/// side of a change an update belongs to.
#[tokio::test]
async fn a_sync_next_fence_lands_against_the_update() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    rig.frames(1).await;
    c.fence(msg::FENCE_REQUEST | msg::FENCE_SYNC_NEXT, b"mark")
        .await
        .unwrap();
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();

    let answered = c
        .fences
        .iter()
        .find(|(flags, payload)| payload == b"mark" && flags & msg::FENCE_REQUEST == 0)
        .expect("the fence came back");
    assert_eq!(
        answered.0 & msg::FENCE_SYNC_NEXT,
        msg::FENCE_SYNC_NEXT,
        "with its flags"
    );
    let tail: Vec<u8> = c.order.iter().rev().take(2).copied().collect();
    assert_eq!(
        tail,
        [
            msg::server_type::FRAMEBUFFER_UPDATE,
            msg::server_type::SERVER_FENCE
        ],
        "the update follows the fence with nothing in between: {:?}",
        c.order
    );
    assert_eq!(c.fb.data(), rig.picture());
}

/// A client that lists the Fence encoding gets pinged, and its answers are
/// what the round trip is measured from.
#[tokio::test]
async fn answering_the_pings_gives_the_server_a_round_trip() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    assert!(
        alloyvnc::stats::document(&rig.shared).contains("\"rtt_ms\":0.000"),
        "nothing has come back yet"
    );

    // Each turn answers the ping that came with the last update, so the
    // measurement takes a few frames rather than one.
    let mut measured = false;
    for _ in 0..20 {
        rig.frames(1).await;
        c.request_all(true).await.unwrap();
        c.next_update().await.unwrap();
        if !alloyvnc::stats::document(&rig.shared).contains("\"rtt_ms\":0.000") {
            measured = true;
            break;
        }
    }
    assert!(
        measured,
        "the server never got a round trip: {}",
        alloyvnc::stats::document(&rig.shared)
    );
    assert!(c.pings > 0, "and it did ask");
    assert_eq!(c.fb.data(), rig.picture());
}

/// A client that stops answering is not sent more and more: the window
/// shuts, the frames fold together, and when it answers again one update
/// carries the lot.
#[tokio::test]
async fn a_client_that_stops_answering_stops_being_sent_to() {
    let rig = Rig::start_narrow(16 * 1024).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.hold_fences = true;
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    // The whole picture went out, which is far past a sixteen kilobyte
    // window, and the fence that came with it is being held.
    for _ in 0..10 {
        rig.frames(1).await;
        c.request_all(true).await.unwrap();
    }
    let blocked = tokio::time::timeout(Duration::from_millis(300), c.next_update()).await;
    assert!(blocked.is_err(), "nothing more should come: {blocked:?}");
    assert!(c.held_fences() > 0, "and it is holding what it was asked");

    // Answering opens the window, and what arrives is everything at once.
    let released = c.release_fences().await.unwrap();
    assert!(released > 0);
    c.request_all(true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), c.next_update())
        .await
        .expect("the window opened again")
        .unwrap();
    assert_eq!(c.fb.data(), rig.picture(), "and the picture is exact");
}

/// One slow client does not slow the other down, and neither ends up with
/// the wrong picture.
#[tokio::test]
async fn a_slow_client_does_not_hold_up_a_fast_one() {
    let rig = Rig::start_narrow(16 * 1024).await;
    rig.frames(1).await;
    let mut fast = Client::connect(rig.addr, None).await.unwrap();
    fast.set_encodings(ALL).await.unwrap();
    fast.request_all(false).await.unwrap();
    fast.next_update().await.unwrap();

    let mut slow = Client::connect(rig.addr, None).await.unwrap();
    slow.hold_fences = true;
    slow.set_encodings(ALL).await.unwrap();
    slow.request_all(false).await.unwrap();
    slow.next_update().await.unwrap();

    let mut fast_updates = 0;
    for _ in 0..30 {
        rig.frames(1).await;
        fast.request_all(true).await.unwrap();
        if tokio::time::timeout(Duration::from_millis(500), fast.next_update())
            .await
            .is_ok()
        {
            fast_updates += 1;
        }
        slow.request_all(true).await.unwrap();
    }
    assert!(fast_updates >= 25, "the fast client kept going: {fast_updates}");
    assert_eq!(fast.fb.data(), rig.picture());

    // The slow one starts answering again and catches up in a couple of
    // updates rather than thirty, because the frames it missed folded into
    // one region while it was not listening.
    slow.hold_fences = false;
    slow.release_fences().await.unwrap();
    let mut slow_updates = 0;
    for _ in 0..10 {
        slow.request_all(true).await.unwrap();
        if tokio::time::timeout(Duration::from_millis(400), slow.next_update())
            .await
            .is_err()
        {
            break;
        }
        slow_updates += 1;
        if slow.fb.data() == rig.picture() {
            break;
        }
    }
    assert!(slow_updates > 0, "it did get going again");
    assert!(
        slow_updates < 10,
        "it caught up in {slow_updates} rather than thirty"
    );
    assert_eq!(slow.fb.data(), rig.picture(), "and its picture is exact too");
}

/// The counters are a document anyone can fetch.
#[tokio::test]
async fn the_stats_endpoint_answers_with_both_sessions() {
    let rig = Rig::start(None).await;
    rig.frames(2).await;
    let mut one = Client::connect(rig.addr, None).await.unwrap();
    one.set_encodings(ALL).await.unwrap();
    one.request_all(false).await.unwrap();
    one.next_update().await.unwrap();
    let mut two = Client::connect(rig.addr, None).await.unwrap();
    two.set_encodings(ALL).await.unwrap();
    two.request_all(false).await.unwrap();
    two.next_update().await.unwrap();

    let endpoint = alloyvnc::stats::Endpoint::bind("127.0.0.1:0".parse().unwrap(), rig.shared.clone())
        .await
        .unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(endpoint.run());

    let body = fetch(addr, "GET /stats HTTP/1.1\r\nHost: x\r\n\r\n").await;
    let (head, json) = body.split_once("\r\n\r\n").expect("headers then body");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(head.contains("Content-Type: application/json"), "{head}");

    // No JSON crate is in the tree, so the checks are on the text: the
    // shape has to balance and the fields have to be there.
    assert_eq!(json.matches('{').count(), json.matches('}').count(), "{json}");
    assert_eq!(json.matches('[').count(), json.matches(']').count(), "{json}");
    assert!(json.starts_with("{\"capture\":{\"frames\":"), "{json}");
    assert!(json.contains("\"width\":320,\"height\":200"), "{json}");
    assert_eq!(
        json.matches("\"peer\":\"127.0.0.1:").count(),
        2,
        "both sessions: {json}"
    );
    assert_eq!(
        json.matches("\"latency_ms\":{").count(),
        2,
        "each with a histogram: {json}"
    );
    assert!(json.contains("\"window\":"), "{json}");

    let refused = fetch(addr, "POST / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
}

/// One request, one response, connection closed.
async fn fetch(addr: std::net::SocketAddr, request: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    String::from_utf8(out).expect("the document is text")
}

/// The encoders, end to end: whatever a client asks for, the picture it ends
/// up with is the picture the server has.
#[tokio::test]
async fn every_lossless_encoding_reproduces_the_picture() {
    for (name, encoding) in [
        ("Tight", encoding::TIGHT),
        ("ZRLE", encoding::ZRLE),
        ("Hextile", encoding::HEXTILE),
        ("Raw", encoding::RAW),
    ] {
        let rig = Rig::start(None).await;
        rig.frames(1).await;
        let mut c = Client::connect(rig.addr, None).await.unwrap();
        let mut list = vec![encoding];
        list.extend_from_slice(ALL);
        c.set_encodings(&list).await.unwrap();
        c.request_all(false).await.unwrap();
        c.next_update().await.unwrap();
        assert_eq!(c.fb.data(), rig.picture(), "{name}: the first update");

        for frame in 0..20 {
            rig.frames(1).await;
            c.request_all(true).await.unwrap();
            c.next_update().await.unwrap();
            assert_eq!(c.fb.data(), rig.picture(), "{name}: after frame {frame}");
        }

        // And it really did use what it was asked for.
        let doc = alloyvnc::stats::document(&rig.shared);
        let field = match encoding {
            encoding::TIGHT => "\"tight\":",
            encoding::ZRLE => "\"zrle\":",
            encoding::HEXTILE => "\"hextile\":",
            _ => "\"raw\":",
        };
        let spent: u64 = doc
            .split(field)
            .nth(1)
            .and_then(|rest| rest.split([',', '}']).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(spent > 0, "{name} sent nothing through its own encoding: {doc}");
    }
}

/// A client that says it will take JPEG gets one for the busy parts, and the
/// picture it ends up with is the picture within what JPEG gives back.
#[tokio::test]
async fn a_quality_level_sends_jpeg_and_the_picture_survives_it() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    let mut list = vec![encoding::TIGHT, encoding::TIGHT_QUALITY_BASE + 6];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();

    // The synthetic screen is drawn from a handful of colours, so a palette
    // beats JPEG on every piece of it and JPEG never gets a look in. A
    // gradient is what JPEG is for: no two pixels alike, and nothing for a
    // palette or a run to hold on to. It is painted straight into the shared
    // picture, and no frame is drawn after it, so it is what the update
    // carries.
    {
        let mut fb = rig.shared.fb.write();
        for y in 0..fb.height() {
            for x in 0..fb.width() {
                // Smooth in every channel: a wrap would be a hard edge,
                // and JPEG rings around those rather than blurring them.
                let (w, h) = (fb.width().max(1), fb.height().max(1));
                let v = [
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    ((x + y) * 255 / (w + h)) as u8,
                    0,
                ];
                fb.put_pixel(x, y, v);
            }
        }
    }
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    let doc = alloyvnc::stats::document(&rig.shared);
    let jpeg: u64 = doc
        .split("\"tight_jpeg\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(jpeg > 0, "the noisy band went as JPEG: {doc}");

    // Lossy, so the check is how far off rather than whether.
    let server = rig.picture();
    let client = c.fb.data();
    assert_eq!(server.len(), client.len());
    let worst = server
        .as_chunks::<4>()
        .0
        .iter()
        .zip(client.as_chunks::<4>().0)
        .map(|(a, b)| (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0))
        .max()
        .unwrap_or(0);
    assert!(
        worst <= 40,
        "the worst channel is {worst} off after a JPEG round trip"
    );
}

/// The client's list is an order of preference, not a set.
#[tokio::test]
async fn the_clients_own_order_decides_the_encoding() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;

    // Hextile named before Tight, so Hextile it is, even though the server
    // would rather send Tight.
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(&[encoding::HEXTILE, encoding::TIGHT, encoding::RAW])
        .await
        .unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());

    let doc = alloyvnc::stats::document(&rig.shared);
    let spent = |field: &str| -> u64 {
        doc.split(field)
            .nth(1)
            .and_then(|rest| rest.split([',', '}']).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    assert!(spent("\"hextile\":") > 0, "{doc}");
    assert_eq!(spent("\"tight\":"), 0, "{doc}");

    // And saying something different later changes it, streams and all.
    c.set_encodings(&[encoding::TIGHT, encoding::HEXTILE, encoding::RAW])
        .await
        .unwrap();
    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture(), "after changing its mind");
    let doc = alloyvnc::stats::document(&rig.shared);
    let tight: u64 = doc
        .split("\"tight\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(tight > 0, "{doc}");
}

/// A client with no extension gets the plain message, and the plain message
/// is Latin-1: the tick is lost on the way out and the accent survives.
#[tokio::test]
async fn plain_cut_text_carries_what_latin1_can_and_no_more() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    // Somebody at the desk copies something.
    rig.clip.copy("héllo ✓");
    assert_eq!(c.next_clipboard_text().await.unwrap(), "héllo ?");

    // And the other way: what the client sends reaches the desk whole,
    // because it was Latin-1 to begin with.
    c.cut_text("from the client").await.unwrap();
    rig.until(|| rig.clip.written() == ["from the client"]).await;
}

/// The extension's whole point, end to end: the text is UTF-8 and the server
/// does not push it until somebody over there pastes.
#[tokio::test]
async fn the_extension_holds_the_text_until_it_is_asked_for() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.hold_clipboard = true;
    let mut list = vec![encoding::PSEUDO_EXTENDED_CLIPBOARD];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();

    // The server opens with what it can take.
    c.next_clipboard().await.unwrap();
    let caps = c.clip_messages.first().expect("the server's caps");
    assert!(caps.answers(msg::clipboard::action::PROVIDE), "{caps:?}");
    assert!(caps.answers(msg::clipboard::action::REQUEST), "{caps:?}");

    c.clipboard_caps().await.unwrap();
    // The caps have to land before the copy, or the server answers the way
    // it does for a client that has said nothing yet.
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    rig.clip.copy("héllo ✓");
    c.next_clipboard().await.unwrap();
    assert_eq!(
        c.clip_messages.last(),
        Some(&ClipboardMessage::Notify(msg::clipboard::format::TEXT)),
        "a copy is announced, not sent"
    );
    assert!(c.clip_text.is_empty(), "and nothing was sent with it");

    // Now somebody pastes.
    c.clipboard_request().await.unwrap();
    assert_eq!(c.next_clipboard_text().await.unwrap(), "héllo ✓");
}

/// Client to desk over the extension, with the same string the plain
/// message cannot carry.
#[tokio::test]
async fn a_client_provides_utf8_and_the_desk_gets_all_of_it() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    let mut list = vec![encoding::PSEUDO_EXTENDED_CLIPBOARD];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();
    // The server's own caps come first, unasked, as soon as it sees the
    // pseudo-encoding named.
    c.next_clipboard().await.unwrap();
    c.clipboard_caps().await.unwrap();

    // The long way round: say something was copied, wait to be asked, send
    // it. Which is what a real viewer does.
    c.clipboard_notify().await.unwrap();
    c.next_clipboard().await.unwrap();
    assert_eq!(
        c.clip_messages.last(),
        Some(&ClipboardMessage::Request(msg::clipboard::format::TEXT)),
        "the server asks for what a client says it has"
    );
    c.clipboard_provide("héllo ✓").await.unwrap();
    rig.until(|| rig.clip.written() == ["héllo ✓"]).await;
}

/// A client that pastes its own text back does not get it handed to it
/// again, which is the loop every clipboard bridge has to break somewhere.
#[tokio::test]
async fn a_clients_own_text_is_not_sent_back_to_it() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    c.cut_text("round and round").await.unwrap();
    rig.until(|| rig.clip.written() == ["round and round"]).await;
    // The platform reports it back, as every platform does.
    rig.clip.copy("round and round");
    // Then something else happens, which the client should hear about.
    rig.clip.copy("but this is new");

    assert_eq!(c.next_clipboard_text().await.unwrap(), "but this is new");
    assert_eq!(c.clip_text, ["but this is new"], "and only that");
}

/// Two clients see the same desk, and one client's paste reaches the other.
#[tokio::test]
async fn the_desks_clipboard_reaches_every_client() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut a = Client::connect(rig.addr, None).await.unwrap();
    let mut b = Client::connect(rig.addr, None).await.unwrap();
    for c in [&mut a, &mut b] {
        c.set_encodings(ALL).await.unwrap();
        c.request_all(false).await.unwrap();
        c.next_update().await.unwrap();
    }

    rig.clip.copy("for both of them");
    assert_eq!(a.next_clipboard_text().await.unwrap(), "for both of them");
    assert_eq!(b.next_clipboard_text().await.unwrap(), "for both of them");

    // And the same word copied twice is two changes, not one: a session
    // watching the text alone would sit on the second.
    rig.clip.copy("for both of them");
    assert_eq!(a.next_clipboard_text().await.unwrap(), "for both of them");
    assert_eq!(b.next_clipboard_text().await.unwrap(), "for both of them");
}

/// A peek is answered with what is held, and nothing is sent for it.
#[tokio::test]
async fn a_peek_says_what_is_held_without_sending_it() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.hold_clipboard = true;
    let mut list = vec![encoding::PSEUDO_EXTENDED_CLIPBOARD];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();
    c.next_clipboard().await.unwrap();
    c.clipboard_caps().await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    // Nothing copied yet, so the answer names no formats at all.
    c.clipboard_peek().await.unwrap();
    c.next_clipboard().await.unwrap();
    assert_eq!(c.clip_messages.last(), Some(&ClipboardMessage::Notify(0)));

    rig.clip.copy("held back");
    c.next_clipboard().await.unwrap();
    c.clipboard_peek().await.unwrap();
    c.next_clipboard().await.unwrap();
    assert_eq!(
        c.clip_messages.last(),
        Some(&ClipboardMessage::Notify(msg::clipboard::format::TEXT))
    );
    assert!(c.clip_text.is_empty(), "a peek sends nothing");
}

/// A clipboard change with a still screen is still seen, because the capture
/// thread asks after every wait rather than after every frame.
#[tokio::test]
async fn a_copy_with_nothing_moving_on_screen_still_arrives() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    c.set_encodings(ALL).await.unwrap();
    c.request_all(false).await.unwrap();
    c.next_update().await.unwrap();

    let before = rig.shared.seq();
    rig.clip.copy("nothing is moving");
    assert_eq!(c.next_clipboard_text().await.unwrap(), "nothing is moving");
    assert_eq!(rig.shared.seq(), before, "and no frame was drawn for it");
}

/// A paste from a real browser, which is not the shape this server's own
/// writer makes.
///
/// noVNC's deflater is flushed and never finished, because it carries on
/// into the next message: no final block, no Adler-32 trailer. Reading it
/// to the end of the stream reports a truncation that is not one, and the
/// session was dropped on every paste. Found with noVNC in a browser
/// against the release binary.
#[tokio::test]
async fn a_paste_from_novnc_reaches_the_desk_and_the_session_lives() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    let mut list = vec![encoding::PSEUDO_EXTENDED_CLIPBOARD];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();
    c.next_clipboard().await.unwrap();
    c.clipboard_caps().await.unwrap();

    c.clipboard_provide_flushed("probe ✓ two").await.unwrap();
    rig.until(|| rig.clip.written() == ["probe ✓ two"]).await;

    // And the session is still there, which is the other half of it.
    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
}

/// A clipboard body this end cannot read costs the message and not the
/// session. Everything a client sends after it still works.
#[tokio::test]
async fn an_unreadable_clipboard_body_does_not_end_the_session() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;
    let mut c = Client::connect(rig.addr, None).await.unwrap();
    let mut list = vec![encoding::PSEUDO_EXTENDED_CLIPBOARD];
    list.extend_from_slice(ALL);
    c.set_encodings(&list).await.unwrap();
    c.next_clipboard().await.unwrap();

    // A provide whose stream is noise, a provide that stops halfway
    // through what its own length promised, a caps naming two formats and
    // carrying one size, and a body too short to hold its own flags.
    let provide = (msg::clipboard::action::PROVIDE | msg::clipboard::format::TEXT).to_be_bytes();
    let mut noise = provide.to_vec();
    noise.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let mut half = msg::clipboard_provide_text("a string of some length");
    half.truncate(provide.len() + 4);
    let mut caps =
        (msg::clipboard::action::CAPS | msg::clipboard::format::TEXT | msg::clipboard::format::HTML)
            .to_be_bytes()
            .to_vec();
    caps.extend_from_slice(&4096u32.to_be_bytes());
    for body in [noise, half, caps, vec![0, 0, 0]] {
        c.clipboard_body(&body).await.unwrap();
    }

    // Nothing was put on the desk, and the session carried on.
    c.clipboard_provide("and now a good one").await.unwrap();
    rig.until(|| rig.clip.written() == ["and now a good one"]).await;
    rig.frames(1).await;
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
}
