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

#[cfg(target_os = "linux")]
mod linux;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet (10 ms).
pub const FRAME: usize = 480;
/// Silent packets sent before going quiet, so the decoder ends cleanly.
const SILENT_FRAMES_SENT: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// What an output device plays (WASAPI loopback on Windows, a sink
    /// monitor on PipeWire): the one with this [`OutputDevice::id`], or the
    /// default one.
    SystemOutput(Option<String>),
    /// A microphone: the input device with this id, or the default one.
    Microphone(Option<String>),
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
                CaptureSource::SystemOutput(device) => match open_system_output(tx, device) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                },
                CaptureSource::Microphone(id) => match open_microphone(tx, id.as_deref()) {
                    Ok(s) => Some(OutputCapture::Cpal(s)),
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
            // Consecutive silent frames: after a few, nothing is sent until
            // sound returns (the client just runs out of buffered audio).
            let mut silent = 0u32;
            while !thread_stop.load(Ordering::Relaxed) {
                let Ok(samples) = rx.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                pending.extend_from_slice(&samples);
                while pending.len() >= FRAME * CHANNELS {
                    let frame = &pending[..FRAME * CHANNELS];
                    silent = if frame.iter().all(|s| s.abs() < 1e-5) {
                        silent + 1
                    } else {
                        0
                    };
                    if silent > SILENT_FRAMES_SENT {
                        pending.drain(..FRAME * CHANNELS);
                        continue;
                    }
                    match encoder.encode_float(frame, &mut out) {
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

/// Keeps a system output capture alive.
#[allow(dead_code)]
enum OutputCapture {
    Cpal(cpal::Stream),
    #[cfg(target_os = "linux")]
    PipeWire(linux::SinkCapture),
}

fn open_system_output(
    tx: crossbeam_channel::Sender<Vec<f32>>,
    device: Option<String>,
) -> Result<OutputCapture> {
    #[cfg(target_os = "linux")]
    return Ok(OutputCapture::PipeWire(linux::capture_output(
        tx,
        device
            .as_deref()
            .map(|id| id.strip_prefix("pipewire:").unwrap_or(id)),
    )?));
    #[cfg(not(target_os = "linux"))]
    Ok(OutputCapture::Cpal(open_loopback(tx, device.as_deref())?))
}

/// An audio output device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    /// Stable identifier (a cpal device id such as `wasapi:...` or
    /// `pipewire:<node name>`).
    pub id: String,
    pub name: String,
}

/// The audio API whose devices are listed and chosen: PipeWire where it
/// runs (its sinks are what users know), else the platform default.
fn device_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire) {
        return host;
    }
    cpal::default_host()
}

/// The output devices that can be captured or played to.
pub fn output_devices() -> Vec<OutputDevice> {
    let host = device_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let mut list: Vec<OutputDevice> = devices
        .filter_map(|d| {
            let id = d.id().ok()?;
            let description = d.description().ok()?;
            // PipeWire also lists the default-device alias and applications'
            // playback streams; only real sinks can be both played to and
            // captured.
            if id.id().ends_with("_default")
                || (id.host() != cpal::default_host().id() && !description.supports_input())
            {
                return None;
            }
            Some(OutputDevice {
                id: id.to_string(),
                name: description.name().to_string(),
            })
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list.dedup_by(|a, b| a.id == b.id);
    list
}

/// Where the client's microphone plays on this host so that apps can
/// record it: oriel's own virtual source on Linux; on Windows a virtual
/// cable's playback end (Steam Streaming Microphone or VB-Audio CABLE).
pub fn microphone_output() -> Result<Output> {
    #[cfg(target_os = "linux")]
    return Ok(Output::VirtualMicrophone);
    #[cfg(not(target_os = "linux"))]
    {
        let devices = output_devices();
        for wanted in ["Steam Streaming Microphone", "CABLE Input"] {
            if let Some(d) = devices.iter().find(|d| d.name.contains(wanted)) {
                tracing::info!(device = %d.name, "client microphone plays here");
                return Ok(Output::Device(Some(d.id.clone())));
            }
        }
        bail!(
            "no virtual microphone on the host; install VB-Audio Virtual Cable \
             (https://vb-audio.com/Cable/) and record from \"CABLE Output\""
        )
    }
}

/// Microphones and other inputs.
pub fn input_devices() -> Vec<OutputDevice> {
    // PipeWire offers sinks' monitors as inputs too; those are not mics.
    let outputs: Vec<String> = output_devices().into_iter().map(|d| d.id).collect();
    let host = device_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut list: Vec<OutputDevice> = devices
        .filter_map(|d| {
            let id = d.id().ok()?;
            let description = d.description().ok()?;
            if id.id().ends_with("_default") || outputs.contains(&id.to_string()) {
                return None;
            }
            Some(OutputDevice {
                id: id.to_string(),
                name: description.name().to_string(),
            })
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list.dedup_by(|a, b| a.id == b.id);
    list
}

fn open_microphone(
    tx: crossbeam_channel::Sender<Vec<f32>>,
    id: Option<&str>,
) -> Result<cpal::Stream> {
    let found = id.and_then(|id| {
        let id = id.parse::<cpal::DeviceId>().ok()?;
        cpal::host_from_id(id.host()).ok()?.device_by_id(&id)
    });
    let device = match found {
        Some(d) => d,
        None => cpal::default_host()
            .default_input_device()
            .context("no microphone")?,
    };
    let supported = device.default_input_config()?;
    let config = supported.config();
    tracing::info!(
        device = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_default(),
        rate = config.sample_rate,
        channels = config.channels,
        "capturing the microphone"
    );
    let mut conv = Converter::new(config.sample_rate, config.channels as usize);
    let err = |e| tracing::warn!("microphone error: {e}");
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
        other => bail!("unsupported microphone sample format {other:?}"),
    };
    stream.play()?;
    Ok(stream)
}

/// The output device with this id, or the default one.
fn output_device(id: Option<&str>) -> Result<cpal::Device> {
    if let Some(id) = id {
        let found = id
            .parse::<cpal::DeviceId>()
            .ok()
            .and_then(|id| cpal::host_from_id(id.host()).ok()?.device_by_id(&id));
        match found {
            Some(d) => return Ok(d),
            None => tracing::warn!(id, "audio device not found, using the default"),
        }
    }
    cpal::default_host()
        .default_output_device()
        .context("no audio output device")
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn open_loopback(
    tx: crossbeam_channel::Sender<Vec<f32>>,
    id: Option<&str>,
) -> Result<cpal::Stream> {
    // On WASAPI an input stream on an output device records what it plays.
    let device = output_device(id)?;
    let supported = device.default_output_config()?;
    let config = supported.config();
    tracing::info!(
        device = device.description().map(|d| d.name().to_string()).unwrap_or_default(),
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

/// Where a [`Player`] plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The output device with this [`OutputDevice::id`], or the default.
    Device(Option<String>),
    /// A virtual microphone ("oriel microphone") other apps record from.
    #[cfg(target_os = "linux")]
    VirtualMicrophone,
}

/// Jitter-buffered Opus playback. Feed packets with [`Player::push`].
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
    pub fn start(output: Output) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<(u32, Vec<u8>)>();
        let stats = Arc::new(Mutex::new(PlayerStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread_stats = stats.clone();
        std::thread::Builder::new()
            .name("oriel-audio-play".into())
            .spawn(move || {
                let buffer = Arc::new(Mutex::new(VecDeque::<f32>::new()));
                let stream = match open_output(&output, buffer.clone(), thread_stats.clone()) {
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

/// Pulls 48 kHz stereo from the jitter buffer for an output callback,
/// resampling to the device rate by stepping through the buffer.
pub(crate) struct Reader {
    buffer: Arc<Mutex<VecDeque<f32>>>,
    stats: Arc<Mutex<PlayerStats>>,
    step: f64,
    pos: f64,
    primed: bool,
    channels: usize,
}

impl Reader {
    /// Write `frames` frames of `channels` samples through `out(index, v)`.
    pub(crate) fn fill(&mut self, out: &mut dyn FnMut(usize, f32), frames: usize) {
        let channels = self.channels;
        let mut buf = self.buffer.lock();
        let target = TARGET_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
        // Wait for the buffer to reach its target depth before playing.
        if !self.primed && buf.len() < target {
            for i in 0..frames * channels {
                out(i, 0.0);
            }
            return;
        }
        self.primed = true;
        let mut underrun = false;
        for f in 0..frames {
            let i = self.pos as usize;
            if (i + 1) * CHANNELS > buf.len() {
                self.primed = false;
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
            self.pos += self.step;
        }
        if underrun {
            self.stats.lock().underruns += 1;
        }
        let used = (self.pos as usize).min(buf.len() / CHANNELS);
        buf.drain(..used * CHANNELS);
        self.pos -= used as f64;
    }
}

/// Keeps an output alive (it stays on the playback thread).
type OutputGuard = Box<dyn std::any::Any>;

fn open_output(
    output: &Output,
    buffer: Arc<Mutex<VecDeque<f32>>>,
    stats: Arc<Mutex<PlayerStats>>,
) -> Result<OutputGuard> {
    let id = match output {
        Output::Device(id) => id.as_deref(),
        #[cfg(target_os = "linux")]
        Output::VirtualMicrophone => {
            let reader = Reader {
                buffer,
                stats,
                step: 1.0,
                pos: 0.0,
                primed: false,
                channels: CHANNELS,
            };
            return Ok(Box::new(linux::virtual_microphone(reader)?));
        }
    };
    let device = output_device(id)?;
    let supported = device.default_output_config()?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    tracing::info!(rate, channels, format = ?supported.sample_format(), "audio output");
    let mut reader = Reader {
        buffer,
        stats,
        step: SAMPLE_RATE as f64 / rate as f64,
        pos: 0.0,
        primed: false,
        channels,
    };
    let err = |e| tracing::warn!("audio playback error: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| {
                let frames = data.len() / channels;
                reader.fill(&mut |i, v| data[i] = v, frames);
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |data, _| {
                let frames = data.len() / channels;
                reader.fill(
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
    Ok(Box::new(stream))
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

#[cfg(test)]
mod device_tests {
    /// Lists this machine's devices; run with --nocapture to see them.
    #[test]
    fn list_output_devices() {
        let devices = super::output_devices();
        for d in &devices {
            eprintln!("{} — {}", d.name, d.id);
        }
        let mut ids: Vec<_> = devices.iter().map(|d| &d.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), devices.len());
    }
}

/// Needs a PipeWire with the sinks `oriel-sink-a` and `oriel-sink-b`;
/// runs only with ORIEL_AUDIO_DEVICE_TEST set.
#[cfg(all(test, target_os = "linux"))]
mod routing_tests {
    use super::*;

    #[test]
    fn plays_and_captures_chosen_devices() {
        if std::env::var_os("ORIEL_AUDIO_DEVICE_TEST").is_none() {
            eprintln!("skipping: ORIEL_AUDIO_DEVICE_TEST not set");
            return;
        }
        let bytes = |sink: &str| {
            let total = Arc::new(Mutex::new(0usize));
            let t = total.clone();
            let capture = start_capture(
                CaptureSource::SystemOutput(Some(format!("pipewire:{sink}"))),
                128_000,
                move |p| *t.lock() += p.len(),
            )
            .unwrap();
            (capture, total)
        };
        let (_a, a) = bytes("oriel-sink-a");
        let (_b, b) = bytes("oriel-sink-b");
        let player = Player::start(Output::Device(Some("pipewire:oriel-sink-b".into()))).unwrap();
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let mut out = vec![0u8; 1500];
        let start = Instant::now();
        for seq in 0..150u32 {
            let pcm: Vec<f32> = (0..FRAME)
                .flat_map(|i| {
                    let t = (seq as usize * FRAME + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                    [v, v]
                })
                .collect();
            let n = enc.encode_float(&pcm, &mut out).unwrap();
            player.push(seq, out[..n].to_vec());
            let due = start + Duration::from_millis(10 * (seq as u64 + 1));
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        let (a, b) = (*a.lock(), *b.lock());
        eprintln!("captured bytes: sink a {a}, sink b {b}");
        assert!(b > 3 * a.max(1), "the tone should only reach sink b");
    }

    /// A tone played into the virtual microphone can be recorded from it.
    #[test]
    fn virtual_microphone_roundtrip() {
        if std::env::var_os("ORIEL_AUDIO_DEVICE_TEST").is_none() {
            eprintln!("skipping: ORIEL_AUDIO_DEVICE_TEST not set");
            return;
        }
        let player = Player::start(Output::VirtualMicrophone).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let devices = input_devices();
        eprintln!("inputs: {devices:?}");
        let mic = devices
            .iter()
            .find(|d| d.name == "oriel microphone")
            .expect("virtual microphone listed");
        let total = Arc::new(Mutex::new(0usize));
        let t = total.clone();
        let _capture = start_capture(
            CaptureSource::Microphone(Some(mic.id.clone())),
            64_000,
            move |p| *t.lock() += p.len(),
        )
        .unwrap();
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let mut out = vec![0u8; 1500];
        let start = Instant::now();
        for seq in 0..150u32 {
            let pcm: Vec<f32> = (0..FRAME)
                .flat_map(|i| {
                    let t = (seq as usize * FRAME + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                    [v, v]
                })
                .collect();
            let n = enc.encode_float(&pcm, &mut out).unwrap();
            player.push(seq, out[..n].to_vec());
            let due = start + Duration::from_millis(10 * (seq as u64 + 1));
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        let bytes = *total.lock();
        eprintln!("recorded {bytes} bytes from the virtual microphone");
        assert!(bytes > 5000, "the tone should be recorded");
    }
}

#[cfg(test)]
mod silence_tests {
    use super::*;

    /// The test tone beeps 150 ms each second; the silence between beeps
    /// should cost only the few packets that end each beep.
    #[test]
    fn silence_is_not_sent() {
        let count = Arc::new(Mutex::new(0u32));
        let c = count.clone();
        let capture =
            start_capture(CaptureSource::TestTone, 128_000, move |_| *c.lock() += 1).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        drop(capture);
        let n = *count.lock();
        eprintln!("{n} packets in 2 s");
        assert!((20..=70).contains(&n), "{n} packets");
    }
}
