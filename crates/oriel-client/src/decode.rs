//! Decoder thread: decodes frames in order and publishes the newest one.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use oriel_media::{DecodedFrame, Decoder};
use oriel_proto::{ClientMsg, Codec, Datagram, FrameHeader};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    session::{Sink, UiEvent},
    stats::Stats,
};

pub struct Ready {
    pub frame: DecodedFrame,
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

pub type Job = (FrameHeader, Vec<u8>, Instant);

pub fn spawn(
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    slot: FrameSlot,
    stats: Arc<Mutex<Stats>>,
    hardware: bool,
    sink: Sink,
) -> crossbeam_channel::Sender<Job> {
    let (tx, rx) = crossbeam_channel::unbounded::<Job>();
    std::thread::Builder::new()
        .name("oriel-decode".into())
        .spawn(move || {
            let mut decoder: Option<(Decoder, Codec, (u32, u32))> = None;
            let mut need_keyframe = false;
            let mut last_request = Instant::now() - Duration::from_secs(1);
            while let Ok((header, data, received)) = rx.recv() {
                let ack = Datagram::FrameAck {
                    surface: header.surface,
                    seq: header.seq,
                };
                let size = (header.width, header.height);
                if decoder
                    .as_ref()
                    .is_none_or(|(_, codec, s)| *codec != header.codec || *s != size)
                {
                    decoder = match Decoder::open(header.codec, header.width, header.height, hardware) {
                        Ok(d) => {
                            let mut s = stats.lock();
                            s.decoder = d.description().to_string();
                            s.codec = header.codec.name().to_string();
                            s.resolution = size;
                            Some((d, header.codec, size))
                        }
                        Err(e) => {
                            sink(UiEvent::Closed(format!(
                                "cannot decode {}: {e:#}",
                                header.codec.name()
                            )));
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
                        Ok(Some(frame)) => {
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
                                    frame,
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
