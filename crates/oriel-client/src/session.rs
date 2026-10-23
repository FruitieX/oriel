//! The network side of a streaming session: control messages, video
//! frames, datagrams and input. Runs on the tokio runtime and reports to
//! the UI through a callback.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_net::{ControlStream, read_known, read_msg, write_msg};
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
    /// Android: typed text, insets or the keyboard changed (see
    /// `crate::android`).
    Platform,
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
    pub clock: Arc<Mutex<crate::stats::ClockSync>>,
    /// When the host last answered a ping.
    pub last_pong: Arc<Mutex<Instant>>,
    /// The renderer offers zero-copy decoding here once it exists.
    pub interop: decode::InteropSlot,
    /// Gamepads are forwarded only while the viewer window has focus.
    pub focused: Arc<std::sync::atomic::AtomicBool>,
    mic: Arc<Mutex<Microphone>>,
    speaker: Arc<Mutex<Speaker>>,
    gamepads: Arc<Mutex<Gamepads>>,
    rt: tokio::runtime::Handle,
}

impl Session {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.ctl.send(msg);
    }

    pub fn input(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Start or stop sending the microphone, from `device`.
    pub fn set_microphone(&self, on: bool, device: Option<String>) -> Result<()> {
        let mut mic = self.mic.lock();
        if mic.device != device {
            mic.device = device;
            mic.capture = None;
        }
        mic.set(on)
    }

    /// Play the host's sound on `device`, or (`None`) stop it.
    pub fn set_speaker(&self, device: Option<Option<String>>) -> Result<()> {
        self.speaker.lock().set(device)
    }

    pub fn audio_stats(&self) -> Option<oriel_audio::PlayerStats> {
        Some(*self.speaker.lock().player.as_ref()?.0.stats.lock())
    }

    /// Forward gamepads (while the window has focus) or not.
    pub fn set_gamepads(&self, on: bool) {
        self.gamepads.lock().set(on);
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

/// Where the host's sound plays.
struct Speaker {
    player: Option<(oriel_audio::Player, Option<String>)>,
}

impl Speaker {
    fn set(&mut self, device: Option<Option<String>>) -> Result<()> {
        let Some(device) = device else {
            self.player = None;
            return Ok(());
        };
        if self.player.as_ref().is_some_and(|(_, d)| *d == device) {
            return Ok(());
        }
        // Close the old output before opening the new one.
        self.player = None;
        let player = oriel_audio::Player::start(oriel_audio::Output::Device(device.clone()))?;
        self.player = Some((player, device));
        Ok(())
    }
}

/// Gamepad forwarding, started on first use.
struct Gamepads {
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    focused: Arc<std::sync::atomic::AtomicBool>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    rumble: Option<crate::gamepad::RumbleSender>,
}

impl Gamepads {
    fn set(&mut self, on: bool) {
        self.enabled.store(on, std::sync::atomic::Ordering::Relaxed);
        if on && self.rumble.is_none() {
            self.rumble = Some(crate::gamepad::spawn(
                self.conn.clone(),
                self.ctl.clone(),
                self.focused.clone(),
                self.enabled.clone(),
            ));
        }
    }
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

    let speaker = Arc::new(Mutex::new(Speaker { player: None }));
    if settings.audio
        && let Err(e) = speaker.lock().set(Some(playback_device))
    {
        tracing::warn!("no audio output: {e:#}");
    }

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
    let pads = Arc::new(Mutex::new(Gamepads {
        conn: conn.clone(),
        ctl: ctl_tx.clone(),
        focused: focused.clone(),
        enabled: Default::default(),
        rumble: None,
    }));
    pads.lock().set(gamepads);
    let rumble_pads = pads.clone();

    // Images on the clipboard (text goes through the window system).
    let image_tx = ctl_tx.clone();
    let clipboard = oriel_clipboard::start(
        oriel_clipboard::Options {
            text: false,
            active: Some(focused.clone()),
        },
        move |content| {
            if let oriel_clipboard::Content::Image(png) = content {
                let _ = image_tx.send(ClientMsg::ClipboardImage { png });
            }
        },
    );

    let reader_sink = sink.clone();
    let reader_clock = clock.clone();
    let last_pong = Arc::new(Mutex::new(Instant::now()));
    let reader_pong = last_pong.clone();
    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_known::<HostMsg>(&mut recv).await {
            if let HostMsg::Pong { id, host_us } = msg {
                *reader_pong.lock() = Instant::now();
                if let Some(sent) = pings.lock().remove(&id) {
                    reader_clock.lock().sample(sent, Instant::now(), host_us);
                }
                continue;
            }
            if let HostMsg::ClipboardImage { png } = msg {
                if png.len() <= oriel_clipboard::MAX_IMAGE {
                    let _ = clipboard.set.send(oriel_clipboard::Content::Image(png));
                }
                continue;
            }
            if let HostMsg::Rumble { pad, strong, weak } = msg {
                if let Some(r) = &rumble_pads.lock().rumble {
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
    let frame_stats = stats.clone();
    let frame_clock = clock.clone();
    // Debugging aid: emulate a link of this many kbit/s.
    let limit_kbps: Option<u64> = std::env::var("ORIEL_DEBUG_LIMIT_KBPS")
        .ok()
        .and_then(|v| v.parse().ok());
    // Debugging aid: hold up every third frame this many ms (jitter).
    let jitter_ms: Option<u64> = std::env::var("ORIEL_DEBUG_JITTER_MS")
        .ok()
        .and_then(|v| v.parse().ok());
    tokio::spawn(async move {
        let result: Result<()> = async {
            loop {
                let mut stream = frame_conn.accept_uni().await?;
                // A frame from a host with another protocol version may
                // not decode; drop just that frame.
                let Ok(Some(header)) = read_msg::<FrameHeader>(&mut stream).await else {
                    continue;
                };
                let local_us = frame_clock.lock().local_us(Instant::now());
                frame_stats.lock().arrived(header.capture_us, local_us);
                let data = stream.read_to_end(256 * 1024 * 1024).await?;
                if let Some(ms) = jitter_ms
                    && header.seq.is_multiple_of(3)
                {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
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
    let dg_speaker = speaker.clone();
    let dg_sink = sink.clone();
    tokio::spawn(async move {
        while let Ok(bytes) = dg_conn.read_datagram().await {
            match oriel_proto::decode(&bytes) {
                Ok(Datagram::CursorPosition { x, y, .. }) => {
                    dg_sink(UiEvent::CursorPosition { x, y })
                }
                Ok(Datagram::Audio { seq, data }) => {
                    if let Some((p, _)) = &dg_speaker.lock().player {
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
        clock,
        last_pong,
        interop,
        focused,
        speaker,
        gamepads: pads,
        mic: Arc::new(Mutex::new(mic)),
        rt: rt.clone(),
    }
}
