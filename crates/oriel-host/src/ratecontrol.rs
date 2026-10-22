//! Automatic bitrate: follows what the network carries right now, below the
//! client's chosen bitrate.
//!
//! Every quarter second the pipeline reports what it saw. Congestion (the
//! round trip growing over its recent minimum, frames waiting on the client,
//! or packet loss) cuts the bitrate at once; a clean link raises it again
//! step by step, but only while the stream actually uses what it has.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// What the last quarter second looked like.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub rtt: Duration,
    /// Frames had to wait for the client to catch up.
    pub blocked: bool,
    /// Share of packets lost, 0–1.
    pub loss: f32,
    /// What the encoder produced, kbit/s.
    pub sent_kbps: f32,
}

pub struct RateControl {
    max_kbps: u32,
    target_kbps: u32,
    /// Recent round trips, for the uncongested baseline.
    rtts: VecDeque<(Instant, Duration)>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
}

/// How long the round-trip baseline looks back.
const BASELINE_WINDOW: Duration = Duration::from_secs(10);
/// Wait this long after a cut before raising the bitrate again.
const HOLD_AFTER_DECREASE: Duration = Duration::from_secs(2);
const INCREASE_EVERY: Duration = Duration::from_millis(500);
const DECREASE_EVERY: Duration = Duration::from_millis(300);

impl RateControl {
    pub fn new(max_kbps: u32, now: Instant) -> Self {
        Self {
            max_kbps,
            target_kbps: max_kbps,
            rtts: VecDeque::new(),
            last_decrease: None,
            last_increase: now,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    fn min_kbps(&self) -> u32 {
        (self.max_kbps / 10).max(2_000).min(self.max_kbps)
    }

    /// A new maximum from the client; the target follows it down at once.
    pub fn set_max(&mut self, max_kbps: u32) {
        self.max_kbps = max_kbps;
        self.target_kbps = self.target_kbps.min(max_kbps).max(self.min_kbps());
    }

    /// Feed a quarter second's sample. Returns a new target worth applying.
    pub fn update(&mut self, now: Instant, s: Sample) -> Option<u32> {
        if !s.rtt.is_zero() {
            self.rtts.push_back((now, s.rtt));
        }
        while self
            .rtts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > BASELINE_WINDOW)
        {
            self.rtts.pop_front();
        }
        let base = self.rtts.iter().map(|(_, r)| *r).min().unwrap_or(s.rtt);
        // Queues building up somewhere on the path.
        let delayed = s.rtt > base + (base / 2).max(Duration::from_millis(4));
        let congested = delayed || s.blocked || s.loss >= 0.03;

        let old = self.target_kbps;
        if congested {
            if self
                .last_decrease
                .is_none_or(|t| now.duration_since(t) >= DECREASE_EVERY)
            {
                // Heavy loss or delay: cut harder.
                let factor = if s.loss >= 0.1 || s.rtt > base * 3 {
                    0.6
                } else {
                    0.8
                };
                self.target_kbps = ((old as f32 * factor) as u32).max(self.min_kbps());
                self.last_decrease = Some(now);
            }
        } else if self
            .last_decrease
            .is_none_or(|t| now.duration_since(t) >= HOLD_AFTER_DECREASE)
            && now.duration_since(self.last_increase) >= INCREASE_EVERY
            // Only probe upwards while the stream uses its budget; a still
            // screen says nothing about what the link carries.
            && s.sent_kbps >= old as f32 * 0.5
        {
            self.target_kbps = ((old as f32 * 1.1) as u32 + 500).min(self.max_kbps);
            self.last_increase = now;
        }
        // Small steps are not worth an encoder change.
        let changed = self.target_kbps.abs_diff(old) * 20 >= old;
        if changed {
            Some(self.target_kbps)
        } else {
            self.target_kbps = old;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(rtt_ms: u64, sent: f32) -> Sample {
        Sample {
            rtt: Duration::from_millis(rtt_ms),
            sent_kbps: sent,
            ..Default::default()
        }
    }

    #[test]
    fn backs_off_and_recovers() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut rc = RateControl::new(50_000, t0);
        // A clean link at full use keeps the maximum.
        assert_eq!(rc.update(at(250), clean(2, 45_000.0)), None);
        assert_eq!(rc.target_kbps(), 50_000);

        // Delay building up: cut at once, but not every sample.
        assert_eq!(rc.update(at(500), clean(30, 45_000.0)), Some(30_000));
        assert_eq!(rc.update(at(600), clean(30, 30_000.0)), None);
        // Frames waiting on the client count as congestion too.
        let blocked = Sample {
            blocked: true,
            ..clean(2, 30_000.0)
        };
        assert_eq!(rc.update(at(900), blocked), Some(24_000));

        // Held for a while after a cut, then raised step by step.
        assert_eq!(rc.update(at(1500), clean(2, 24_000.0)), None);
        let mut t = 3000;
        let mut last = rc.target_kbps();
        while rc.target_kbps() < 50_000 {
            rc.update(at(t), clean(2, rc.target_kbps() as f32));
            assert!(rc.target_kbps() >= last);
            last = rc.target_kbps();
            t += 500;
            assert!(t < 20_000, "recovers within seconds");
        }
    }

    #[test]
    fn still_screens_do_not_probe() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(50_000, t0);
        let loss = Sample {
            loss: 0.2,
            ..clean(2, 40_000.0)
        };
        rc.update(t0 + Duration::from_millis(250), loss);
        let cut = rc.target_kbps();
        assert!(cut < 50_000);
        // Nothing to send: the target stays where it is.
        for n in 0..40 {
            rc.update(t0 + Duration::from_millis(3000 + n * 500), clean(2, 100.0));
        }
        assert_eq!(rc.target_kbps(), cut);
    }

    #[test]
    fn floor_and_new_maximum() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(50_000, t0);
        for n in 1..40 {
            let loss = Sample {
                loss: 0.5,
                ..clean(2, 10_000.0)
            };
            rc.update(t0 + Duration::from_millis(n * 400), loss);
        }
        assert_eq!(rc.target_kbps(), 5_000);
        rc.set_max(3_000);
        assert_eq!(rc.target_kbps(), 3_000);
    }
}
