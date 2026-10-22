//! When to show each decoded frame: right away (lowest latency), or after a
//! small buffer that evens out network jitter.
//!
//! A frame is due at its capture time on the host plus the fastest delay
//! seen recently (which absorbs the unknown clock offset and its drift)
//! plus the buffer. Frames later than that are shown at once. In automatic
//! mode the buffer is the delay that 95% of recent frames met.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Smoothing {
    /// Show the newest frame at once.
    Off,
    /// Buffer as much as recent jitter needs.
    Auto,
    Fixed(Duration),
}

impl Smoothing {
    /// From the config: "off", "auto" or milliseconds.
    pub fn parse(s: &str) -> Self {
        match s {
            "auto" => Self::Auto,
            ms => match ms.parse::<u64>() {
                Ok(ms) if ms > 0 => Self::Fixed(Duration::from_millis(ms.min(MAX_DELAY_MS))),
                _ => Self::Off,
            },
        }
    }
}

/// Frames held at most; decoders keep this many surfaces spare.
pub const MAX_QUEUED: usize = 12;
const MAX_DELAY_MS: u64 = 100;
/// How far back the fastest delay is looked for.
const BASE_WINDOW: Duration = Duration::from_secs(5);
/// How far back the automatic buffer looks.
const AUTO_WINDOW: Duration = Duration::from_secs(3);

pub struct Playout<T> {
    epoch: Instant,
    smoothing: Smoothing,
    /// (capture time on the host, item), oldest first.
    frames: VecDeque<(u64, T)>,
    /// (arrival, arrival minus capture), µs, for the fastest delay.
    transits: VecDeque<(i64, i64)>,
    auto_delay_us: f64,
    /// Frames since the last `take_counts`, and how many came too late.
    arrived: u32,
    late: u32,
}

impl<T> Playout<T> {
    pub fn new(smoothing: Smoothing) -> Self {
        Self {
            epoch: Instant::now(),
            smoothing,
            frames: VecDeque::new(),
            transits: VecDeque::new(),
            auto_delay_us: 0.0,
            arrived: 0,
            late: 0,
        }
    }

    pub fn smoothing(&self) -> Smoothing {
        self.smoothing
    }

    pub fn set_smoothing(&mut self, smoothing: Smoothing) {
        self.smoothing = smoothing;
    }

    fn us(&self, at: Instant) -> i64 {
        at.saturating_duration_since(self.epoch).as_micros() as i64
    }

    /// The buffer added on top of the fastest delay.
    pub fn delay(&self) -> Duration {
        match self.smoothing {
            Smoothing::Off => Duration::ZERO,
            Smoothing::Fixed(d) => d,
            Smoothing::Auto => Duration::from_micros(self.auto_delay_us as u64),
        }
    }

    fn base(&self) -> i64 {
        self.transits.iter().map(|(_, t)| *t).min().unwrap_or(0)
    }

    fn due_us(&self, capture_us: u64) -> i64 {
        capture_us as i64 + self.base() + self.delay().as_micros() as i64
    }

    /// A frame captured at `capture_us` (host clock) is ready at `now`.
    pub fn push(&mut self, capture_us: u64, now: Instant, item: T) {
        let now_us = self.us(now);
        let transit = now_us - capture_us as i64;
        self.transits.push_back((now_us, transit));
        while self
            .transits
            .front()
            .is_some_and(|(at, _)| now_us - at > BASE_WINDOW.as_micros() as i64)
        {
            self.transits.pop_front();
        }
        if self.smoothing == Smoothing::Auto {
            self.update_auto(now_us);
        }
        self.arrived += 1;
        // More than a millisecond past its slot: the buffer did not cover it.
        if self.smoothing != Smoothing::Off && self.due_us(capture_us) < now_us - 1000 {
            self.late += 1;
        }
        // In capture order, should frames ever arrive out of it.
        let at = self.frames.partition_point(|(c, _)| *c <= capture_us);
        self.frames.insert(at, (capture_us, item));
        while self.frames.len() > MAX_QUEUED {
            self.frames.pop_front();
        }
    }

    /// Raise the automatic buffer at once to what 95% of recent frames
    /// needed; lower it slowly.
    fn update_auto(&mut self, now_us: i64) {
        let base = self.base();
        let mut excess: Vec<i64> = self
            .transits
            .iter()
            .filter(|(at, _)| now_us - at <= AUTO_WINDOW.as_micros() as i64)
            .map(|(_, t)| t - base)
            .collect();
        if excess.len() < 10 {
            return;
        }
        excess.sort_unstable();
        let p95 = excess[excess.len() * 95 / 100] as f64 + 1000.0;
        let target = p95.min(MAX_DELAY_MS as f64 * 1000.0);
        if target > self.auto_delay_us {
            self.auto_delay_us = target;
        } else {
            self.auto_delay_us += (target - self.auto_delay_us) * 0.02;
        }
    }

    /// The frame to show now, if any: the newest one that is due (older due
    /// frames are skipped).
    pub fn pop_due(&mut self, now: Instant) -> Option<T> {
        if self.smoothing == Smoothing::Off {
            return self.frames.drain(..).last().map(|(_, item)| item);
        }
        let now_us = self.us(now);
        let mut shown = None;
        while let Some((capture_us, _)) = self.frames.front() {
            if self.due_us(*capture_us) > now_us {
                break;
            }
            shown = self.frames.pop_front().map(|(_, item)| item);
        }
        shown
    }

    /// When the next queued frame is due.
    pub fn next_due(&self) -> Option<Instant> {
        if self.smoothing == Smoothing::Off {
            return None;
        }
        let (capture_us, _) = self.frames.front()?;
        Some(self.epoch + Duration::from_micros(self.due_us(*capture_us).max(0) as u64))
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Frames that arrived, and how many of them too late for the buffer,
    /// since the last call.
    pub fn take_counts(&mut self) -> (u32, u32) {
        (
            std::mem::take(&mut self.arrived),
            std::mem::take(&mut self.late),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_US: u64 = 16_667;

    /// Frames captured every 16.7 ms that arrive with `extra(n)` µs of
    /// jitter; returns when each was shown.
    fn run(smoothing: Smoothing, extra: impl Fn(u64) -> u64) -> Vec<(u64, Instant)> {
        let mut p = Playout::new(smoothing);
        let t0 = p.epoch;
        let at = |us: u64| t0 + Duration::from_micros(us);
        // Host clock 1 s ahead of ours, 5 ms of fastest delay.
        let offset = 1_000_000u64;
        // Frames are received and decoded in order: a late one holds up
        // the next.
        let mut events: Vec<(u64, u64)> = Vec::new();
        let mut last = 0;
        for n in 0..300 {
            last = (n * FRAME_US + 5_000 + extra(n)).max(last);
            events.push((n, last));
        }
        let mut shown = Vec::new();
        let mut i = 0;
        // Step through time in 1 ms ticks.
        for t in (0..300 * FRAME_US + 200_000).step_by(1000) {
            while i < events.len() && events[i].1 <= t {
                let (n, _) = events[i];
                p.push(n * FRAME_US + offset, at(t), n);
                i += 1;
            }
            if let Some(n) = p.pop_due(at(t)) {
                shown.push((n, at(t)));
            }
        }
        shown
    }

    /// Spread of the intervals between shown frames, ms.
    fn unevenness(shown: &[(u64, Instant)]) -> f32 {
        let gaps: Vec<f32> = shown
            .windows(2)
            .skip(60)
            .map(|w| (w[1].1 - w[0].1).as_secs_f32() * 1000.0)
            .collect();
        let mean = gaps.iter().sum::<f32>() / gaps.len() as f32;
        (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f32>() / gaps.len() as f32).sqrt()
    }

    // Every third frame 20 ms late.
    fn jittery(n: u64) -> u64 {
        if n.is_multiple_of(3) { 20_000 } else { 0 }
    }

    #[test]
    fn off_shows_frames_as_they_come() {
        let shown = run(Smoothing::Off, jittery);
        assert!(unevenness(&shown) > 5.0);
    }

    #[test]
    fn buffer_evens_out_jitter() {
        let fixed = run(Smoothing::Fixed(Duration::from_millis(25)), jittery);
        assert!(unevenness(&fixed) < 1.5, "{}", unevenness(&fixed));
        // Every frame shown, in order.
        assert!(fixed.len() >= 295);
        assert!(fixed.windows(2).all(|w| w[1].0 > w[0].0));

        let auto = run(Smoothing::Auto, jittery);
        assert!(unevenness(&auto) < 1.5, "{}", unevenness(&auto));
    }

    #[test]
    fn auto_stays_small_on_a_steady_link() {
        let mut p = Playout::new(Smoothing::Auto);
        let t0 = p.epoch;
        for n in 0..200u64 {
            let t = t0 + Duration::from_micros(n * FRAME_US + 5_000 + n % 2 * 300);
            p.push(n * FRAME_US, t, n);
            p.pop_due(t);
        }
        assert!(p.delay() < Duration::from_millis(2), "{:?}", p.delay());
    }

    #[test]
    fn parse() {
        assert_eq!(Smoothing::parse("off"), Smoothing::Off);
        assert_eq!(Smoothing::parse("auto"), Smoothing::Auto);
        assert_eq!(
            Smoothing::parse("35"),
            Smoothing::Fixed(Duration::from_millis(35))
        );
        assert_eq!(
            Smoothing::parse("900"),
            Smoothing::Fixed(Duration::from_millis(100))
        );
    }
}
