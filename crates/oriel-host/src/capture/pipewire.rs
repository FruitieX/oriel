//! Linux capture: frames from a PipeWire screen cast stream (shared memory,
//! BGRx), with the cursor as stream metadata.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use oriel_proto::CursorShape;
use parking_lot::{Condvar, Mutex};
use pipewire as pw;
use pw::spa;

use super::{Capturer, Geometry};
use crate::{
    cursor::{self, CursorPos},
    gnome::GnomeSession,
};

#[derive(Default)]
struct Latest {
    seq: u64,
    data: Vec<u8>,
    stride: usize,
    size: (u32, u32),
    cursor: CursorPos,
    shape: Option<CursorShape>,
    shape_id: u32,
    error: Option<String>,
}

type Shared = Arc<(Mutex<Latest>, Condvar)>;

pub struct PipeWireCapturer {
    session: Arc<GnomeSession>,
    shared: Shared,
    seen: u64,
    frame: Vec<u8>,
    stride: usize,
    size: (u32, u32),
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PipeWireCapturer {
    pub fn open(target: super::Target) -> Result<Self> {
        let session = GnomeSession::shared(target)?;
        let mode = match target {
            super::Target::Virtual(mode) => Some(mode),
            super::Target::Display(_) => None,
        };
        let shared: Shared = Default::default();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let node_id = session.node_id;
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("oriel-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_stream(node_id, mode, thread_shared.clone(), quit_rx) {
                    tracing::error!("PipeWire capture failed: {e:#}");
                    let (latest, cv) = &*thread_shared;
                    latest.lock().error = Some(format!("{e:#}"));
                    cv.notify_all();
                }
            })?;
        let mut capturer = Self {
            session,
            shared,
            seen: 0,
            frame: Vec::new(),
            stride: 0,
            size: (0, 0),
            quit: Some(quit_tx),
            thread: Some(thread),
        };
        // Learn the size from the first frame, but leave the frame for the
        // pipeline (a static desktop may not send another).
        {
            let (latest, cv) = &*capturer.shared;
            let mut l = latest.lock();
            let deadline = Instant::now() + Duration::from_secs(5);
            while l.seq == 0 && l.error.is_none() {
                if cv.wait_until(&mut l, deadline).timed_out() {
                    bail!("no frames from the PipeWire stream");
                }
            }
            if let Some(e) = &l.error {
                bail!("{e}");
            }
            capturer.size = l.size;
        }
        Ok(capturer)
    }
}

impl Drop for PipeWireCapturer {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Capturer for PipeWireCapturer {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            width: self.size.0,
            height: self.size.1,
            ..self.session.geometry
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        let (latest, cv) = &*self.shared;
        let mut l = latest.lock();
        if l.seq == self.seen && l.error.is_none() {
            cv.wait_for(&mut l, timeout);
        }
        if let Some(e) = &l.error {
            bail!("{e}");
        }
        if l.seq == self.seen {
            return Ok(false);
        }
        self.seen = l.seq;
        // Size changes need a new encoder: report as a capture restart.
        if self.size != (0, 0) && self.size != l.size {
            bail!("screen size changed");
        }
        self.size = l.size;
        self.stride = l.stride;
        std::mem::swap(&mut self.frame, &mut l.data);
        Ok(true)
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        if self.frame.is_empty() {
            bail!("no frame yet");
        }
        Ok((&self.frame, self.stride))
    }

    fn keep_alive(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        Some(self.session.clone())
    }

    fn cursor(&mut self) -> CursorPos {
        self.shared.0.lock().cursor
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        self.shared.0.lock().shape.take()
    }
}

/// Metadata size for a cursor bitmap of up to `n`×`n` pixels.
const fn cursor_meta_size(n: usize) -> usize {
    size_of::<spa::sys::spa_meta_cursor>() + size_of::<spa::sys::spa_meta_bitmap>() + n * n * 4
}

/// Room for cursors up to 384×384 (Mutter's largest, at high scales).
const CURSOR_META_SIZE: usize = cursor_meta_size(384);

/// Capture node `node_id`. For a virtual monitor, `mode` fixes its size and
/// refresh rate (Mutter sizes the monitor from the negotiated format).
fn run_stream(
    node_id: u32,
    mode: Option<oriel_proto::VirtualMode>,
    shared: Shared,
    quit: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connect to PipeWire")?;
    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |_| ml.quit());

    let stream = pw::stream::StreamBox::new(
        &core,
        "oriel-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let format_info = Arc::new(Mutex::new(spa::param::video::VideoInfoRaw::default()));
    let fmt = format_info.clone();
    let process_shared = shared.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| {
            tracing::info!(?old, ?new, "PipeWire stream state");
        })
        .param_changed(move |stream, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = fmt.lock();
            if info.parse(param).is_err() {
                return;
            }
            tracing::info!(
                format = ?info.format(),
                width = info.size().width,
                height = info.size().height,
                "PipeWire stream format"
            );
            // Ask for the cursor as metadata, in shared memory buffers.
            let meta = pod_bytes(spa::pod::Value::Object(spa::pod::Object {
                type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
                id: spa::sys::SPA_PARAM_Meta,
                properties: vec![
                    spa::pod::Property::new(
                        spa::sys::SPA_PARAM_META_type,
                        spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor)),
                    ),
                    // A range, as gnome-remote-desktop asks: a fixed size
                    // that differs from what Mutter offers is refused, and
                    // then no cursor arrives at all.
                    spa::pod::Property::new(
                        spa::sys::SPA_PARAM_META_size,
                        spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                            spa::utils::ChoiceFlags::empty(),
                            spa::utils::ChoiceEnum::Range {
                                default: CURSOR_META_SIZE as i32,
                                min: cursor_meta_size(1) as i32,
                                max: CURSOR_META_SIZE as i32,
                            },
                        ))),
                    ),
                ],
            }));
            let buffers = pod_bytes(spa::pod::Value::Object(spa::pod::Object {
                type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
                id: spa::sys::SPA_PARAM_Buffers,
                properties: vec![spa::pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    spa::pod::Value::Int(
                        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr),
                    ),
                )],
            }));
            let mut params = [
                spa::pod::Pod::from_bytes(&meta).unwrap(),
                spa::pod::Pod::from_bytes(&buffers).unwrap(),
            ];
            if let Err(e) = stream.update_params(&mut params) {
                tracing::warn!("cannot request cursor metadata: {e}");
            }
        })
        .process(move |stream, _| unsafe {
            let raw = stream.dequeue_raw_buffer();
            if raw.is_null() {
                return;
            }
            let info = *format_info.lock();
            read_buffer(&process_shared, raw, &info);
            stream.queue_raw_buffer(raw);
        })
        .register()?;

    let format = match mode {
        Some(mode) => virtual_format(mode),
        None => monitor_format(),
    };
    let mut params = [spa::pod::Pod::from_bytes(&format).unwrap()];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    mainloop.run();
    Ok(())
}

/// A virtual monitor's format: exactly this size and rate.
fn virtual_format(mode: oriel_proto::VirtualMode) -> Vec<u8> {
    pod_bytes(spa::pod::Value::Object(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle {
                width: mode.width,
                height: mode.height
            }
        ),
        // Variable frame rate (as Mutter offers it); the maximum becomes
        // the monitor's refresh rate.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction {
                num: mode.refresh_hz,
                denom: 1
            },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: mode.refresh_hz,
                denom: 1
            }
        ),
    )))
}

/// A monitor's format: whatever size it has, any rate.
fn monitor_format() -> Vec<u8> {
    pod_bytes(spa::pod::Value::Object(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    )))
}

fn pod_bytes(value: spa::pod::Value) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .expect("serialize pod")
        .0
        .into_inner()
}

/// Copy a buffer's frame and cursor into `shared`.
unsafe fn read_buffer(
    shared: &Shared,
    raw: *mut pw::sys::pw_buffer,
    info: &spa::param::video::VideoInfoRaw,
) {
    unsafe {
        let buf = (*raw).buffer;
        if buf.is_null() {
            return;
        }
        let (latest, cv) = &**shared;
        let mut l = latest.lock();

        // Cursor metadata.
        let cursor = spa::sys::spa_buffer_find_meta_data(
            buf,
            spa::sys::SPA_META_Cursor,
            size_of::<spa::sys::spa_meta_cursor>(),
        ) as *const spa::sys::spa_meta_cursor;
        if !cursor.is_null() {
            let c = &*cursor;
            if c.id != 0 {
                l.cursor = CursorPos {
                    visible: true,
                    captured: false,
                    x: c.position.x,
                    y: c.position.y,
                };
                if c.bitmap_offset as usize >= size_of::<spa::sys::spa_meta_cursor>() {
                    let bitmap = (cursor as *const u8).add(c.bitmap_offset as usize)
                        as *const spa::sys::spa_meta_bitmap;
                    let b = &*bitmap;
                    let (w, h) = (b.size.width, b.size.height);
                    if w > 0 && h > 0 && w <= 384 && h <= 384 {
                        let pixels = (bitmap as *const u8).add(b.offset as usize);
                        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                        let bgra = b.format == spa::sys::SPA_VIDEO_FORMAT_BGRA;
                        for y in 0..h as usize {
                            let row = std::slice::from_raw_parts(
                                pixels.add(y * b.stride as usize),
                                w as usize * 4,
                            );
                            for px in row.as_chunks::<4>().0 {
                                if bgra {
                                    rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                                } else {
                                    rgba.extend_from_slice(px);
                                }
                            }
                        }
                        let shape = cursor::make_shape(
                            w,
                            h,
                            c.hotspot.x.max(0) as u32,
                            c.hotspot.y.max(0) as u32,
                            rgba,
                        );
                        l.shape_id = c.id;
                        l.shape = Some(shape);
                    }
                }
            } else {
                l.cursor.visible = false;
            }
        }

        // Video frame (absent when only the cursor moved).
        if (*buf).n_datas == 0 {
            return;
        }
        let data = &*(*buf).datas;
        let chunk = &*data.chunk;
        static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            tracing::debug!(
                data_type = data.type_,
                mapped = !data.data.is_null(),
                size = chunk.size,
                offset = chunk.offset,
                stride = chunk.stride,
                flags = chunk.flags,
                "PipeWire buffer"
            );
        }
        if data.data.is_null()
            || chunk.size == 0
            || chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0
        {
            return;
        }
        let (w, h) = (info.size().width, info.size().height);
        let stride = if chunk.stride > 0 {
            chunk.stride as usize
        } else {
            w as usize * 4
        };
        let len = stride * h as usize;
        if (chunk.size as usize) < len {
            return;
        }
        let src =
            std::slice::from_raw_parts((data.data as *const u8).add(chunk.offset as usize), len);
        l.data.clear();
        l.data.extend_from_slice(src);
        l.stride = stride;
        l.size = (w, h);
        l.seq += 1;
        drop(l);
        cv.notify_all();
    }
}
