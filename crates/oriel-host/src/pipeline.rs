//! The capture → encode loop. Runs on its own thread; talks to the async
//! session through channels.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use crossbeam_channel::Receiver;
use oriel_media::{Encoder, EncoderConfig, EncoderInput};
use oriel_proto::{Codec, CursorShape, CursorState, FrameHeader, SurfaceId, SurfaceInfo};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::capture::{self, Capturer, Geometry, SourceKind, TestState};

#[derive(Debug, Clone)]
pub struct Config {
    pub source: SourceKind,
    pub target: capture::Target,
    pub surface: SurfaceId,
    pub codecs: Vec<Codec>,
    pub bitrate_kbps: u32,
    pub max_fps: u32,
    pub encoder: Option<String>,
    pub auto_codec: bool,
    /// Codecs to encode at 4:4:4 where the encoder can.
    pub full_chroma: Vec<Codec>,
    /// Encode in software where this host can.
    pub software_encode: bool,
    /// Capture with Windows Graphics Capture (Windows hosts).
    pub graphics_capture: bool,
    pub pyrowave_kbps: u32,
    /// Adapt the bitrate to the network, up to `bitrate_kbps`.
    pub auto_bitrate: bool,
}

pub enum Cmd {
    Keyframe,
    /// Capture this target instead (display switch, virtual monitor).
    Target(capture::Target),
    /// Codecs, automatic switching, those wanted at 4:4:4, and software
    /// encoding.
    Codecs(Vec<Codec>, bool, Vec<Codec>, bool),
    GraphicsCapture(bool),
    Bitrate(u32),
    AutoBitrate(bool),
    MaxFps(u32),
    PyroWaveBitrate(u32),
    Stop,
}

pub enum Event {
    Surface(SurfaceInfo),
    Frame(FrameHeader, Vec<u8>),
    CursorShape(CursorShape),
    CursorState(CursorState),
    CursorPos {
        x: i32,
        y: i32,
    },
    Notice(String),
    /// A display was captured, but nothing showed up (no monitor attached
    /// or switched to another input): the session may use a virtual one.
    NoPicture,
    /// The last second's frames (loss is the session's to add).
    Health(oriel_proto::HostHealth),
    Clipboard(oriel_clipboard::Content),
    /// A game set a virtual gamepad's motors.
    Rumble {
        pad: u8,
        strong: u16,
        weak: u16,
    },
    /// The pipeline gave up; the session should end.
    Failed(String),
}

/// State shared with the session.
pub struct Shared {
    /// Sequence number of the next frame to send.
    pub next_seq: AtomicU64,
    /// Every frame below this has been decoded by the client (`FrameAck`).
    decoded_below: AtomicU64,
    /// Every frame below this has reached the client's QUIC stack.
    delivered_below: AtomicU64,
    /// Every frame below this was handed to QUIC in full.
    written_below: AtomicU64,
    /// Microseconds since `epoch` of the last `FrameAck` progress.
    last_decoded_us: AtomicU64,
    epoch: Instant,
    pub rtt_us: AtomicU64,
    /// Packets sent and lost on the connection so far.
    pub sent_packets: AtomicU64,
    pub lost_packets: AtomicU64,
    /// Since when (µs since `epoch` + 1, 0 = not) frames wait for earlier
    /// ones to be decoded.
    blocked_since_us: AtomicU64,
    pub geometry: Mutex<Geometry>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            next_seq: AtomicU64::new(0),
            decoded_below: AtomicU64::new(0),
            delivered_below: AtomicU64::new(0),
            written_below: AtomicU64::new(0),
            last_decoded_us: AtomicU64::new(0),
            epoch: Instant::now(),
            rtt_us: AtomicU64::new(0),
            sent_packets: AtomicU64::new(0),
            lost_packets: AtomicU64::new(0),
            blocked_since_us: AtomicU64::new(0),
            geometry: Mutex::new(Geometry::default()),
        }
    }
}

impl Shared {
    /// The client decoded every frame up to `seq`.
    pub fn decoded(&self, seq: u64) {
        if self.decoded_below.fetch_max(seq + 1, Ordering::Relaxed) <= seq {
            self.last_decoded_us
                .store(self.epoch.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }

    fn set_blocked(&self, blocked: bool) {
        if !blocked {
            self.blocked_since_us.store(0, Ordering::Relaxed);
        } else if self.blocked_since_us.load(Ordering::Relaxed) == 0 {
            self.blocked_since_us.store(
                self.epoch.elapsed().as_micros() as u64 + 1,
                Ordering::Relaxed,
            );
        }
    }

    /// How long sending has waited for the client, and the frame counters
    /// behind it (for the stall log).
    pub fn blocked(&self) -> Option<(Duration, String)> {
        let since = self.blocked_since_us.load(Ordering::Relaxed);
        if since == 0 {
            return None;
        }
        let now = self.epoch.elapsed().as_micros() as u64;
        let last_ack = self.last_decoded_us.load(Ordering::Relaxed);
        Some((
            Duration::from_micros(now.saturating_sub(since)),
            format!(
                "next frame {}, written below {}, decoded below {}, delivered below {}, \
                 last decode ack {:.1} s ago",
                self.next_seq.load(Ordering::Relaxed),
                self.written_below.load(Ordering::Relaxed),
                self.decoded_below.load(Ordering::Relaxed),
                self.delivered_below.load(Ordering::Relaxed),
                now.saturating_sub(last_ack) as f64 / 1e6,
            ),
        ))
    }

    /// Frame `seq` is with QUIC in full.
    pub fn written(&self, seq: u64) {
        self.written_below.fetch_max(seq + 1, Ordering::Relaxed);
    }

    /// QUIC delivered (or gave up on) frame `seq`.
    pub fn delivered(&self, seq: u64) {
        self.delivered_below.fetch_max(seq + 1, Ordering::Relaxed);
    }

    /// Frames sent but not yet decoded. Pacing on decoding (not delivery)
    /// keeps a slow client from piling up a backlog in its receive buffers.
    /// Delivery only counts when decode acknowledgements stall. For an old
    /// client (no acknowledgements at all) it counts in full; otherwise
    /// one more frame per quiet second, up to 16: enough to get past lost
    /// datagrams, without piling up minutes of video for a client that is
    /// held up (a hidden macOS app).
    fn in_flight(&self) -> u64 {
        let next = self.next_seq.load(Ordering::Relaxed);
        let decoded = self.decoded_below.load(Ordering::Relaxed);
        let since_ack = self.epoch.elapsed().as_micros() as u64
            - self
                .last_decoded_us
                .load(Ordering::Relaxed)
                .min(self.epoch.elapsed().as_micros() as u64);
        let acked = if since_ack > 1_000_000 {
            let mut delivered = self.delivered_below.load(Ordering::Relaxed);
            if decoded > 0 {
                delivered = delivered.min(decoded + (since_ack / 1_000_000).min(16));
            }
            decoded.max(delivered)
        } else {
            decoded
        };
        next.saturating_sub(acked)
    }
}

pub fn spawn(
    config: Config,
    shared: Arc<Shared>,
    test: TestState,
    cmds: Receiver<Cmd>,
    events: UnboundedSender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("oriel-pipeline".into())
        .spawn(move || {
            #[cfg(windows)]
            crate::win::boost_thread_priority();
            let mut p = Pipeline {
                rate: crate::ratecontrol::RateControl::new(config.bitrate_kbps, Instant::now()),
                auto: AutoCodec {
                    retry_at: Instant::now(),
                    backoff: Duration::from_secs(30),
                },
                config,
                shared,
                test,
                cmds,
                events,
                graphics_capture_failed: false,
            };
            if let Err(e) = p.run() {
                if p.events.is_closed() {
                    return;
                }
                tracing::error!("pipeline failed: {e:#}");
                let _ = p.events.send(Event::Failed(format!("{e:#}")));
            }
        })
        .expect("spawn pipeline thread")
}

struct Pipeline {
    config: Config,
    shared: Arc<Shared>,
    test: TestState,
    cmds: Receiver<Cmd>,
    events: UnboundedSender<Event>,
    auto: AutoCodec,
    /// Automatic bitrate; kept across encoder and capturer reopens.
    rate: crate::ratecontrol::RateControl,
    /// Windows Graphics Capture failed to start; not tried again until it
    /// is asked for anew.
    graphics_capture_failed: bool,
}

enum Flow {
    /// Reopen the capturer (and encoder).
    Continue,
    /// Keep the capturer, reopen the encoder (codec switch).
    Reencode,
    Stop,
}

/// RTT below which PyroWave is worth trying in automatic codec mode.
const PYROWAVE_MAX_RTT: Duration = Duration::from_millis(3);
/// Refresh frames sent once the screen is still (see `stream`), and how
/// long it must be still first.
const REFRESH_FRAMES: u32 = 30;
const REFRESH_AFTER: Duration = Duration::from_millis(150);
/// Capture starts with the current image; none this long after opening
/// means no monitor shows the display.
const NO_PICTURE_AFTER: Duration = Duration::from_secs(4);

/// A quarter second of streaming, for automatic bitrate.
struct Quarter {
    start: Instant,
    captured: u32,
    encoded: u32,
    blocked: bool,
    bytes: u64,
    sent_packets: u64,
    lost_packets: u64,
}

impl Quarter {
    fn new(shared: &Shared) -> Self {
        Self {
            start: Instant::now(),
            captured: 0,
            encoded: 0,
            blocked: false,
            bytes: 0,
            sent_packets: shared.sent_packets.load(Ordering::Relaxed),
            lost_packets: shared.lost_packets.load(Ordering::Relaxed),
        }
    }

    fn sample(&self, shared: &Shared, max_fps: u32, rtt: Duration) -> crate::ratecontrol::Sample {
        let secs = self.start.elapsed().as_secs_f32().max(0.001);
        let wanted = self.captured.min((max_fps as f32 * secs) as u32);
        let sent = shared.sent_packets.load(Ordering::Relaxed) - self.sent_packets;
        let lost = shared.lost_packets.load(Ordering::Relaxed) - self.lost_packets;
        crate::ratecontrol::Sample {
            rtt,
            // Waiting for acknowledgements is normal; it is congestion only
            // when it cost frames.
            blocked: self.blocked && wanted >= 3 && (self.encoded as f32) < wanted as f32 * 0.8,
            loss: if sent >= 20 {
                lost as f32 / sent as f32
            } else {
                0.0
            },
            sent_kbps: self.bytes as f32 * 8.0 / 1000.0 / secs,
        }
    }
}

/// Automatic codec switching state.
struct AutoCodec {
    /// Do not try PyroWave again before this.
    retry_at: Instant,
    backoff: Duration,
}

impl Pipeline {
    fn send(&self, event: Event) -> Result<()> {
        if self.events.send(event).is_err() {
            bail!("session ended");
        }
        Ok(())
    }

    fn open_capturer(&mut self) -> Result<Option<Box<dyn Capturer>>> {
        let mut failures = 0u32;
        let started = Instant::now();
        let mut reported = false;
        loop {
            let method = if self.config.graphics_capture && !self.graphics_capture_failed {
                capture::Method::GraphicsCapture
            } else {
                capture::Method::Duplication
            };
            let opened = capture::open(self.config.source, self.config.target, method, &self.test);
            if let (Err(e), capture::Method::GraphicsCapture) = (&opened, method) {
                tracing::warn!("Windows Graphics Capture unavailable: {e:#}");
                self.graphics_capture_failed = true;
                self.send(Event::Notice(format!(
                    "Windows Graphics Capture does not work here ({e:#}); using Desktop \
                     Duplication"
                )))?;
                continue;
            }
            match opened {
                Ok(c) => {
                    if reported {
                        self.send(Event::Notice("Capture resumed".into()))?;
                    }
                    return Ok(Some(c));
                }
                Err(e) => {
                    failures += 1;
                    if failures == 1 {
                        tracing::warn!("cannot capture: {e:#}");
                    }
                    // Mode changes and desktop switches refuse duplication
                    // for a moment; only report lasting failures.
                    if !reported && started.elapsed() >= Duration::from_millis(1500) {
                        reported = true;
                        self.send(Event::Notice(format!("Capture unavailable: {e:#}")))?;
                        if matches!(self.config.target, capture::Target::Display(_)) {
                            self.send(Event::NoPicture)?;
                        }
                    }
                    let delay = Duration::from_millis((100 * failures as u64).min(1000));
                    match self.cmds.recv_timeout(delay) {
                        Ok(Cmd::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            return Ok(None);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn rtt(&self) -> Duration {
        Duration::from_micros(self.shared.rtt_us.load(Ordering::Relaxed))
    }

    /// Whether automatic mode should use PyroWave right now.
    fn pyrowave_allowed(&self) -> bool {
        !self.config.auto_codec
            || (Instant::now() >= self.auto.retry_at && self.rtt() < PYROWAVE_MAX_RTT)
    }

    fn open_encoder(&self, cap: &dyn Capturer, bitrate_kbps: u32) -> Result<(Encoder, Codec)> {
        if self.config.software_encode {
            match self.open_encoder_with(cap, bitrate_kbps, true) {
                Ok(opened) => return Ok(opened),
                Err(e) => {
                    tracing::info!("no software encoder: {e:#}");
                    let _ = self.send(Event::Notice(
                        "This host has no software encoder for these codecs; it encodes \
                         in hardware"
                            .into(),
                    ));
                }
            }
        }
        self.open_encoder_with(cap, bitrate_kbps, false)
    }

    fn open_encoder_with(
        &self,
        cap: &dyn Capturer,
        bitrate_kbps: u32,
        software: bool,
    ) -> Result<(Encoder, Codec)> {
        let (width, height) = cap.size();
        let mut errors = Vec::new();
        for &codec in &self.config.codecs {
            // PyroWave runs on the GPU only.
            if codec == Codec::PyroWave && (software || !self.pyrowave_allowed()) {
                continue;
            }
            let config = EncoderConfig {
                codec,
                width,
                height,
                fps: self.config.max_fps,
                bitrate_kbps: if codec == Codec::PyroWave {
                    self.config.pyrowave_kbps
                } else {
                    bitrate_kbps
                },
                device: cap.hw_device(),
                encoder: self.config.encoder.clone(),
                full_chroma: self.config.full_chroma.contains(&codec),
                software,
            };
            match Encoder::open(&config) {
                Ok(enc) => return Ok((enc, codec)),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        bail!("no usable encoder: {}", errors.join("; "))
    }

    /// The bitrate to encode at: the controller's when automatic.
    fn video_kbps(&self, chosen: u32) -> u32 {
        if self.config.auto_bitrate {
            self.rate.target_kbps().min(chosen)
        } else {
            chosen
        }
    }

    fn run(&mut self) -> Result<()> {
        let mut bitrate = self.config.bitrate_kbps;
        // Outlives a capturer that is reopened for the same target (after a
        // desktop switch, say), so a virtual monitor is not recreated.
        let mut keep: Option<(
            capture::Target,
            Option<Arc<dyn std::any::Any + Send + Sync>>,
        )> = None;
        loop {
            if keep.as_ref().is_some_and(|(t, _)| *t != self.config.target) {
                // Release the old target first (a virtual monitor in
                // another mode must go before the new one is added).
                drop(keep.take());
            }
            let Some(mut cap) = self.open_capturer()? else {
                return Ok(());
            };
            keep = Some((self.config.target, cap.keep_alive()));
            *self.shared.geometry.lock() = cap.geometry();
            loop {
                let (mut enc, codec) = self.open_encoder(cap.as_ref(), self.video_kbps(bitrate))?;
                let (width, height) = cap.size();
                self.send(Event::Surface(SurfaceInfo {
                    id: self.config.surface,
                    width,
                    height,
                    codec,
                    full_chroma: enc.full_chroma(),
                    encoder: enc.name().to_string(),
                }))?;
                match self.stream(cap.as_mut(), &mut enc, codec, &mut bitrate)? {
                    Flow::Stop => return Ok(()),
                    Flow::Continue => break,
                    Flow::Reencode => {}
                }
            }
        }
    }

    /// Stream from one capturer until it fails (→ `Continue`, reopen) or the
    /// session stops.
    fn stream(
        &mut self,
        cap: &mut dyn Capturer,
        enc: &mut Encoder,
        codec: Codec,
        bitrate: &mut u32,
    ) -> Result<Flow> {
        let interval = Duration::from_secs(1) / self.config.max_fps.max(1);
        let (width, height) = cap.size();
        // When the next frame is due. Frames may go up to a quarter interval
        // early (capture jitter) without raising the average rate.
        let mut next_due = Instant::now();
        let mut capture_time = Instant::now();
        let mut has_frame = false;
        let mut dirty = false;
        // Once the screen stops changing, the image is encoded a few more
        // times: each P-frame refines the last, lossy one (+5 dB on text
        // after scrolling). PyroWave frames are complete already.
        let mut last_change = Instant::now();
        let mut refreshes_left = 0u32;
        let mut force_key = true;
        let mut cursor_state: Option<CursorState> = None;
        let mut shape_id = 0u64;
        let mut encode_errors = 0u32;
        let mut blocked = false;
        // Automatic codec mode: how long frames waited on the network.
        let auto = self.config.auto_codec && self.config.codecs.contains(&Codec::PyroWave);
        let mut window_start = Instant::now();
        let mut window_captured = 0u32;
        let mut window_encoded = 0u32;
        let mut window_blocked = false;
        let mut slow_windows = 0;
        // Automatic bitrate looks at quarter seconds.
        let mut quarter = Quarter::new(&self.shared);
        // What the encoder runs at, and when it was last reopened for that.
        let mut enc_kbps = self.video_kbps(*bitrate);
        let mut reopened = Instant::now();
        // A display that shows nothing for this long has no monitor on it.
        let opened = Instant::now();
        let mut no_picture_reported = false;

        loop {
            while let Ok(cmd) = self.cmds.try_recv() {
                match cmd {
                    Cmd::Keyframe => force_key = true,
                    Cmd::Bitrate(kbps) if kbps != *bitrate => {
                        *bitrate = kbps;
                        self.rate.set_max(kbps);
                        if codec != Codec::PyroWave {
                            let kbps = self.video_kbps(kbps);
                            if !enc.set_bitrate(kbps) {
                                let (new, _) = self.open_encoder(cap, kbps)?;
                                *enc = new;
                                force_key = true;
                            }
                        }
                    }
                    Cmd::Bitrate(_) => {}
                    Cmd::AutoBitrate(on) if on != self.config.auto_bitrate => {
                        self.config.auto_bitrate = on;
                        self.rate = crate::ratecontrol::RateControl::new(*bitrate, Instant::now());
                        if codec != Codec::PyroWave && !on {
                            let kbps = *bitrate;
                            if !enc.set_bitrate(kbps) {
                                let (new, _) = self.open_encoder(cap, kbps)?;
                                *enc = new;
                                force_key = true;
                            }
                        }
                    }
                    Cmd::AutoBitrate(_) => {}
                    Cmd::MaxFps(fps) if fps != self.config.max_fps => {
                        self.config.max_fps = fps.clamp(1, 240);
                        return Ok(Flow::Reencode);
                    }
                    Cmd::PyroWaveBitrate(kbps) if kbps != self.config.pyrowave_kbps => {
                        self.config.pyrowave_kbps = kbps.max(10_000);
                        if codec == Codec::PyroWave {
                            return Ok(Flow::Reencode);
                        }
                    }
                    Cmd::MaxFps(_) | Cmd::PyroWaveBitrate(_) => {}
                    Cmd::Codecs(codecs, auto, full_chroma, software) => {
                        if !codecs.is_empty() {
                            self.config.codecs = codecs;
                            self.config.auto_codec = auto;
                            self.config.full_chroma = full_chroma;
                            self.config.software_encode = software;
                            self.auto.retry_at = Instant::now();
                            return Ok(Flow::Reencode);
                        }
                    }
                    Cmd::GraphicsCapture(on) => {
                        if on != self.config.graphics_capture {
                            self.config.graphics_capture = on;
                            self.graphics_capture_failed = false;
                            return Ok(Flow::Continue);
                        }
                    }
                    Cmd::Target(target) => {
                        if self.config.target != target {
                            self.config.target = target;
                            return Ok(Flow::Continue);
                        }
                    }
                    Cmd::Stop => return Ok(Flow::Stop),
                }
            }
            if self.events.is_closed() {
                return Ok(Flow::Stop);
            }

            let now = Instant::now();
            let next_slot = next_due.checked_sub(interval / 4).unwrap_or(next_due);
            let timeout = if blocked {
                Duration::from_millis(1)
            } else if has_frame && (dirty || force_key) {
                next_slot.saturating_duration_since(now)
            } else if has_frame && refreshes_left > 0 {
                next_slot
                    .max(last_change + REFRESH_AFTER)
                    .saturating_duration_since(now)
            } else {
                Duration::from_millis(16)
            };
            match cap.next_frame(timeout) {
                Ok(true) => {
                    has_frame = true;
                    dirty = true;
                    last_change = Instant::now();
                    refreshes_left = if codec == Codec::PyroWave {
                        0
                    } else {
                        REFRESH_FRAMES
                    };
                    capture_time = Instant::now();
                    window_captured += 1;
                    quarter.captured += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::info!("capture interrupted: {e:#}");
                    return Ok(Flow::Continue);
                }
            }

            if !has_frame
                && !no_picture_reported
                && opened.elapsed() >= NO_PICTURE_AFTER
                && matches!(self.config.target, capture::Target::Display(_))
            {
                no_picture_reported = true;
                tracing::info!("the captured display shows nothing");
                self.send(Event::NoPicture)?;
            }

            if let Some(shape) = cap.cursor_shape() {
                shape_id = shape.id;
                self.send(Event::CursorShape(shape))?;
            }
            let pos = cap.cursor();
            let state = CursorState {
                surface: self.config.surface,
                visible: pos.visible,
                captured: pos.captured,
                shape: shape_id,
                x: pos.x,
                y: pos.y,
            };
            match cursor_state {
                Some(prev)
                    if prev.visible == state.visible
                        && prev.captured == state.captured
                        && prev.shape == state.shape =>
                {
                    if (prev.x, prev.y) != (state.x, state.y) {
                        self.send(Event::CursorPos {
                            x: state.x,
                            y: state.y,
                        })?;
                        cursor_state = Some(state);
                    }
                }
                _ => {
                    self.send(Event::CursorState(state))?;
                    cursor_state = Some(state);
                }
            }

            if quarter.start.elapsed() >= Duration::from_millis(250) {
                let sample = quarter.sample(&self.shared, self.config.max_fps, self.rtt());
                quarter = Quarter::new(&self.shared);
                if self.config.auto_bitrate
                    && codec != Codec::PyroWave
                    && let Some(kbps) = self.rate.update(Instant::now(), sample)
                {
                    let kbps = kbps.min(*bitrate);
                    tracing::debug!(kbps, ?sample, "automatic bitrate");
                    if !enc.set_bitrate(kbps) {
                        // Reopening costs a keyframe: only for real cuts, or
                        // to climb back after a while.
                        let current = enc_kbps;
                        let worth = kbps * 5 < current * 4
                            || (kbps > current && reopened.elapsed() >= Duration::from_secs(5));
                        if worth && reopened.elapsed() >= Duration::from_secs(2) {
                            let (new, _) = self.open_encoder(cap, kbps)?;
                            *enc = new;
                            force_key = true;
                            enc_kbps = kbps;
                            reopened = Instant::now();
                        }
                    } else {
                        enc_kbps = kbps;
                    }
                }
            }

            if window_start.elapsed() >= Duration::from_secs(1) {
                // Slow: the screen changed more often than we managed to
                // send, because frames were waiting on the network.
                let wanted = window_captured.min(
                    (self.config.max_fps as f64 * window_start.elapsed().as_secs_f64()) as u32,
                );
                let slow = window_blocked
                    && wanted >= 10
                    && (window_encoded as f64) < wanted as f64 * 0.75;
                let ratio = window_encoded as f64 / wanted.max(1) as f64;
                self.send(Event::Health(oriel_proto::HostHealth {
                    sent_fps: window_encoded.min(u16::MAX as u32) as u16,
                    wanted_fps: wanted.min(u16::MAX as u32) as u16,
                    network_limited: slow,
                    loss_permille: 0,
                    bitrate_kbps: if codec == Codec::PyroWave {
                        self.config.pyrowave_kbps
                    } else {
                        enc_kbps
                    },
                }))?;
                window_start = Instant::now();
                window_captured = 0;
                window_encoded = 0;
                window_blocked = false;
                if auto {
                    if codec == Codec::PyroWave {
                        slow_windows = if slow { slow_windows + 1 } else { 0 };
                        if slow_windows >= 2 {
                            self.auto.retry_at = Instant::now() + self.auto.backoff;
                            tracing::info!(
                                ratio,
                                retry_in = ?self.auto.backoff,
                                "link too slow for PyroWave, switching codec"
                            );
                            self.send(Event::Notice(format!(
                                "Link too slow for PyroWave; trying again in {} s",
                                self.auto.backoff.as_secs()
                            )))?;
                            self.auto.backoff =
                                (self.auto.backoff * 2).min(Duration::from_secs(600));
                            return Ok(Flow::Reencode);
                        }
                    } else if self.pyrowave_allowed() {
                        tracing::info!("trying PyroWave again");
                        return Ok(Flow::Reencode);
                    }
                }
            }

            blocked = false;
            let refresh = !dirty && refreshes_left > 0 && last_change.elapsed() >= REFRESH_AFTER;
            if !(has_frame && (dirty || force_key || refresh)) || Instant::now() < next_slot {
                continue;
            }
            let rtt = self.rtt();
            let limit = (rtt.as_secs_f64() / interval.as_secs_f64()).ceil() as u64 + 2;
            let held = self.shared.in_flight() >= limit.clamp(2, 16);
            self.shared.set_blocked(held);
            if held {
                blocked = true;
                window_blocked = true;
                quarter.blocked = true;
                continue;
            }

            let now = Instant::now();
            next_due = if now > next_due + interval {
                now + interval
            } else {
                next_due + interval
            };
            let result = if enc.wants_cpu_input() {
                match cap.cpu_frame() {
                    Ok((data, stride)) => {
                        enc.encode(EncoderInput::Bgra { data, stride }, force_key)
                    }
                    Err(e) => {
                        tracing::info!("capture readback failed: {e:#}");
                        return Ok(Flow::Continue);
                    }
                }
            } else {
                #[cfg(windows)]
                {
                    enc.encode(
                        EncoderInput::D3D11 {
                            copy: &mut |dst, index| cap.copy_to_texture(dst, index),
                        },
                        force_key,
                    )
                }
                // The phone's screen share encodes itself.
                #[cfg(target_os = "android")]
                {
                    enc.encode(EncoderInput::Prepared, force_key)
                }
                #[cfg(not(any(windows, target_os = "android")))]
                unreachable!("only D3D11 encoders take GPU input")
            };
            match result {
                Ok(Some(out)) => {
                    encode_errors = 0;
                    let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed);
                    let header = FrameHeader {
                        surface: self.config.surface,
                        seq,
                        capture_us: crate::host_us()
                            .saturating_sub(capture_time.elapsed().as_micros() as u64),
                        encode_us: out.encode_us,
                        keyframe: out.keyframe,
                        codec,
                        width,
                        height,
                        full_chroma: enc.full_chroma(),
                    };
                    if force_key && !out.keyframe {
                        tracing::warn!("encoder ignored a keyframe request");
                    }
                    quarter.bytes += out.data.len() as u64;
                    self.send(Event::Frame(header, out.data))?;
                    window_encoded += 1;
                    quarter.encoded += 1;
                    if refresh {
                        refreshes_left -= 1;
                    }
                    dirty = false;
                    force_key = false;
                }
                Ok(None) => tracing::debug!("encoder buffered a frame"),
                Err(e) => {
                    encode_errors += 1;
                    tracing::warn!("encode failed: {e:#}");
                    if encode_errors >= 3 {
                        let (new, _) = self.open_encoder(cap, self.video_kbps(*bitrate))?;
                        *enc = new;
                        force_key = true;
                        encode_errors = 0;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Shared` whose last decode acknowledgement was `quiet` ago.
    fn shared(next: u64, decoded: u64, delivered: u64, quiet: Duration) -> Shared {
        let s = Shared {
            epoch: Instant::now() - quiet - Duration::from_secs(1),
            ..Default::default()
        };
        s.next_seq.store(next, Ordering::Relaxed);
        s.decoded_below.store(decoded, Ordering::Relaxed);
        s.delivered_below.store(delivered, Ordering::Relaxed);
        s.last_decoded_us.store(1_000_000, Ordering::Relaxed);
        s
    }

    #[test]
    fn paces_on_decoding() {
        assert_eq!(shared(10, 7, 10, Duration::ZERO).in_flight(), 3);
    }

    #[test]
    fn old_client_paces_on_delivery() {
        assert_eq!(shared(500, 0, 499, Duration::from_secs(60)).in_flight(), 1);
    }

    #[test]
    fn quiet_client_gets_a_trickle() {
        // Three quiet seconds: three more frames than decoded.
        assert_eq!(
            shared(500, 100, 499, Duration::from_millis(3500)).in_flight(),
            397
        );
        // Never more than 16, however long it stays quiet.
        assert_eq!(
            shared(500, 100, 499, Duration::from_secs(600)).in_flight(),
            384
        );
    }
}
