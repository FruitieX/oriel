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
    pub display: Option<u32>,
    pub surface: SurfaceId,
    pub codecs: Vec<Codec>,
    pub bitrate_kbps: u32,
    pub max_fps: u32,
    pub encoder: Option<String>,
}

pub enum Cmd {
    Keyframe,
    Bitrate(u32),
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
    /// The pipeline gave up; the session should end.
    Failed(String),
}

/// State shared with the session.
#[derive(Default)]
pub struct Shared {
    /// Sequence number of the next frame to send.
    pub next_seq: AtomicU64,
    /// Every frame below this has been received (by app-level ack or QUIC
    /// acknowledgement).
    pub acked_below: AtomicU64,
    pub rtt_us: AtomicU64,
    pub geometry: Mutex<Geometry>,
}

impl Shared {
    pub fn ack(&self, seq: u64) {
        self.acked_below.fetch_max(seq + 1, Ordering::Relaxed);
    }

    fn in_flight(&self) -> u64 {
        self.next_seq
            .load(Ordering::Relaxed)
            .saturating_sub(self.acked_below.load(Ordering::Relaxed))
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
                config,
                shared,
                test,
                cmds,
                events,
                clock: Instant::now(),
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
    clock: Instant,
}

enum Flow {
    Continue,
    Stop,
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
        loop {
            match capture::open(self.config.source, self.config.display, &self.test) {
                Ok(c) => {
                    if failures > 0 {
                        self.send(Event::Notice("Capture resumed".into()))?;
                    }
                    return Ok(Some(c));
                }
                Err(e) => {
                    failures += 1;
                    if failures == 1 {
                        tracing::warn!("cannot capture: {e:#}");
                        self.send(Event::Notice(format!("Capture unavailable: {e:#}")))?;
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

    fn open_encoder(&self, cap: &dyn Capturer, bitrate_kbps: u32) -> Result<(Encoder, Codec)> {
        let (width, height) = cap.size();
        let mut errors = Vec::new();
        for &codec in &self.config.codecs {
            if codec == Codec::PyroWave {
                continue;
            }
            let config = EncoderConfig {
                codec,
                width,
                height,
                fps: self.config.max_fps,
                bitrate_kbps,
                device: cap.hw_device(),
                encoder: self.config.encoder.clone(),
            };
            match Encoder::open(&config) {
                Ok(enc) => return Ok((enc, codec)),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        bail!("no usable encoder: {}", errors.join("; "))
    }

    fn run(&mut self) -> Result<()> {
        let mut bitrate = self.config.bitrate_kbps;
        loop {
            let Some(mut cap) = self.open_capturer()? else {
                return Ok(());
            };
            *self.shared.geometry.lock() = cap.geometry();
            let (mut enc, codec) = self.open_encoder(cap.as_ref(), bitrate)?;
            let (width, height) = cap.size();
            self.send(Event::Surface(SurfaceInfo {
                id: self.config.surface,
                width,
                height,
                codec,
            }))?;
            match self.stream(cap.as_mut(), &mut enc, codec, &mut bitrate)? {
                Flow::Stop => return Ok(()),
                Flow::Continue => {}
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
        let mut last_encode = Instant::now() - interval;
        let mut capture_time = Instant::now();
        let mut has_frame = false;
        let mut dirty = false;
        let mut force_key = true;
        let mut cursor_state: Option<CursorState> = None;
        let mut shape_id = 0u64;
        let mut encode_errors = 0u32;
        let mut blocked = false;

        loop {
            while let Ok(cmd) = self.cmds.try_recv() {
                match cmd {
                    Cmd::Keyframe => force_key = true,
                    Cmd::Bitrate(kbps) if kbps != *bitrate => {
                        *bitrate = kbps;
                        let (new, _) = self.open_encoder(cap, kbps)?;
                        *enc = new;
                        force_key = true;
                    }
                    Cmd::Bitrate(_) => {}
                    Cmd::Stop => return Ok(Flow::Stop),
                }
            }
            if self.events.is_closed() {
                return Ok(Flow::Stop);
            }

            let now = Instant::now();
            let next_slot = last_encode + interval;
            let timeout = if blocked {
                Duration::from_millis(1)
            } else if has_frame && (dirty || force_key) {
                next_slot.saturating_duration_since(now)
            } else {
                Duration::from_millis(16)
            };
            match cap.next_frame(timeout) {
                Ok(true) => {
                    has_frame = true;
                    dirty = true;
                    capture_time = Instant::now();
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::info!("capture interrupted: {e:#}");
                    return Ok(Flow::Continue);
                }
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

            blocked = false;
            if !(has_frame && (dirty || force_key)) || Instant::now() < next_slot {
                continue;
            }
            let rtt = Duration::from_micros(self.shared.rtt_us.load(Ordering::Relaxed));
            let limit = (rtt.as_secs_f64() / interval.as_secs_f64()).ceil() as u64 + 2;
            if self.shared.in_flight() >= limit.clamp(2, 16) {
                blocked = true;
                continue;
            }

            last_encode = Instant::now();
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
                #[cfg(not(windows))]
                unreachable!("only D3D11 encoders take GPU input")
            };
            match result {
                Ok(Some(out)) => {
                    encode_errors = 0;
                    let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed);
                    let header = FrameHeader {
                        surface: self.config.surface,
                        seq,
                        capture_us: capture_time.duration_since(self.clock).as_micros() as u64,
                        encode_us: out.encode_us,
                        keyframe: out.keyframe,
                        codec,
                        width,
                        height,
                    };
                    if force_key && !out.keyframe {
                        tracing::warn!("encoder ignored a keyframe request");
                    }
                    self.send(Event::Frame(header, out.data))?;
                    dirty = false;
                    force_key = false;
                }
                Ok(None) => tracing::debug!("encoder buffered a frame"),
                Err(e) => {
                    encode_errors += 1;
                    tracing::warn!("encode failed: {e:#}");
                    if encode_errors >= 3 {
                        let (new, _) = self.open_encoder(cap, *bitrate)?;
                        *enc = new;
                        force_key = true;
                        encode_errors = 0;
                    }
                }
            }
        }
    }
}
