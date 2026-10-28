//! How video frames arrive, reported to the host for automatic bitrate
//! ([`oriel_proto::Datagram::Delivery`]).
//!
//! Each frame's one-way delay is the arrival of its first bytes here minus
//! the host's send time: the time it waited behind earlier data, without
//! its own transmission (a large frame on a slow link would otherwise read
//! as a queue). The two clocks' offset is unknown but constant, so what is
//! reported only uses differences: the delay over its recent minimum (a
//! queue), its slope (a queue filling up, as in WebRTC's congestion
//! control: jitter scatters around a flat line), and the rate frames came
//! in at.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use oriel_proto::{DeliveryReport, FrameHeader};

/// How often to report.
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
/// How far back the minimum delay looks.
const BASELINE_WINDOW: Duration = Duration::from_secs(10);
/// Frames the slope and the delivery rate look at.
const RECENT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
struct Arrival {
    at: Instant,
    /// One-way delay plus the clocks' offset, µs.
    delay: i64,
    bytes: usize,
    keyframe: bool,
}

pub struct DeliveryTracker {
    recent: VecDeque<Arrival>,
    /// Increasing delays with their arrival times: the front is the
    /// window's minimum.
    lows: VecDeque<(Instant, i64)>,
    last_report: Instant,
}

impl Default for DeliveryTracker {
    fn default() -> Self {
        Self {
            recent: VecDeque::new(),
            lows: VecDeque::new(),
            last_report: Instant::now(),
        }
    }
}

impl DeliveryTracker {
    /// Frame `header` (`bytes` long) started arriving at `started_us` (µs
    /// on any steady local clock) and was in full at `at`.
    pub fn arrived(&mut self, header: &FrameHeader, bytes: usize, started_us: i64, at: Instant) {
        let sent = header.capture_us as i64 + header.encode_us as i64;
        let delay = started_us - sent;
        // The frames after a keyframe wait behind it: no baseline.
        if !header.keyframe {
            while self.lows.back().is_some_and(|&(_, d)| d >= delay) {
                self.lows.pop_back();
            }
            self.lows.push_back((at, delay));
        }
        while self
            .lows
            .front()
            .is_some_and(|&(t, _)| at.duration_since(t) > BASELINE_WINDOW)
        {
            self.lows.pop_front();
        }
        self.recent.push_back(Arrival {
            at,
            delay,
            bytes,
            keyframe: header.keyframe,
        });
        while self
            .recent
            .front()
            .is_some_and(|a| at.duration_since(a.at) > RECENT)
        {
            self.recent.pop_front();
        }
    }

    /// A report, once every [`REPORT_EVERY`] while frames arrive.
    pub fn report(&mut self, now: Instant) -> Option<DeliveryReport> {
        if now.duration_since(self.last_report) < REPORT_EVERY {
            return None;
        }
        let &(_, low) = self.lows.front()?;
        self.last_report = now;
        let deltas: Vec<Arrival> = self
            .recent
            .iter()
            .filter(|a| !a.keyframe)
            .copied()
            .collect();
        // The queue: the latest few frames, so one late frame is not one.
        let latest: Vec<i64> = deltas.iter().rev().take(3).map(|a| a.delay).collect();
        let queue = latest.iter().min().map_or(0, |d| d - low).max(0);
        Some(DeliveryReport {
            queue_us: queue.min(u32::MAX as i64) as u32,
            trend_us_per_s: slope(&deltas),
            delivered_kbps: delivered_kbps(&self.recent),
            frames: self.recent.len().min(u16::MAX as usize) as u16,
        })
    }
}

/// Least-squares slope of delay over arrival time, µs per second (0 with
/// too few frames to tell).
fn slope(frames: &[Arrival]) -> i32 {
    if frames.len() < 5 {
        return 0;
    }
    let t0 = frames[0].at;
    let d0 = frames[0].delay;
    let n = frames.len() as f64;
    let xs: Vec<f64> = frames
        .iter()
        .map(|a| a.at.duration_since(t0).as_secs_f64())
        .collect();
    let ys: Vec<f64> = frames.iter().map(|a| (a.delay - d0) as f64).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (x, y) in xs.iter().zip(&ys) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx) * (x - mx);
    }
    // Under 50 ms of spread says nothing about a trend.
    if sxx <= 0.0 || xs.last().copied().unwrap_or(0.0) < 0.05 {
        return 0;
    }
    (sxy / sxx).clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// The rate frames came in at: what arrived after the first frame, over
/// the time since. Below the link's capacity whenever the stream pauses,
/// which is why the host only believes it while frames queue.
fn delivered_kbps(frames: &VecDeque<Arrival>) -> u32 {
    let (Some(first), Some(last)) = (frames.front(), frames.back()) else {
        return 0;
    };
    let span = last.at.duration_since(first.at);
    if frames.len() < 4 || span < Duration::from_millis(30) {
        return 0;
    }
    let bytes: usize = frames.iter().skip(1).map(|a| a.bytes).sum();
    (bytes as f64 * 8.0 / 1000.0 / span.as_secs_f64()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use oriel_proto::Codec;

    fn header(seq: u64, sent_us: u64) -> FrameHeader {
        FrameHeader {
            surface: 1,
            seq,
            capture_us: sent_us,
            encode_us: 0,
            keyframe: false,
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            full_chroma: false,
        }
    }

    /// Frames every 10 ms from a host whose clock is far off ours, each
    /// arriving `extra(n)` µs later than the last-but-steady one.
    fn run(extra: impl Fn(u64) -> i64, bytes: usize) -> DeliveryReport {
        let t0 = Instant::now();
        let mut d = DeliveryTracker::default();
        let mut last = None;
        for n in 0..200u64 {
            let sent = 5_000_000_000 + n * 10_000;
            let arrive_us = n as i64 * 10_000 + 20_000 + extra(n);
            let at = t0 + Duration::from_micros(arrive_us as u64);
            d.arrived(&header(n, sent), bytes, arrive_us, at);
            if let Some(r) = d.report(at) {
                last = Some(r);
            }
        }
        last.expect("reports")
    }

    #[test]
    fn steady_link() {
        let r = run(|_| 0, 50_000);
        assert_eq!(r.queue_us, 0);
        assert_eq!(r.trend_us_per_s, 0);
        // 50 kB every 10 ms: 40 Mbit/s.
        assert!((39_000..=41_000).contains(&r.delivered_kbps), "{r:?}");
    }

    #[test]
    fn jitter_is_flat() {
        let r = run(
            |n| [0, 8_000, 2_000, 12_000, 0, 5_000][(n % 6) as usize],
            50_000,
        );
        assert!(r.trend_us_per_s.abs() < 20_000, "{r:?}");
    }

    #[test]
    fn a_filling_queue_climbs() {
        // From frame 150 each frame waits 2 ms longer: 200 ms per second.
        let r = run(|n| n.saturating_sub(150) as i64 * 2_000, 50_000);
        assert!(r.trend_us_per_s > 150_000, "{r:?}");
        assert!(r.queue_us > 80_000, "{r:?}");
    }
}
