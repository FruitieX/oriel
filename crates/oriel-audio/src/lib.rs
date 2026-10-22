//! Audio: system sound capture on the host, Opus encoding, and low-latency
//! playback with a small jitter buffer on the client.
//!
//! Audio travels as 10 ms Opus packets in unreliable datagrams; the
//! decoder conceals lost packets.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet (10 ms).
pub const FRAME: usize = 480;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// What the default output device plays (WASAPI loopback on Windows).
    SystemOutput,
    /// A short beep every second, for testing.
    TestTone,
}

/// Stops capturing when dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start capturing; `on_packet` receives each encoded 10 ms Opus packet.
pub fn start_capture(
    source: CaptureSource,
    bitrate: i32,
    mut on_packet: impl FnMut(Vec<u8>) + Send + 'static,
) -> Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("oriel-audio-capture".into())
        .spawn(move || {
            let mut encoder = match opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            ) {
                Ok(e) => e,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                    return;
                }
            };
            let _ = encoder.set_bitrate(opus::Bitrate::Bits(bitrate));
            let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
            // The stream must stay on the thread that created it.
            let _stream = match source {
                CaptureSource::SystemOutput => match open_loopback(tx) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                },
                CaptureSource::TestTone => {
                    let stop = thread_stop.clone();
                    std::thread::spawn(move || tone(tx, stop));
                    None
                }
            };
            let _ = ready_tx.send(Ok(()));
            let mut pending: Vec<f32> = Vec::with_capacity(FRAME * CHANNELS * 4);
            let mut out = vec![0u8; 1500];
            while !thread_stop.load(Ordering::Relaxed) {
                let Ok(samples) = rx.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                pending.extend_from_slice(&samples);
                while pending.len() >= FRAME * CHANNELS {
                    match encoder.encode_float(&pending[..FRAME * CHANNELS], &mut out) {
                        Ok(n) => on_packet(out[..n].to_vec()),
                        Err(e) => tracing::warn!("opus encode failed: {e}"),
                    }
                    pending.drain(..FRAME * CHANNELS);
                }
            }
        })?;
    ready_rx.recv().context("audio capture thread exited")??;
    Ok(Capture {
        stop,
        thread: Some(thread),
    })
}

/// Converts arbitrary input (rate, channels) to 48 kHz interleaved stereo.
struct Converter {
    channels: usize,
    step: f64,
    pos: f64,
    last: [f32; 2],
}

impl Converter {
    fn new(rate: u32, channels: usize) -> Self {
        Self {
            channels,
            step: rate as f64 / SAMPLE_RATE as f64,
            pos: 0.0,
            last: [0.0; 2],
        }
    }

    fn convert(&mut self, input: &[f32]) -> Vec<f32> {
        let frames: Vec<[f32; 2]> = input
            .chunks(self.channels)
            .map(|f| match f {
                [m] => [*m, *m],
                [l, r, ..] => [*l, *r],
                [] => [0.0, 0.0],
            })
            .collect();
        if (self.step - 1.0).abs() < 1e-9 {
            return frames.into_iter().flatten().collect();
        }
        // Linear interpolation between the previous and current frames.
        let mut out = Vec::with_capacity((frames.len() as f64 / self.step) as usize * 2 + 4);
        while self.pos < frames.len() as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let a = if i == 0 { self.last } else { frames[i - 1] };
            let b = frames[i];
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.step;
        }
        self.pos -= frames.len() as f64;
        if let Some(l) = frames.last() {
            self.last = *l;
        }
        out
    }
}

fn open_loopback(tx: crossbeam_channel::Sender<Vec<f32>>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    // On WASAPI an input stream on an output device records what it plays.
    let device = host
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config()?;
    let config = supported.config();
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        format = ?supported.sample_format(),
        "capturing system audio"
    );
    let mut conv = Converter::new(config.sample_rate, config.channels as usize);
    let err = |e| tracing::warn!("audio capture error: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data, _| {
                let _ = tx.try_send(conv.convert(data));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data, _| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                let _ = tx.try_send(conv.convert(&f));
            },
            err,
            None,
        )?,
        other => bail!("unsupported capture sample format {other:?}"),
    };
    stream.play()?;
    Ok(stream)
}

fn tone(tx: crossbeam_channel::Sender<Vec<f32>>, stop: Arc<AtomicBool>) {
    let start = Instant::now();
    let mut produced = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let due = (start.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
        let mut buf = Vec::with_capacity((due.saturating_sub(produced)) * 2);
        while produced < due {
            let t = produced as f64 / SAMPLE_RATE as f64;
            let on = t.fract() < 0.15;
            let v = if on {
                (t * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.2
            } else {
                0.0
            };
            buf.extend_from_slice(&[v, v]);
            produced += 1;
        }
        if !buf.is_empty() {
            let _ = tx.try_send(buf);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Client-side playback. Feed packets with [`Player::push`].
pub struct Player {
    tx: crossbeam_channel::Sender<(u32, Vec<u8>)>,
    pub stats: Arc<Mutex<PlayerStats>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PlayerStats {
    pub buffered_ms: f32,
    pub lost: u64,
    pub underruns: u64,
}

/// Target buffer depth. Lower is less latency, higher survives jitter.
const TARGET_MS: usize = 30;
const MAX_MS: usize = 80;

impl Player {
    pub fn start() -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<(u32, Vec<u8>)>();
        let stats = Arc::new(Mutex::new(PlayerStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread_stats = stats.clone();
        std::thread::Builder::new()
            .name("oriel-audio-play".into())
            .spawn(move || {
                let buffer = Arc::new(Mutex::new(VecDeque::<f32>::new()));
                let stream = match open_output(buffer.clone(), thread_stats.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.into()));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let mut pcm = vec![0f32; FRAME * CHANNELS * 6];
                let mut expected: Option<u32> = None;
                while let Ok((seq, data)) = rx.recv() {
                    let mut decoded = Vec::new();
                    if let Some(exp) = expected {
                        let gap = seq.wrapping_sub(exp);
                        if gap > 0x8000_0000 {
                            continue; // late duplicate / reordered
                        }
                        // Conceal up to 5 lost packets.
                        for _ in 0..gap.min(5) {
                            if let Ok(n) = decoder.decode_float(&[], &mut pcm, false) {
                                decoded.extend_from_slice(&pcm[..n * CHANNELS]);
                            }
                        }
                        thread_stats.lock().lost += gap as u64;
                    }
                    expected = Some(seq.wrapping_add(1));
                    match decoder.decode_float(&data, &mut pcm, false) {
                        Ok(n) => decoded.extend_from_slice(&pcm[..n * CHANNELS]),
                        Err(e) => tracing::debug!("opus decode failed: {e}"),
                    }
                    let mut buf = buffer.lock();
                    buf.extend(decoded);
                    let max = MAX_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
                    if buf.len() > max {
                        // Too far behind: skip ahead to the target depth.
                        let target = TARGET_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
                        let drop = buf.len() - target;
                        buf.drain(..drop);
                    }
                    thread_stats.lock().buffered_ms =
                        buf.len() as f32 / CHANNELS as f32 / SAMPLE_RATE as f32 * 1000.0;
                }
                drop(stream);
            })?;
        ready_rx.recv().context("audio playback thread exited")??;
        Ok(Self { tx, stats })
    }

    pub fn push(&self, seq: u32, packet: Vec<u8>) {
        let _ = self.tx.send((seq, packet));
    }
}

fn open_output(
    buffer: Arc<Mutex<VecDeque<f32>>>,
    stats: Arc<Mutex<PlayerStats>>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config()?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    tracing::info!(rate, channels, format = ?supported.sample_format(), "audio output");
    // Resample 48 kHz stereo to the device rate by stepping through the
    // buffer.
    let step = SAMPLE_RATE as f64 / rate as f64;
    let mut pos = 0.0f64;
    let mut primed = false;
    let mut fill = move |out: &mut dyn FnMut(usize, f32), frames: usize| {
        let mut buf = buffer.lock();
        let target = TARGET_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
        // Wait for the buffer to reach its target depth before playing.
        if !primed && buf.len() < target {
            for i in 0..frames * channels {
                out(i, 0.0);
            }
            return;
        }
        primed = true;
        let mut underrun = false;
        for f in 0..frames {
            let i = pos as usize;
            if (i + 1) * CHANNELS > buf.len() {
                primed = false;
                underrun = true;
                for c in 0..channels {
                    out(f * channels + c, 0.0);
                }
                continue;
            }
            let l = buf[i * CHANNELS];
            let r = buf[i * CHANNELS + 1];
            for c in 0..channels {
                out(f * channels + c, if c % 2 == 0 { l } else { r });
            }
            pos += step;
        }
        if underrun {
            stats.lock().underruns += 1;
        }
        let used = (pos as usize).min(buf.len() / CHANNELS);
        buf.drain(..used * CHANNELS);
        pos -= used as f64;
    };
    let err = |e| tracing::warn!("audio playback error: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| {
                let frames = data.len() / channels;
                fill(&mut |i, v| data[i] = v, frames);
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |data, _| {
                let frames = data.len() / channels;
                fill(
                    &mut |i, v| data[i] = (v.clamp(-1.0, 1.0) * 32767.0) as i16,
                    frames,
                );
            },
            err,
            None,
        )?,
        other => bail!("unsupported output sample format {other:?}"),
    };
    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_resamples() {
        let mut c = Converter::new(44_100, 1);
        let out = c.convert(&vec![0.5; 4410]);
        // 100 ms of mono at 44.1 kHz → ~4800 stereo frames at 48 kHz.
        assert!((out.len() as i64 / 2 - 4800).abs() <= 2, "{}", out.len());
        // The first frames interpolate from the previous chunk (silence).
        assert!(out.iter().skip(4).all(|v| (*v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn opus_roundtrip() {
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let input = vec![0.1f32; FRAME * CHANNELS];
        let mut packet = vec![0u8; 1500];
        let n = enc.encode_float(&input, &mut packet).unwrap();
        let mut pcm = vec![0f32; FRAME * CHANNELS];
        assert_eq!(
            dec.decode_float(&packet[..n], &mut pcm, false).unwrap(),
            FRAME
        );
    }
}
