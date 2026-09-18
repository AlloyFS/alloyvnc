//! How much a client can be sent before waiting to hear from it.
//!
//! TCP will accept anything: a write to a socket returns as soon as the
//! kernel has taken a copy, long after the link has stopped keeping up. A
//! server that writes whenever it has something to say therefore builds a
//! queue it cannot see, in its own send buffer, in a router's, in the
//! client's receive buffer, and every update sitting in that queue is stale
//! before it arrives. On a LAN nothing shows; over a slow link the picture
//! runs seconds behind the desk and every keystroke looks broken.
//!
//! The window is on **bytes in flight**, not on frames. Frames are not the
//! unit the link cares about: one frame of a video is a megabyte and one of
//! a cursor blink is two hundred bytes, and a link that carries thirty of
//! the second a second cannot carry one of the first. Bytes are what a queue
//! is made of.
//!
//! Measuring them needs the client's help, which is what RFB's fence
//! (pseudo-encoding -312, message 248) is for: the server sends one carrying
//! a marker and asks for it back, and the client echoes it once it has
//! processed everything sent before it. The round trip goes through the
//! whole queue, so it says both how long the path is and how far behind the
//! client has fallen. A client that does not list the extension is given no
//! window at all: there is no way to measure one, and refusing to send would
//! be worse than sending too much.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Where a window may not go, whatever the measurements say. A window of
/// nothing is a session that never recovers.
const MINIMUM_WINDOW: u64 = 16 * 1024;
/// Four megabytes is a 1080p screenful of Raw; past that the window is not
/// what limits anything.
const MAXIMUM_WINDOW: u64 = 4 * 1024 * 1024;

/// A round trip no more than this much longer than the shortest one seen
/// means the path is clear and the window can open.
const CLEAR: Duration = Duration::from_millis(25);
/// This much longer means a queue has formed and the window must close.
const QUEUED: Duration = Duration::from_millis(100);

/// Round trips open at once. More than a few and each one is measuring the
/// ones in front of it rather than the link.
const MAX_OUTSTANDING: usize = 3;
/// Bytes written, or time passed, before another round trip is worth
/// starting. Whichever comes first.
const PING_BYTES: u64 = 16 * 1024;
const PING_INTERVAL: Duration = Duration::from_millis(100);
/// A round trip older than this is written off: a client that answers after
/// five seconds is saying nothing that can be used.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the shortest round trip is believed. A route that changes for
/// the better should not be judged for ever against a path that is gone.
const BASE_LIFETIME: Duration = Duration::from_secs(60);

/// Where a window starts and how fast it opens. Session configuration
/// rather than constants, so a test can start small and watch it move.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub initial_window: u64,
    pub growth: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            initial_window: 64 * 1024,
            growth: 4 * 1024,
        }
    }
}

struct Ping {
    seq: u64,
    /// Bytes written to this client when the fence went out.
    at_bytes: u64,
    at: Instant,
}

/// One client's share of the link.
pub struct Congestion {
    /// Whether the client listed the Fence extension. Without it nothing
    /// here can be measured and nothing is held back.
    measured: bool,
    limits: Limits,
    window: u64,
    written: u64,
    acked: u64,
    /// The last update's size, as the only guess available at the next one's.
    last_update: u64,
    pings: VecDeque<Ping>,
    next_seq: u64,
    last_ping: Option<Instant>,
    last_ping_bytes: u64,
    /// The shortest round trip seen, and when it was seen.
    base: Option<(Duration, Instant)>,
    /// The round trip, smoothed, for the log and the stats.
    rtt: Option<Duration>,
    /// Round trips written off unanswered.
    pub lost: u64,
}

impl Congestion {
    pub fn new(limits: Limits) -> Congestion {
        Congestion {
            measured: false,
            limits,
            window: limits.initial_window.clamp(MINIMUM_WINDOW, MAXIMUM_WINDOW),
            written: 0,
            acked: 0,
            last_update: 0,
            pings: VecDeque::new(),
            next_seq: 1,
            last_ping: None,
            last_ping_bytes: 0,
            base: None,
            rtt: None,
            lost: 0,
        }
    }

    /// The client listed the Fence extension, so the link can be measured.
    pub fn measure(&mut self) {
        self.measured = true;
    }

    pub fn measured(&self) -> bool {
        self.measured
    }

    pub fn in_flight(&self) -> u64 {
        self.written.saturating_sub(self.acked)
    }

    pub fn window(&self) -> u64 {
        self.window
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    pub fn base_rtt(&self) -> Option<Duration> {
        self.base.map(|(d, _)| d)
    }

    pub fn outstanding(&self) -> usize {
        self.pings.len()
    }

    /// Whether another update may be built and handed to the writer.
    ///
    /// Nothing outstanding means go, whatever the size: a client that has
    /// caught up is never made to wait. Otherwise the next update has to fit
    /// the window beside what is already in flight.
    pub fn may_send(&self) -> bool {
        if !self.measured {
            return true;
        }
        let flight = self.in_flight();
        flight == 0 || flight + self.last_update <= self.window
    }

    /// Count an update handed to the writer.
    pub fn wrote(&mut self, bytes: u64) {
        self.written += bytes;
        self.last_update = bytes;
    }

    /// Whether a round trip is worth starting now.
    pub fn ping_due(&self, now: Instant) -> bool {
        if !self.measured || self.pings.len() >= MAX_OUTSTANDING {
            return false;
        }
        let by_bytes = self.written.saturating_sub(self.last_ping_bytes) >= PING_BYTES;
        let by_time = self
            .last_ping
            .is_none_or(|t| now.saturating_duration_since(t) >= PING_INTERVAL);
        by_bytes || by_time
    }

    /// Start a round trip. The sequence number goes in the fence's payload
    /// and comes back in the client's echo.
    pub fn ping(&mut self, now: Instant) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pings.push_back(Ping {
            seq,
            at_bytes: self.written,
            at: now,
        });
        self.last_ping = Some(now);
        self.last_ping_bytes = self.written;
        seq
    }

    /// The client echoed one of our fences. `false` if it was not ours.
    pub fn pong(&mut self, seq: u64, now: Instant) -> bool {
        // An echo confirms every fence before it as well: a client processes
        // its input in order, so anything older has been through too.
        let Some(position) = self.pings.iter().position(|p| p.seq == seq) else {
            return false;
        };
        let ping = self
            .pings
            .drain(..=position)
            .next_back()
            .expect("the ping just found");
        self.acked = self.acked.max(ping.at_bytes);
        let sample = now.saturating_duration_since(ping.at);
        self.rtt = Some(match self.rtt {
            // Seven eighths of the old and an eighth of the new, which is
            // TCP's own smoothing: one slow round trip should not move it.
            Some(old) => (old * 7 + sample) / 8,
            None => sample,
        });
        let stale = self
            .base
            .is_none_or(|(d, at)| sample < d || now.saturating_duration_since(at) > BASE_LIFETIME);
        if stale {
            self.base = Some((sample, now));
        }
        self.resize(sample);
        true
    }

    /// Move the window on one measurement.
    fn resize(&mut self, sample: Duration) {
        let base = self.base.map_or(sample, |(d, _)| d);
        let over = sample.saturating_sub(base);
        if over < CLEAR {
            self.window += self.limits.growth;
        } else if over > QUEUED {
            self.window /= 2;
        }
        self.window = self.window.clamp(MINIMUM_WINDOW, MAXIMUM_WINDOW);
    }

    /// Write off round trips nothing came back for. Their bytes stay counted
    /// as in flight, which is the safe way to be wrong: a client that has
    /// gone quiet is sent less rather than more.
    pub fn expire(&mut self, now: Instant) {
        while let Some(front) = self.pings.front() {
            if now.saturating_duration_since(front.at) <= PING_TIMEOUT {
                break;
            }
            self.pings.pop_front();
            self.lost += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured() -> Congestion {
        let mut c = Congestion::new(Limits::default());
        c.measure();
        c
    }

    /// One round trip of `rtt`, carrying `bytes`.
    fn round(c: &mut Congestion, now: &mut Instant, bytes: u64, rtt: Duration) {
        c.wrote(bytes);
        let seq = c.ping(*now);
        *now += rtt;
        assert!(c.pong(seq, *now));
    }

    #[test]
    fn a_clear_path_opens_the_window_a_step_at_a_time() {
        let mut c = measured();
        let mut now = Instant::now();
        let start = c.window();
        for _ in 0..5 {
            round(&mut c, &mut now, 8 * 1024, Duration::from_millis(5));
        }
        assert_eq!(c.window(), start + 5 * Limits::default().growth);
        assert_eq!(c.in_flight(), 0, "every byte was answered");
        assert!(c.may_send());
    }

    #[test]
    fn a_queue_forming_halves_it() {
        let mut c = measured();
        let mut now = Instant::now();
        round(&mut c, &mut now, 8 * 1024, Duration::from_millis(5));
        let opened = c.window();
        // The path itself is 5 ms; this round trip took 205.
        round(&mut c, &mut now, 8 * 1024, Duration::from_millis(205));
        assert_eq!(c.window(), opened / 2);
    }

    #[test]
    fn the_middle_ground_leaves_it_alone() {
        let mut c = measured();
        let mut now = Instant::now();
        round(&mut c, &mut now, 8 * 1024, Duration::from_millis(5));
        let opened = c.window();
        // Over the clear mark, under the queued one: neither reading is
        // strong enough to act on.
        round(&mut c, &mut now, 8 * 1024, Duration::from_millis(55));
        assert_eq!(c.window(), opened);
    }

    #[test]
    fn the_window_keeps_to_its_floor() {
        let mut c = measured();
        let mut now = Instant::now();
        round(&mut c, &mut now, 1024, Duration::from_millis(1));
        for _ in 0..20 {
            round(&mut c, &mut now, 1024, Duration::from_millis(500));
        }
        assert_eq!(c.window(), MINIMUM_WINDOW, "a client is never starved outright");
    }

    #[test]
    fn the_window_keeps_to_its_ceiling() {
        let mut c = Congestion::new(Limits {
            initial_window: 64 * 1024,
            growth: 1024 * 1024,
        });
        c.measure();
        let mut now = Instant::now();
        for _ in 0..20 {
            round(&mut c, &mut now, 1024, Duration::from_millis(1));
        }
        assert_eq!(c.window(), MAXIMUM_WINDOW);
    }

    #[test]
    fn sending_stops_when_the_window_is_full() {
        let mut c = Congestion::new(Limits {
            initial_window: 32 * 1024,
            growth: 4 * 1024,
        });
        c.measure();
        assert!(c.may_send(), "nothing is outstanding");
        c.wrote(30 * 1024);
        let seq = c.ping(Instant::now());
        // Thirty kilobytes out, and the next update guessed at thirty too:
        // sixty does not fit a window of thirty-two.
        assert!(!c.may_send(), "in flight {} window {}", c.in_flight(), c.window());
        c.pong(seq, Instant::now());
        assert!(c.may_send(), "the echo cleared it");
    }

    #[test]
    fn a_client_without_the_extension_is_never_held_back() {
        let mut c = Congestion::new(Limits::default());
        c.wrote(100 * 1024 * 1024);
        assert!(!c.measured());
        assert!(c.may_send());
        assert!(!c.ping_due(Instant::now()), "and there is nothing to ping with");
    }

    #[test]
    fn a_round_trip_nothing_answers_is_written_off() {
        let mut c = measured();
        let start = Instant::now();
        c.wrote(8 * 1024);
        c.ping(start);
        assert_eq!(c.outstanding(), 1);
        c.expire(start + Duration::from_secs(1));
        assert_eq!(c.outstanding(), 1, "not yet");
        c.expire(start + PING_TIMEOUT + Duration::from_millis(1));
        assert_eq!(c.outstanding(), 0);
        assert_eq!(c.lost, 1);
        assert_eq!(c.in_flight(), 8 * 1024, "its bytes are still out there");
    }

    #[test]
    fn a_ping_is_due_on_bytes_or_on_time_and_never_past_three() {
        let mut c = measured();
        let start = Instant::now();
        assert!(c.ping_due(start), "the first one is always due");
        c.ping(start);
        assert!(!c.ping_due(start), "not without bytes or time");
        c.wrote(PING_BYTES);
        assert!(c.ping_due(start), "enough bytes have gone");
        c.ping(start);
        assert!(c.ping_due(start + PING_INTERVAL), "enough time has passed");
        c.ping(start + PING_INTERVAL);
        assert_eq!(c.outstanding(), 3);
        assert!(!c.ping_due(start + Duration::from_secs(1)), "three is the limit");
    }

    #[test]
    fn an_echo_answers_every_fence_before_it() {
        let mut c = measured();
        let now = Instant::now();
        c.wrote(1000);
        let first = c.ping(now);
        c.wrote(1000);
        let second = c.ping(now);
        assert_eq!(c.outstanding(), 2);
        c.pong(second, now + Duration::from_millis(5));
        assert_eq!(c.outstanding(), 0, "the older one went with it");
        assert_eq!(c.in_flight(), 0);
        assert!(
            !c.pong(first, now + Duration::from_millis(9)),
            "and is not answered twice"
        );
    }
}
