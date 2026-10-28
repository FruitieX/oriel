//! Automatic bitrate: follows what the network carries right now, below the
//! client's chosen bitrate.
//!
//! Every quarter second the pipeline reports what it saw. Congestion (the
//! round trip growing over its recent minimum, frames waiting on the client,
//! or packet loss) cuts the bitrate; a clean link raises it again step by
//! step, but only while the stream actually uses what it has.
//!
//! Radio links (5G, busy Wi-Fi) jitter without being full, so the delay
//! that counts grows with the jitter seen over the baseline window, and
//! except for heavy delay or loss a cut needs congestion in several samples
//! in a row. The client's [`RateSensitivity`] sets how much of that applies.
//!
//! Beyond `Quick` (the original behaviour), clients that report how frames
//! arrive ([`DeliveryReport`]) add better signals:
//!
//! - The one-way delay replaces the round trip, whose way back (the
//!   client's uplink, often the worse half of a mobile link) does not
//!   carry video. A queue filling up shows as a delay that climbs steadily
//!   (the trend); jitter scatters around a flat line.
//! - Loss counts only with a queue: loss on a steady delay is radio noise.
//! - ECN congestion marks (QUIC counts them as congestion events without
//!   losses) cut without waiting: a router said it is filling up.
//! - While frames wait and the link carries well under the target, a cut
//!   goes straight to what it carried rather than down in steps.
//!
//! QUIC's congestion window is logged with each cut but not used: video
//! sends a frame at a time, which QUIC sees as application-limited, so the
//! window stops growing at about what the stream uses and says little
//! about what the link could carry.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use oriel_proto::{DeliveryReport, RateSensitivity};

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
    /// QUIC saw ECN congestion marks.
    pub ecn_marked: bool,
    /// The client's latest report, if fresh.
    pub delivery: Option<DeliveryReport>,
    /// What the encoder runs at, kbit/s (0: unknown). One that has to be
    /// reopened for a new bitrate lags behind the target for a moment.
    pub encoder_kbps: u32,
    /// What QUIC's congestion window carries per round trip, kbit/s (0:
    /// unknown); for the log.
    #[expect(dead_code, reason = "logged with the sample")]
    pub cwnd_kbps: f32,
}

/// Why the bitrate was cut, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Delay,
    /// The one-way delay climbing.
    Rising,
    Blocked,
    Loss,
    Ecn,
}

/// How one [`RateSensitivity`] tells congestion from noise.
struct Profile {
    /// Use the newer signals (see the module comment).
    signals: bool,
    /// One-way delay growth that means a queue filling up, ms per second.
    trend: f32,
    /// Delay over the baseline that counts: a share of the baseline, at
    /// least `delay_floor`, and at least `jitter` times the usual jitter.
    delay_share: f32,
    delay_floor: Duration,
    jitter: f32,
    /// Congested samples in a row before a cut.
    persist: u32,
    loss: f32,
    /// Cut at once, without waiting for `persist` samples: this much loss,
    /// or a round trip `heavy_delay` times the baseline and at least
    /// `heavy_margin` margins over it.
    heavy_loss: f32,
    heavy_delay: u32,
    heavy_margin: u32,
    cut: f32,
    heavy_cut: f32,
    hold: Duration,
}

impl Profile {
    fn of(s: RateSensitivity) -> Self {
        match s {
            // What automatic bitrate did before it had a choice.
            RateSensitivity::Quick => Self {
                signals: false,
                trend: 0.0,
                delay_share: 0.5,
                delay_floor: Duration::from_millis(4),
                jitter: 0.0,
                persist: 1,
                loss: 0.03,
                heavy_loss: 0.1,
                heavy_delay: 3,
                heavy_margin: 1,
                cut: 0.8,
                heavy_cut: 0.6,
                hold: Duration::from_secs(2),
            },
            RateSensitivity::Balanced => Self {
                signals: true,
                trend: 40.0,
                delay_share: 0.5,
                delay_floor: Duration::from_millis(6),
                jitter: 3.0,
                persist: 2,
                loss: 0.05,
                heavy_loss: 0.15,
                heavy_delay: 3,
                heavy_margin: 2,
                cut: 0.85,
                heavy_cut: 0.65,
                hold: Duration::from_millis(1500),
            },
            RateSensitivity::Tolerant => Self {
                signals: true,
                trend: 80.0,
                delay_share: 1.0,
                delay_floor: Duration::from_millis(15),
                jitter: 4.0,
                persist: 4,
                loss: 0.1,
                heavy_loss: 0.25,
                heavy_delay: 4,
                heavy_margin: 3,
                cut: 0.9,
                heavy_cut: 0.7,
                hold: Duration::from_secs(1),
            },
        }
    }
}

pub struct RateControl {
    max_kbps: u32,
    target_kbps: u32,
    profile: Profile,
    /// Recent round trips, for the uncongested baseline and the jitter.
    rtts: VecDeque<(Instant, Duration)>,
    /// Congested samples in a row.
    streak: u32,
    /// The queue when the bitrate was last cut, and since when the encoder
    /// runs at that cut.
    queue_at_cut: Duration,
    drain_from: Option<Instant>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
}

/// How long the round-trip baseline looks back.
const BASELINE_WINDOW: Duration = Duration::from_secs(10);
const INCREASE_EVERY: Duration = Duration::from_millis(500);
const DECREASE_EVERY: Duration = Duration::from_millis(300);
/// After a cut (once the encoder runs at it) the queue it found takes a
/// while to drain; until then it only counts as more congestion where it
/// grew.
const DRAIN: Duration = Duration::from_secs(1);

impl RateControl {
    pub fn new(max_kbps: u32, sensitivity: RateSensitivity, now: Instant) -> Self {
        Self {
            max_kbps,
            target_kbps: max_kbps,
            profile: Profile::of(sensitivity),
            rtts: VecDeque::new(),
            streak: 0,
            queue_at_cut: Duration::ZERO,
            drain_from: None,
            last_decrease: None,
            last_increase: now,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    pub fn set_sensitivity(&mut self, sensitivity: RateSensitivity) {
        self.profile = Profile::of(sensitivity);
        self.streak = 0;
    }

    fn min_kbps(&self) -> u32 {
        (self.max_kbps / 10).max(2_000).min(self.max_kbps)
    }

    /// A new maximum from the client; the target follows it down at once.
    pub fn set_max(&mut self, max_kbps: u32) {
        self.max_kbps = max_kbps;
        self.target_kbps = self.target_kbps.min(max_kbps).max(self.min_kbps());
    }

    /// The usual change between successive round trips in the window.
    fn jitter(&self) -> Duration {
        let n = self.rtts.len();
        if n < 2 {
            return Duration::ZERO;
        }
        let sum: Duration = self
            .rtts
            .iter()
            .zip(self.rtts.iter().skip(1))
            .map(|((_, a), (_, b))| a.abs_diff(*b))
            .sum();
        sum / (n - 1) as u32
    }

    /// Feed a quarter second's sample. Returns a new target worth applying
    /// and what caused a cut.
    pub fn update(&mut self, now: Instant, s: Sample) -> Option<(u32, Option<Cause>)> {
        // The jitter before this sample, so that a jump does not excuse
        // itself.
        let jitter = self.jitter();
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
        let p = &self.profile;
        let base = self.rtts.iter().map(|(_, r)| *r).min().unwrap_or(s.rtt);
        // Queues building up somewhere on the path: delay over the baseline
        // by more than this counts.
        let margin = base
            .mul_f32(p.delay_share)
            .max(p.delay_floor)
            .max(jitter.mul_f32(p.jitter));
        // Delay past this cuts at once.
        let heavy_excess = (base * (p.heavy_delay - 1)).max(margin * p.heavy_margin);
        let rtt_excess = s.rtt.saturating_sub(base);
        let oneway = s.delivery.filter(|_| p.signals);
        // The queue the video waits in: one way where the client says,
        // else the round trip's growth.
        let queue = oneway.map_or(rtt_excess, |d| Duration::from_micros(d.queue_us as u64));
        let rising = oneway
            .is_some_and(|d| d.trend_us_per_s as f32 / 1000.0 >= p.trend && queue >= margin / 2);
        let delayed = queue > margin;
        let lossy = s.loss >= p.loss
            && (!p.signals || s.loss >= p.heavy_loss || rising || queue > margin / 2);
        let ecn = p.signals && s.ecn_marked;
        let cause = if lossy {
            Some(Cause::Loss)
        } else if ecn {
            Some(Cause::Ecn)
        } else if delayed {
            Some(Cause::Delay)
        } else if rising {
            Some(Cause::Rising)
        } else if s.blocked {
            Some(Cause::Blocked)
        } else {
            None
        };
        // The queue a cut found only drains once the encoder runs at it.
        if s.encoder_kbps as f32 > self.target_kbps as f32 * 1.05 && self.drain_from.is_some() {
            self.drain_from = Some(now);
        }
        let draining = p.signals
            && self
                .drain_from
                .is_some_and(|t| now.duration_since(t) < DRAIN)
            && queue <= self.queue_at_cut + margin;
        let cause = cause.filter(|c| !draining || !matches!(c, Cause::Delay | Cause::Blocked));
        self.streak = if cause.is_some() { self.streak + 1 } else { 0 };
        let heavy = s.loss >= p.heavy_loss || (queue > heavy_excess && !draining);
        // Frames held back (or a queue filling up) while the link carried
        // well under the target: that was its capacity, cut to it in one
        // go. (What the encoder produced is no measure here: frames held
        // back are never encoded.)
        let carried = oneway
            .map(|d| d.delivered_kbps as f32)
            .filter(|&k| k > 0.0 && k < self.target_kbps as f32 * 0.9 && (s.blocked || rising));

        let old = self.target_kbps;
        if cause.is_some() {
            if (heavy || ecn || self.streak >= p.persist)
                && self
                    .last_decrease
                    .is_none_or(|t| now.duration_since(t) >= DECREASE_EVERY)
            {
                let factor = if heavy { p.heavy_cut } else { p.cut };
                let mut target = old as f32 * factor;
                if let Some(carried) = carried {
                    // Not below a quarter at once: one report can be off.
                    target = target.min(carried * 0.9).max(old as f32 * 0.25);
                }
                self.target_kbps = (target as u32).max(self.min_kbps());
                self.last_decrease = Some(now);
                self.queue_at_cut = queue;
                self.drain_from = Some(now);
            }
        } else if self
            .last_decrease
            .is_none_or(|t| now.duration_since(t) >= p.hold)
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
            Some((self.target_kbps, cause.filter(|_| self.target_kbps < old)))
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
        let mut rc = RateControl::new(50_000, RateSensitivity::Quick, t0);
        // A clean link at full use keeps the maximum.
        assert_eq!(rc.update(at(250), clean(2, 45_000.0)), None);
        assert_eq!(rc.target_kbps(), 50_000);

        // Delay building up: cut at once, but not every sample.
        assert_eq!(
            rc.update(at(500), clean(30, 45_000.0)),
            Some((30_000, Some(Cause::Delay)))
        );
        assert_eq!(rc.update(at(600), clean(30, 30_000.0)), None);
        // Frames waiting on the client count as congestion too.
        let blocked = Sample {
            blocked: true,
            ..clean(2, 30_000.0)
        };
        assert_eq!(
            rc.update(at(900), blocked),
            Some((24_000, Some(Cause::Blocked)))
        );

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

    /// A 5G-like link: 25 ms round trips that wander by ±10 ms.
    fn jittery(n: u64) -> Duration {
        Duration::from_millis([25, 33, 20, 35, 24, 18, 30, 26][(n % 8) as usize])
    }

    #[test]
    fn balanced_rides_out_jitter() {
        let t0 = Instant::now();
        for sensitivity in [RateSensitivity::Balanced, RateSensitivity::Tolerant] {
            let mut rc = RateControl::new(50_000, sensitivity, t0);
            for n in 1..200 {
                let s = Sample {
                    rtt: jittery(n),
                    ..clean(0, 45_000.0)
                };
                rc.update(t0 + Duration::from_millis(n * 250), s);
            }
            assert_eq!(rc.target_kbps(), 50_000, "{sensitivity:?}");
        }
        // Quick takes the same jitter for congestion.
        let mut rc = RateControl::new(50_000, RateSensitivity::Quick, t0);
        for n in 1..200 {
            let s = Sample {
                rtt: jittery(n),
                ..clean(0, 45_000.0)
            };
            rc.update(t0 + Duration::from_millis(n * 250), s);
        }
        assert!(rc.target_kbps() < 50_000);
    }

    #[test]
    fn balanced_cuts_lasting_congestion() {
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_millis(n * 250);
        for (sensitivity, within) in [
            (RateSensitivity::Balanced, 2),
            (RateSensitivity::Tolerant, 4),
        ] {
            let mut rc = RateControl::new(50_000, sensitivity, t0);
            for n in 1..40 {
                rc.update(
                    at(n),
                    Sample {
                        rtt: jittery(n),
                        ..clean(0, 45_000.0)
                    },
                );
            }
            // A queue builds up: the round trip climbs and stays up.
            let mut cut_after = None;
            for k in 1..=8 {
                let s = Sample {
                    rtt: Duration::from_millis(60 + k * 10),
                    ..clean(0, 45_000.0)
                };
                if rc.update(at(40 + k), s).is_some() {
                    cut_after = Some(k);
                    break;
                }
            }
            assert!(
                cut_after.is_some_and(|k| k <= within),
                "{sensitivity:?}: {cut_after:?}"
            );
            // Heavy loss cuts on the first sample.
            let mut rc = RateControl::new(50_000, sensitivity, t0);
            let s = Sample {
                loss: 0.5,
                ..clean(25, 45_000.0)
            };
            assert!(matches!(rc.update(at(1), s), Some((_, Some(Cause::Loss)))));
        }
    }

    fn report(queue_ms: u32, trend_ms_per_s: i32, delivered: u32) -> Option<DeliveryReport> {
        Some(DeliveryReport {
            queue_us: queue_ms * 1000,
            trend_us_per_s: trend_ms_per_s * 1000,
            delivered_kbps: delivered,
            frames: 40,
        })
    }

    /// Balanced with a client reporting: a few seconds of settling on a
    /// 25 ms link, then `then` for up to 8 quarters. Returns the first
    /// change.
    fn after_settling(then: Sample) -> Option<(u32, Option<Cause>)> {
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_millis(n * 250);
        let mut rc = RateControl::new(50_000, RateSensitivity::Balanced, t0);
        for n in 1..40 {
            let s = Sample {
                delivery: report(1, 0, 20_000),
                ..clean(25, 45_000.0)
            };
            assert_eq!(rc.update(at(n), s), None);
        }
        (40..48).find_map(|n| rc.update(at(n), then))
    }

    #[test]
    fn uplink_delay_is_not_congestion() {
        // The round trip doubles, but the video's way is clear.
        let s = Sample {
            delivery: report(1, 0, 20_000),
            ..clean(60, 45_000.0)
        };
        assert_eq!(after_settling(s), None);
    }

    #[test]
    fn loss_needs_a_queue() {
        let noise = Sample {
            loss: 0.06,
            delivery: report(1, 0, 20_000),
            ..clean(25, 45_000.0)
        };
        assert_eq!(after_settling(noise), None);
        let full = Sample {
            loss: 0.06,
            delivery: report(10, 60, 20_000),
            ..clean(25, 45_000.0)
        };
        assert!(matches!(after_settling(full), Some((_, Some(Cause::Loss)))));
    }

    #[test]
    fn a_climbing_delay_cuts() {
        let s = Sample {
            delivery: report(8, 60, 44_000),
            ..clean(25, 45_000.0)
        };
        assert!(matches!(after_settling(s), Some((_, Some(Cause::Rising)))));
    }

    #[test]
    fn ecn_marks_cut_at_once() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(50_000, RateSensitivity::Tolerant, t0);
        let s = Sample {
            ecn_marked: true,
            ..clean(25, 45_000.0)
        };
        assert_eq!(
            rc.update(t0 + Duration::from_millis(250), s),
            Some((45_000, Some(Cause::Ecn)))
        );
    }

    #[test]
    fn cuts_to_what_the_link_carried() {
        // The encoder made 45 Mbit/s; 12 got through while frames waited.
        let s = Sample {
            blocked: true,
            delivery: report(30, 100, 12_000),
            ..clean(25, 45_000.0)
        };
        let (kbps, _) = after_settling(s).expect("a cut");
        assert_eq!(kbps, 12_500, "a quarter of 50 at most at once");
        let s = Sample {
            blocked: true,
            delivery: report(30, 100, 30_000),
            ..clean(25, 45_000.0)
        };
        assert_eq!(after_settling(s).expect("a cut").0, 27_000);
    }

    #[test]
    fn lets_the_queue_drain() {
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_millis(n * 250);
        let mut rc = RateControl::new(50_000, RateSensitivity::Balanced, t0);
        let s = |queue, delivered| Sample {
            blocked: true,
            delivery: report(queue, 0, delivered),
            ..clean(25, 8_000.0)
        };
        rc.update(at(1), s(60, 30_000));
        assert_eq!(rc.target_kbps(), 27_000);
        // The queue it found stays a while: no further cuts.
        for n in 2..=4 {
            assert_eq!(rc.update(at(n), s(60, 26_000)), None);
        }
        // Still there after a second: the link carries less still.
        assert!(rc.update(at(6), s(60, 20_000)).is_some());
        // A queue that grows cuts at once.
        let mut rc = RateControl::new(50_000, RateSensitivity::Balanced, t0);
        rc.update(at(1), s(60, 30_000));
        assert!(rc.update(at(3), s(90, 20_000)).is_some());
    }

    #[test]
    fn waits_for_the_encoder() {
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_millis(n * 250);
        let mut rc = RateControl::new(50_000, RateSensitivity::Balanced, t0);
        let s = |encoder_kbps| Sample {
            blocked: true,
            delivery: report(60, 0, 30_000),
            encoder_kbps,
            ..clean(25, 8_000.0)
        };
        rc.update(at(1), s(50_000));
        assert_eq!(rc.target_kbps(), 27_000);
        // The encoder is reopened at the cut only after two seconds.
        for n in 2..=8 {
            assert_eq!(rc.update(at(n), s(50_000)), None, "{n}");
        }
        for n in 9..=11 {
            assert_eq!(rc.update(at(n), s(27_000)), None, "{n}");
        }
    }

    #[test]
    fn still_screens_do_not_probe() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(50_000, RateSensitivity::Quick, t0);
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
        let mut rc = RateControl::new(50_000, RateSensitivity::Quick, t0);
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
