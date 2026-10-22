//! Decoder thread: decodes frames in order and publishes the newest one.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_media::{DecodedFrame, Decoder};
use oriel_proto::{ClientMsg, Codec, Datagram, FrameHeader};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    session::{Sink, UiEvent},
    stats::Stats,
};

/// A decoded picture: planes in CPU memory, or (zero-copy PyroWave) a set
/// of GPU textures the renderer samples directly.
pub enum Picture {
    Cpu(DecodedFrame),
    #[cfg(not(target_os = "macos"))]
    Gpu(Arc<crate::interop::PlaneSet>),
}

pub struct Ready {
    pub picture: Picture,
    pub header: FrameHeader,
    pub received: Instant,
    pub decoded: Instant,
}

/// The newest decoded frame, waiting to be drawn.
#[derive(Clone, Default)]
pub struct FrameSlot(Arc<Mutex<Option<Ready>>>);

impl FrameSlot {
    pub fn take(&self) -> Option<Ready> {
        self.0.lock().take()
    }

    fn put(&self, ready: Ready) {
        *self.0.lock() = Some(ready);
    }
}

/// Set by the renderer once it knows whether GPU interop is possible.
#[cfg(not(target_os = "macos"))]
pub type InteropSlot = Arc<Mutex<Option<Arc<crate::interop::VkInterop>>>>;
#[cfg(target_os = "macos")]
pub type InteropSlot = Arc<Mutex<Option<()>>>;

pub type Job = (FrameHeader, Vec<u8>, Instant);

/// Plane sets cycled through, so the one being drawn is not overwritten.
#[cfg(not(target_os = "macos"))]
const GPU_SETS: usize = 3;

enum VideoDecoder {
    Cpu(Decoder),
    #[cfg(not(target_os = "macos"))]
    PyroGpu {
        decoder: oriel_media::PyroDecoder,
        sets: Vec<Arc<crate::interop::PlaneSet>>,
        next: usize,
    },
}

impl VideoDecoder {
    fn open(header: &FrameHeader, hardware: bool, interop: &InteropSlot) -> Result<(Self, String)> {
        #[cfg(not(target_os = "macos"))]
        if header.codec == Codec::PyroWave
            && let Some(interop) = interop.lock().clone()
        {
            match Self::open_pyro_gpu(header, &interop) {
                Ok(d) => return Ok((d, "pyrowave (Vulkan, zero-copy)".into())),
                Err(e) => tracing::warn!("zero-copy PyroWave unavailable: {e:#}"),
            }
        }
        let _ = interop;
        let d = Decoder::open(header.codec, header.width, header.height, hardware)?;
        let desc = d.description().to_string();
        Ok((Self::Cpu(d), desc))
    }

    #[cfg(not(target_os = "macos"))]
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

    fn is_zero_copy(&self) -> bool {
        match self {
            Self::Cpu(_) => false,
            #[cfg(not(target_os = "macos"))]
            Self::PyroGpu { .. } => true,
        }
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        match self {
            Self::Cpu(d) => Ok(d.decode(data)?.map(Picture::Cpu)),
            #[cfg(not(target_os = "macos"))]
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
                Ok(Some(Picture::Gpu(sets[set].clone())))
            }
        }
    }
}

pub fn spawn(
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    slot: FrameSlot,
    stats: Arc<Mutex<Stats>>,
    hardware: bool,
    interop: InteropSlot,
    sink: Sink,
) -> crossbeam_channel::Sender<Job> {
    let (tx, rx) = crossbeam_channel::unbounded::<Job>();
    std::thread::Builder::new()
        .name("oriel-decode".into())
        .spawn(move || {
            let mut decoder: Option<(VideoDecoder, Codec, (u32, u32))> = None;
            // Do not keep retrying zero-copy once it failed.
            let mut zero_copy_failed = false;
            let mut need_keyframe = false;
            let mut last_request = Instant::now() - Duration::from_secs(1);
            while let Ok((header, data, received)) = rx.recv() {
                let ack = Datagram::FrameAck {
                    surface: header.surface,
                    seq: header.seq,
                };
                let size = (header.width, header.height);
                // Reopen on a new stream, or to switch PyroWave to zero-copy
                // once the renderer offers it (every PyroWave frame is a
                // keyframe).
                let upgrade = header.codec == Codec::PyroWave
                    && !zero_copy_failed
                    && interop.lock().is_some()
                    && decoder.as_ref().is_some_and(|(d, ..)| !d.is_zero_copy());
                if upgrade
                    || decoder
                        .as_ref()
                        .is_none_or(|(_, codec, s)| *codec != header.codec || *s != size)
                {
                    // Free the old decoder's GPU resources first.
                    drop(decoder.take());
                    decoder = match VideoDecoder::open(&header, hardware, &interop) {
                        Ok((d, description)) => {
                            tracing::info!(decoder = %description, "opened video decoder");
                            if header.codec == Codec::PyroWave
                                && interop.lock().is_some()
                                && !d.is_zero_copy()
                            {
                                zero_copy_failed = true;
                            }
                            let mut s = stats.lock();
                            s.decoder = description;
                            s.codec = header.codec.name().to_string();
                            s.resolution = size;
                            Some((d, header.codec, size))
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
                    let (dec, _, _) = decoder.as_mut().unwrap();
                    let started = Instant::now();
                    match dec.decode(&data) {
                        Ok(Some(picture)) => {
                            need_keyframe = false;
                            let decoded = Instant::now();
                            stats.lock().record(
                                data.len(),
                                decoded - started,
                                header.encode_us,
                                header.keyframe,
                            );
                            // Only the newest frame matters to the renderer.
                            if rx.is_empty() {
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
