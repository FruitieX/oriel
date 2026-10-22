//! The network side of a streaming session: control messages, video
//! frames, datagrams and input. Runs on the tokio runtime and reports to
//! the UI through a callback.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_net::{ControlStream, read_msg, write_msg};
use oriel_proto::{ClientMsg, Datagram, FrameHeader, HostMsg, InputEvent, StreamSettings};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{decode, stats::Stats};

pub enum UiEvent {
    Host(HostMsg),
    CursorPosition {
        x: i32,
        y: i32,
    },
    FrameReady,
    /// Keys were taken by the Windows keyboard hook.
    HookKeys,
    /// The connection ended. `retry` when it was lost (timeout, network)
    /// rather than closed on purpose.
    Closed {
        reason: String,
        retry: bool,
    },
}

pub type Sink = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Handle the UI uses to talk to the session.
#[derive(Clone)]
pub struct Session {
    pub conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    input: mpsc::UnboundedSender<InputEvent>,
    pub frames: decode::FrameSlot,
    pub stats: Arc<Mutex<Stats>>,
    pub audio: Option<Arc<Mutex<oriel_audio::PlayerStats>>>,
    pub clock: Arc<Mutex<crate::stats::ClockSync>>,
    /// The renderer offers zero-copy decoding here once it exists.
    pub interop: decode::InteropSlot,
    /// Gamepads are forwarded only while the viewer window has focus.
    pub focused: Arc<std::sync::atomic::AtomicBool>,
    mic: Arc<Mutex<Microphone>>,
    rt: tokio::runtime::Handle,
}

impl Session {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.ctl.send(msg);
    }

    pub fn input(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Start or stop sending the microphone.
    pub fn set_microphone(&self, on: bool) -> Result<()> {
        self.mic.lock().set(on)
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    pub fn close(&self) {
        self.send(ClientMsg::Bye);
        let conn = self.conn.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            conn.close(0u32.into(), b"bye");
        });
    }
}

/// What a session asks for and uses locally.
pub struct SessionOptions {
    pub settings: StreamSettings,
    pub hardware_decode: bool,
    /// Local output for the host's sound (`None`: the default device).
    pub playback_device: Option<String>,
    /// Forward local gamepads while the viewer has focus.
    pub gamepads: bool,
    /// Send the microphone from the start.
    pub microphone: bool,
    /// Which input (`None`: the default one).
    pub microphone_device: Option<String>,
}

/// The local microphone, sent to the host while on.
struct Microphone {
    conn: quinn::Connection,
    device: Option<String>,
    /// Continues across on/off, so the host never sees stale packets.
    seq: Arc<std::sync::atomic::AtomicU32>,
    capture: Option<oriel_audio::Capture>,
}

impl Microphone {
    fn set(&mut self, on: bool) -> Result<()> {
        if !on {
            self.capture = None;
            return Ok(());
        }
        if self.capture.is_some() {
            return Ok(());
        }
        let (conn, seq) = (self.conn.clone(), self.seq.clone());
        self.capture = Some(oriel_audio::start_capture(
            oriel_audio::CaptureSource::Microphone(self.device.clone()),
            64_000,
            move |data| {
                let seq = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(bytes) = oriel_proto::encode(&Datagram::Microphone { seq, data }) {
                    let _ = conn.send_datagram(bytes.into());
                }
            },
        )?);
        Ok(())
    }
}

pub fn start(
    rt: &tokio::runtime::Handle,
    conn: quinn::Connection,
    ctl: ControlStream,
    options: SessionOptions,
    sink: Sink,
) -> Session {
    let SessionOptions {
        settings,
        hardware_decode,
        playback_device,
        gamepads,
        microphone,
        microphone_device,
    } = options;
    let mut mic = Microphone {
        conn: conn.clone(),
        device: microphone_device,
        seq: Default::default(),
        capture: None,
    };
    if microphone && let Err(e) = mic.set(true) {
        tracing::warn!("no microphone: {e:#}");
    }
    let _guard = rt.enter();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let stats = Arc::new(Mutex::new(Stats::default()));
    let frames = decode::FrameSlot::default();
    let interop = decode::InteropSlot::default();
    let decoder = decode::spawn(
        conn.clone(),
        ctl_tx.clone(),
        frames.clone(),
        stats.clone(),
        hardware_decode,
        interop.clone(),
        sink.clone(),
    );

    let player = if settings.audio {
        match oriel_audio::Player::start(oriel_audio::Output::Device(playback_device)) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("no audio output: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let audio_stats = player.as_ref().map(|p| p.stats.clone());

    let ControlStream { mut send, mut recv } = ctl;
    let _ = ctl_tx.send(ClientMsg::StartStream(settings));
    tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    // Clock sync: ping once a second, keep the tightest round trip.
    let clock = Arc::new(Mutex::new(crate::stats::ClockSync::default()));
    let pings: Arc<Mutex<std::collections::HashMap<u64, Instant>>> = Default::default();
    let ping_tx = ctl_tx.clone();
    let ping_times = pings.clone();
    tokio::spawn(async move {
        for id in 0u64.. {
            ping_times.lock().insert(id, Instant::now());
            if ping_tx.send(ClientMsg::Ping { id }).is_err() {
                break;
            }
            ping_times.lock().retain(|k, _| id.saturating_sub(*k) < 10);
            tokio::time::sleep(Duration::from_millis(if id < 5 { 100 } else { 1000 })).await;
        }
    });

    let focused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rumble =
        gamepads.then(|| crate::gamepad::spawn(conn.clone(), ctl_tx.clone(), focused.clone()));

    let reader_sink = sink.clone();
    let reader_clock = clock.clone();
    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_msg::<HostMsg>(&mut recv).await {
            if let HostMsg::Pong { id, host_us } = msg {
                if let Some(sent) = pings.lock().remove(&id) {
                    reader_clock.lock().sample(sent, Instant::now(), host_us);
                }
                continue;
            }
            if let HostMsg::Rumble { pad, strong, weak } = msg {
                if let Some(r) = &rumble {
                    let _ = r.send((pad, strong, weak));
                }
                continue;
            }
            reader_sink(UiEvent::Host(msg));
        }
    });

    let input_conn = conn.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            let mut stream = input_conn.open_uni().await?;
            // Input jumps the queue ahead of anything else we send.
            stream.set_priority(i32::MAX)?;
            while let Some(event) = input_rx.recv().await {
                let mut batch = oriel_proto::encode_framed(&event)?;
                while let Ok(more) = input_rx.try_recv() {
                    batch.extend(oriel_proto::encode_framed(&more)?);
                }
                stream.write_all(&batch).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!("input stream ended: {e:#}");
        }
    });

    let frame_conn = conn.clone();
    // Debugging aid: emulate a link of this many kbit/s.
    let limit_kbps: Option<u64> = std::env::var("ORIEL_DEBUG_LIMIT_KBPS")
        .ok()
        .and_then(|v| v.parse().ok());
    tokio::spawn(async move {
        let result: Result<()> = async {
            loop {
                let mut stream = frame_conn.accept_uni().await?;
                let Some(header) = read_msg::<FrameHeader>(&mut stream).await? else {
                    continue;
                };
                let data = stream.read_to_end(256 * 1024 * 1024).await?;
                if let Some(kbps) = limit_kbps {
                    let secs = data.len() as f64 * 8.0 / (kbps as f64 * 1000.0);
                    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                }
                if decoder.send((header, data, Instant::now())).is_err() {
                    break;
                }
            }
            Ok(())
        }
        .await;
        tracing::debug!("frame receiver ended: {result:?}");
    });

    let dg_conn = conn.clone();
    let dg_sink = sink.clone();
    tokio::spawn(async move {
        while let Ok(bytes) = dg_conn.read_datagram().await {
            match oriel_proto::decode(&bytes) {
                Ok(Datagram::CursorPosition { x, y, .. }) => {
                    dg_sink(UiEvent::CursorPosition { x, y })
                }
                Ok(Datagram::Audio { seq, data }) => {
                    if let Some(p) = &player {
                        p.push(seq, data);
                    }
                }
                _ => {}
            }
        }
    });

    let closed_conn = conn.clone();
    tokio::spawn(async move {
        let reason = closed_conn.closed().await;
        let (reason, retry) = match reason {
            quinn::ConnectionError::ApplicationClosed(c) => {
                (String::from_utf8_lossy(&c.reason).into_owned(), false)
            }
            quinn::ConnectionError::LocallyClosed => ("disconnected".into(), false),
            quinn::ConnectionError::TimedOut => ("connection timed out".into(), true),
            other => (other.to_string(), true),
        };
        sink(UiEvent::Closed { reason, retry });
    });

    Session {
        conn,
        ctl: ctl_tx,
        input: input_tx,
        frames,
        stats,
        audio: audio_stats,
        clock,
        interop,
        focused,
        mic: Arc::new(Mutex::new(mic)),
        rt: rt.clone(),
    }
}
