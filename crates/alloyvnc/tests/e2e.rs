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
        let step = Step::new();
        let capture = Synth::new(320, 200, Pace::Manual(step.clone()));
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

#[tokio::test]
async fn full_then_incremental_updates_reproduce_the_picture() {
    let rig = Rig::start(None).await;
    rig.frames(1).await;

    let mut c = Client::connect(rig.addr, None).await.unwrap();
    assert_eq!(c.name, "e2e");
    assert_eq!((c.fb.width(), c.fb.height()), (320, 200));
    c.set_encodings(&[encoding::RAW, encoding::COPY_RECT])
        .await
        .unwrap();

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
    let area: i64 = rects.iter().map(|r| r.area()).sum();
    assert!(
        area < 320 * 200 / 2,
        "an incremental update carries only the change: {rects:?}"
    );
    assert_eq!(c.fb.data(), rig.picture());

    // Several frames between requests are coalesced into one update.
    rig.frames(5).await;
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
    assert_eq!(c.fb.data(), rig.picture());
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
    // The acknowledgement arrives on its own; a frame after it is not pushed.
    rig.frames(1).await;
    c.fence(msg::FENCE_REQUEST | msg::FENCE_SYNC_NEXT, b"ping")
        .await
        .unwrap();
    c.request_all(true).await.unwrap();
    c.next_update().await.unwrap();
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
