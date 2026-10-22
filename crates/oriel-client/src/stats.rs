use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Rolling statistics over the last second.
#[derive(Default)]
pub struct Stats {
    frames: VecDeque<FrameSample>,
    pub decoder: String,
    pub codec: String,
    pub resolution: (u32, u32),
    pub keyframes: u64,
}

struct FrameSample {
    at: Instant,
    bytes: usize,
    decode: Duration,
    encode_us: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    pub fps: f32,
    pub mbps: f32,
    pub decode_ms: f32,
    pub encode_ms: f32,
}

impl Stats {
    pub fn record(&mut self, bytes: usize, decode: Duration, encode_us: u32, keyframe: bool) {
        let now = Instant::now();
        self.frames.push_back(FrameSample {
            at: now,
            bytes,
            decode,
            encode_us,
        });
        if keyframe {
            self.keyframes += 1;
        }
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while self
            .frames
            .front()
            .is_some_and(|f| now.duration_since(f.at) > Duration::from_secs(1))
        {
            self.frames.pop_front();
        }
    }

    pub fn summary(&mut self) -> Summary {
        self.trim(Instant::now());
        let n = self.frames.len();
        if n == 0 {
            return Summary::default();
        }
        let bytes: usize = self.frames.iter().map(|f| f.bytes).sum();
        Summary {
            fps: n as f32,
            mbps: bytes as f32 * 8.0 / 1e6,
            decode_ms: self
                .frames
                .iter()
                .map(|f| f.decode.as_secs_f32())
                .sum::<f32>()
                * 1000.0
                / n as f32,
            encode_ms: self.frames.iter().map(|f| f.encode_us as f32).sum::<f32>()
                / 1000.0
                / n as f32,
        }
    }
}

/// Estimates the host clock relative to ours from ping round trips, keeping
/// the sample with the smallest round trip.
pub struct ClockSync {
    epoch: Instant,
    /// host_us - local_us
    offset: Option<i64>,
    best_rtt: Duration,
    samples: u32,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            offset: None,
            best_rtt: Duration::MAX,
            samples: 0,
        }
    }
}

impl ClockSync {
    pub fn local_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_micros() as u64
    }

    pub fn sample(&mut self, sent: Instant, received: Instant, host_us: u64) {
        let rtt = received - sent;
        self.samples += 1;
        // Let the best sample age out slowly in case clocks drift.
        if self.samples.is_multiple_of(30) {
            self.best_rtt = self.best_rtt.saturating_mul(2);
        }
        if rtt <= self.best_rtt {
            self.best_rtt = rtt;
            let mid = self.local_us(sent) + rtt.as_micros() as u64 / 2;
            self.offset = Some(host_us as i64 - mid as i64);
        }
    }

    /// Time from host capture to `at`, if the clocks are synced.
    pub fn since_capture(&self, capture_us: u64, at: Instant) -> Option<Duration> {
        let offset = self.offset?;
        let capture_local = capture_us as i64 - offset;
        let elapsed = self.local_us(at) as i64 - capture_local;
        (elapsed >= 0).then(|| Duration::from_micros(elapsed as u64))
    }
}
