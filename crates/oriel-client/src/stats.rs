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
