//! The server driven end to end, in process, against the synthetic screen.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use alloyvnc::client::Client;
use alloyvnc::server::Server;
use alloyvnc::session::SessionConfig;
use alloyvnc::shared::Shared;
use alloyvnc_proto::{encoding, msg};
use alloyvnc_screen::NullInput;
use alloyvnc_screen::synth::{Pace, Step, Synth};

struct Rig {
    addr: SocketAddr,
    shared: Arc<Shared>,
    step: Arc<Step>,
    stop: Arc<AtomicBool>,
}

impl Rig {
    async fn start(password: Option<&str>) -> Rig {
        Rig::start_with(password, false).await
    }

    /// A rig whose screen reports the way the real backends do: one
    /// rectangle around everything that changed, and no moves at all.
    async fn start_coarse() -> Rig {
        Rig::start_with(None, true).await
    }

    async fn start_with(password: Option<&str>, coarse: bool) -> Rig {
        let step = Step::new();
        let capture = Synth::new(320, 200, Pace::Manual(step.clone()));
        let capture = if coarse { capture.coarse() } else { capture };
        let shared = Shared::new("e2e", 320, 200, Box::new(NullInput));
        let session = SessionConfig {
            password: password.map(str::to_owned),
            max_fps: 1000,
            auth_fail_delay: Duration::ZERO,
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
        }
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
        .chunks_exact(4)
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
