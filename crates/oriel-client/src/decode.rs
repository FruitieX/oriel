//! Decoder thread: decodes frames in order and publishes the newest one.

use std::{
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::Result;
#[cfg(zero_copy)]
use oriel_media::ColorMatrix;
#[cfg(target_os = "linux")]
use oriel_media::Frame;
use oriel_media::{DecodedFrame, Decoder};
use oriel_proto::{ClientMsg, Codec, Datagram, FrameHeader};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    session::{Sink, UiEvent},
    stats::Stats,
};

/// A decoded picture: planes in CPU memory, or (zero-copy) a set of GPU
/// textures the renderer samples directly.
pub enum Picture {
    Cpu(DecodedFrame),
    #[cfg(zero_copy)]
    Gpu(GpuPicture),
}

#[cfg(zero_copy)]
pub struct GpuPicture {
    pub set: Arc<crate::interop::PlaneSet>,
    pub matrix: ColorMatrix,
    pub full_range: bool,
    /// Keeps the decoder from reusing the memory while this is shown.
    _hold: Option<Box<dyn Send>>,
}

pub struct Ready {
    pub picture: Picture,
    pub header: FrameHeader,
    pub received: Instant,
    pub decoded: Instant,
}

/// Decoded frames waiting to be drawn, each when it is due (see
/// [`crate::playout`]).
#[derive(Clone)]
pub struct FrameSlot(pub Arc<Mutex<crate::playout::Playout<Ready>>>);

impl Default for FrameSlot {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(crate::playout::Playout::new(
            crate::playout::Smoothing::Off,
        ))))
    }
}

impl FrameSlot {
    /// The frame to draw now, if any.
    pub fn take(&self, now: Instant) -> Option<Ready> {
        self.0.lock().pop_due(now)
    }

    /// When the next queued frame is due (never without smoothing).
    pub fn next_due(&self) -> Option<Instant> {
        self.0.lock().next_due()
    }

    fn buffered(&self) -> bool {
        self.0.lock().smoothing() != crate::playout::Smoothing::Off
    }

    fn put(&self, ready: Ready) {
        let (capture_us, decoded) = (ready.header.capture_us, ready.decoded);
        self.0.lock().push(capture_us, decoded, ready);
    }
}

/// Set by the renderer once it knows whether GPU interop is possible.
#[cfg(zero_copy)]
pub type InteropSlot = Arc<Mutex<Option<Arc<crate::interop::VkInterop>>>>;
#[cfg(not(zero_copy))]
pub type InteropSlot = Arc<Mutex<Option<()>>>;

pub type Job = (FrameHeader, Vec<u8>, Instant);

/// Plane sets cycled through, so the ones queued or being drawn are not
/// overwritten.
#[cfg(zero_copy)]
const GPU_SETS: usize = 4 + crate::playout::MAX_QUEUED;

/// VAAPI surfaces imported as textures, by surface.
#[cfg(target_os = "linux")]
struct DmabufImports {
    interop: Arc<crate::interop::VkInterop>,
    /// Surface → (the memory it was imported from, its textures).
    sets: std::collections::HashMap<usize, (u64, Arc<crate::interop::PlaneSet>)>,
}

enum VideoDecoder {
    Cpu(Decoder),
    #[cfg(target_os = "linux")]
    Vaapi {
        decoder: Decoder,
        imports: DmabufImports,
    },
    #[cfg(zero_copy)]
    PyroGpu {
        decoder: oriel_media::PyroDecoder,
        sets: Vec<Arc<crate::interop::PlaneSet>>,
        next: usize,
    },
}

impl VideoDecoder {
    fn open(
        header: &FrameHeader,
        hardware: bool,
        interop: &InteropSlot,
        zero_copy: bool,
    ) -> Result<(Self, String)> {
        #[cfg(zero_copy)]
        if zero_copy
            && header.codec == Codec::PyroWave
            && let Some(interop) = interop.lock().clone()
        {
            match Self::open_pyro_gpu(header, &interop) {
                Ok(d) => return Ok((d, "pyrowave (Vulkan, zero-copy)".into())),
                Err(e) => tracing::warn!("zero-copy PyroWave unavailable: {e:#}"),
            }
        }
        // The DMA-BUF import takes NV12 only, not 4:4:4.
        #[cfg(target_os = "linux")]
        if zero_copy
            && hardware
            && header.codec != Codec::PyroWave
            && !header.full_chroma
            && let Some(interop) = interop.lock().clone()
            && interop.dmabuf
        {
            let d = Decoder::open_with(header.codec, header.width, header.height, true, true)?;
            let desc = format!("{} (zero-copy)", d.description());
            let imports = DmabufImports {
                interop,
                sets: Default::default(),
            };
            return Ok((
                Self::Vaapi {
                    decoder: d,
                    imports,
                },
                desc,
            ));
        }
        let _ = (interop, zero_copy);
        let d = Decoder::open(header.codec, header.width, header.height, hardware)?;
        let desc = d.description().to_string();
        Ok((Self::Cpu(d), desc))
    }

    #[cfg(zero_copy)]
    fn open_pyro_gpu(header: &FrameHeader, interop: &crate::interop::VkInterop) -> Result<Self> {
        let mut decoder = oriel_media::PyroDecoder::new_on_gpu(
            header.width,
            header.height,
            interop.device_uuid,
            interop.driver_uuid,
        )?;
        let mut sets = Vec::new();
        for _ in 0..GPU_SETS {
            let (set, planes) = interop.create_planes(header.width, header.height)?;
            decoder.import_planes(planes)?;
            sets.push(Arc::new(set));
        }
        Ok(Self::PyroGpu {
            decoder,
            sets,
            next: 0,
        })
    }

    /// The FFmpeg decoder's description once it fell back to software
    /// (the hardware could not take the stream, e.g. 4:4:4).
    fn software_description(&self) -> Option<&str> {
        let d = match self {
            Self::Cpu(d) => d,
            #[cfg(target_os = "linux")]
            Self::Vaapi { decoder, .. } => decoder,
            #[cfg(zero_copy)]
            Self::PyroGpu { .. } => return None,
        };
        Some(d.description()).filter(|d| d.contains("(software)"))
    }

    fn is_zero_copy(&self) -> bool {
        match self {
            Self::Cpu(_) => false,
            #[cfg(target_os = "linux")]
            Self::Vaapi { .. } => true,
            #[cfg(zero_copy)]
            Self::PyroGpu { .. } => true,
        }
    }

    /// Decode; an `Err(ZeroCopyFailed)` means to reopen without zero-copy.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        match self {
            Self::Cpu(d) => Ok(d.decode(data)?.map(Picture::Cpu)),
            #[cfg(target_os = "linux")]
            Self::Vaapi { decoder, imports } => match decoder.decode_frame(data)? {
                None => Ok(None),
                Some(Frame::Cpu(frame)) => Ok(Some(Picture::Cpu(frame))),
                Some(Frame::Dmabuf(frame)) => {
                    let set = match imports.sets.get(&frame.surface) {
                        Some((buffer, set))
                            if *buffer == frame.buffer
                                && set.size == (frame.width, frame.height) =>
                        {
                            set.clone()
                        }
                        _ => {
                            tracing::debug!(
                                surface = frame.surface,
                                buffer = frame.buffer,
                                "importing decoder surface"
                            );
                            let set = imports.interop.import_dmabuf(&frame).map_err(|e| {
                                anyhow::Error::new(ZeroCopyFailed(format!("{e:#}")))
                            })?;
                            let set = Arc::new(set);
                            // Replaces a stale import of a reused surface ID,
                            // which would show an old picture.
                            imports
                                .sets
                                .insert(frame.surface, (frame.buffer, set.clone()));
                            set
                        }
                    };
                    Ok(Some(Picture::Gpu(GpuPicture {
                        set,
                        matrix: frame.matrix,
                        full_range: frame.full_range,
                        _hold: Some(Box::new(frame)),
                    })))
                }
            },
            #[cfg(zero_copy)]
            Self::PyroGpu {
                decoder,
                sets,
                next,
            } => {
                let set = *next;
                if !decoder.decode_into(data, set)? {
                    return Ok(None);
                }
                *next = (set + 1) % sets.len();
                Ok(Some(Picture::Gpu(GpuPicture {
                    set: sets[set].clone(),
                    matrix: ColorMatrix::Bt709,
                    full_range: true,
                    _hold: None,
                })))
            }
        }
    }
}

#[derive(Debug)]
struct ZeroCopyFailed(String);

impl std::fmt::Display for ZeroCopyFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zero-copy import failed: {}", self.0)
    }
}

impl std::error::Error for ZeroCopyFailed {}

/// What the decoder thread is doing, for the watchdog.
#[derive(Default)]
struct Watch {
    /// Frames taken from the queue.
    jobs: AtomicU64,
    /// When the current call started (µs since `epoch`, 0 = idle) and what
    /// it is (`Phase`).
    busy_since_us: AtomicU64,
    phase: AtomicU8,
    done: AtomicBool,
    epoch: std::sync::OnceLock<Instant>,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Phase {
    Decode = 1,
    Open = 2,
}

impl Watch {
    fn now_us(&self) -> u64 {
        self.epoch.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
    }

    fn busy(&self, phase: Phase) -> impl Drop + '_ {
        self.phase.store(phase as u8, Ordering::Relaxed);
        self.busy_since_us.store(self.now_us(), Ordering::Relaxed);
        Idle(self)
    }
}

struct Idle<'a>(&'a Watch);

impl Drop for Idle<'_> {
    fn drop(&mut self) {
        self.0.busy_since_us.store(0, Ordering::Relaxed);
    }
}

struct WatchDone(Arc<Watch>);

impl Drop for WatchDone {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Relaxed);
        if std::thread::panicking() {
            tracing::error!("decoder thread panicked");
        }
    }
}

/// Log when video stops moving through the decoder, and why: a call into
/// the decoder that does not return, or no frames arriving at all. (Video
/// stopped after macOS had the app in the background, and the logs could
/// not tell these apart.)
fn spawn_watchdog(watch: Arc<Watch>, rx: crossbeam_channel::Receiver<Job>) {
    const AFTER: Duration = Duration::from_secs(2);
    let _ = std::thread::Builder::new()
        .name("oriel-decode-watch".into())
        .spawn(move || {
            let mut jobs = watch.jobs.load(Ordering::Relaxed);
            let mut moved = Instant::now();
            let mut stalled: Option<&str> = None;
            while !watch.done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let now_jobs = watch.jobs.load(Ordering::Relaxed);
                let since = watch.busy_since_us.load(Ordering::Relaxed);
                let busy = (since != 0)
                    .then(|| Duration::from_micros(watch.now_us().saturating_sub(since)));
                if now_jobs != jobs {
                    jobs = now_jobs;
                    moved = Instant::now();
                }
                let state = if busy.is_some_and(|b| b > AFTER) {
                    Some(match watch.phase.load(Ordering::Relaxed) {
                        2 => "opening the decoder",
                        _ => "decoding a frame",
                    })
                } else if busy.is_none() && rx.is_empty() && moved.elapsed() > AFTER {
                    Some("waiting for frames: none arrive")
                } else {
                    None
                };
                // No frames is also what a still screen looks like.
                let quiet = |what: &str| what.starts_with("waiting");
                match (stalled, state) {
                    (None, Some(what)) if quiet(what) => {
                        tracing::debug!("video stalled: {what}");
                    }
                    (None, Some(what)) => {
                        tracing::warn!(queued = rx.len(), "video stalled: {what}");
                    }
                    (Some(what), None) if quiet(what) => {
                        tracing::debug!("video moving again after: {what}");
                    }
                    (Some(what), None) => {
                        tracing::warn!(queued = rx.len(), "video moving again after: {what}");
                    }
                    _ => {}
                }
                stalled = state;
            }
        });
}

/// How long a received frame may wait for the decoder before the queue is
/// dropped in favour of a new keyframe.
const STALE: Duration = Duration::from_millis(250);

/// A decoder with what it was opened for: codec, size, 4:4:4, hardware.
type Opened = (VideoDecoder, Codec, (u32, u32), bool, bool);

pub fn spawn(
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    slot: FrameSlot,
    stats: Arc<Mutex<Stats>>,
    hardware_setting: Arc<std::sync::atomic::AtomicBool>,
    interop: InteropSlot,
    sink: Sink,
) -> crossbeam_channel::Sender<Job> {
    let (tx, rx) = crossbeam_channel::unbounded::<Job>();
    let watch = Arc::new(Watch::default());
    spawn_watchdog(watch.clone(), rx.clone());
    std::thread::Builder::new()
        .name("oriel-decode".into())
        .spawn(move || {
            let _done = WatchDone(watch.clone());
            // VideoToolbox decodes in another process, at the priority of
            // the thread that asks. A default-priority thread in an app macOS
            // has had in the background got its decodes held up for seconds
            // when the window came back.
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(
                    libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
                    0,
                );
            }
            let mut decoder: Option<Opened> = None;
            // Do not keep retrying zero-copy once it failed: [PyroWave,
            // VAAPI].
            // ORIEL_NO_ZERO_COPY=1 forces the CPU path (debugging aid).
            let mut zero_copy_failed = [std::env::var_os("ORIEL_NO_ZERO_COPY").is_some(); 2];
            let path = |codec: Codec| usize::from(codec != Codec::PyroWave);
            // Whether this codec can decode zero-copy with the current
            // renderer.
            let hardware_now = || hardware_setting.load(std::sync::atomic::Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            let zero_copy_possible = |codec: Codec, full_chroma: bool, hardware: bool| {
                interop.lock().as_ref().is_some_and(|i| {
                    codec == Codec::PyroWave || (hardware && i.dmabuf && !full_chroma)
                })
            };
            #[cfg(not(target_os = "linux"))]
            let zero_copy_possible = |codec: Codec, _full_chroma: bool, _hardware: bool| {
                codec == Codec::PyroWave && interop.lock().is_some()
            };
            let mut need_keyframe = false;
            let mut last_request = Instant::now() - Duration::from_secs(1);
            while let Ok((header, data, received)) = rx.recv() {
                watch.jobs.fetch_add(1, Ordering::Relaxed);
                // Fallen behind (the app was held up in the background):
                // decoding the backlog in order would keep the picture
                // frozen for as long as it took to build. Drop it and start
                // again from a fresh keyframe.
                if received.elapsed() > STALE && !rx.is_empty() {
                    let mut dropped = 1;
                    let mut header = header;
                    for (newer, ..) in rx.try_iter() {
                        header = newer;
                        dropped += 1;
                    }
                    tracing::info!(dropped, "decoder fell behind; skipping to a new keyframe");
                    // The frames are consumed: let the host pace on.
                    if let Ok(bytes) = oriel_proto::encode(&Datagram::FrameAck {
                        surface: header.surface,
                        seq: header.seq,
                    }) {
                        let _ = conn.send_datagram(bytes.into());
                    }
                    need_keyframe = true;
                    let _ = ctl.send(ClientMsg::RequestKeyframe);
                    last_request = Instant::now();
                    continue;
                }
                let hardware = hardware_now();
                let ack = Datagram::FrameAck {
                    surface: header.surface,
                    seq: header.seq,
                };
                let size = (header.width, header.height);
                // Reopen on a new stream, or on a keyframe to switch to
                // zero-copy once the renderer offers it (every PyroWave frame
                // is a keyframe).
                let upgrade = header.keyframe
                    && !zero_copy_failed[path(header.codec)]
                    && zero_copy_possible(header.codec, header.full_chroma, hardware)
                    && decoder.as_ref().is_some_and(|(d, ..)| !d.is_zero_copy());
                if upgrade
                    || decoder.as_ref().is_none_or(|(_, codec, s, full, hw)| {
                        *codec != header.codec
                            || *s != size
                            || *full != header.full_chroma
                            || *hw != hardware
                    })
                {
                    // Free the old decoder's GPU resources first.
                    drop(decoder.take());
                    let allow_zero_copy = !zero_copy_failed[path(header.codec)];
                    let opened = {
                        let _busy = watch.busy(Phase::Open);
                        VideoDecoder::open(&header, hardware, &interop, allow_zero_copy)
                    };
                    decoder = match opened {
                        Ok((d, description)) => {
                            tracing::info!(decoder = %description, "opened video decoder");
                            if zero_copy_possible(header.codec, header.full_chroma, hardware)
                                && !d.is_zero_copy()
                            {
                                zero_copy_failed[path(header.codec)] = true;
                            }
                            let mut s = stats.lock();
                            s.decoder = description;
                            s.codec = if header.full_chroma {
                                format!("{} 4:4:4", header.codec.name())
                            } else {
                                header.codec.name().to_string()
                            };
                            s.codec_id = Some(header.codec);
                            s.full_chroma = header.full_chroma;
                            s.resolution = size;
                            Some((d, header.codec, size, header.full_chroma, hardware))
                        }
                        Err(e) => {
                            sink(UiEvent::Closed {
                                reason: format!("cannot decode {}: {e:#}", header.codec.name()),
                                retry: false,
                            });
                            return;
                        }
                    };
                    need_keyframe = !header.keyframe;
                }
                if need_keyframe && !header.keyframe {
                    if last_request.elapsed() > Duration::from_millis(250) {
                        let _ = ctl.send(ClientMsg::RequestKeyframe);
                        last_request = Instant::now();
                    }
                } else {
                    let (dec, ..) = decoder.as_mut().unwrap();
                    let started = Instant::now();
                    let result = {
                        let _busy = watch.busy(Phase::Decode);
                        dec.decode(&data)
                    };
                    match result {
                        Ok(Some(picture)) => {
                            need_keyframe = false;
                            let decoded = Instant::now();
                            let mut s = stats.lock();
                            s.record(
                                data.len(),
                                decoded - started,
                                header.encode_us,
                                header.keyframe,
                            );
                            if let Some(d) = dec.software_description()
                                && s.decoder != d
                            {
                                tracing::info!(decoder = d, "hardware cannot decode this stream");
                                s.decoder = d.to_string();
                            }
                            drop(s);
                            // Without smoothing only the newest frame matters
                            // to the renderer.
                            if rx.is_empty() || slot.buffered() {
                                slot.put(Ready {
                                    picture,
                                    header,
                                    received,
                                    decoded,
                                });
                                sink(UiEvent::FrameReady);
                            }
                        }
                        Ok(None) => {}
                        Err(e) if e.is::<ZeroCopyFailed>() => {
                            tracing::warn!("{e:#}; decoding through the CPU");
                            zero_copy_failed[path(header.codec)] = true;
                            decoder = None;
                            need_keyframe = true;
                            let _ = ctl.send(ClientMsg::RequestKeyframe);
                            last_request = Instant::now();
                        }
                        Err(e) => {
                            tracing::warn!(seq = header.seq, "decode error: {e:#}");
                            need_keyframe = true;
                            let _ = ctl.send(ClientMsg::RequestKeyframe);
                            last_request = Instant::now();
                        }
                    }
                }
                if let Ok(bytes) = oriel_proto::encode(&ack) {
                    let _ = conn.send_datagram(bytes.into());
                }
            }
        })
        .expect("spawn decoder thread");
    tx
}
