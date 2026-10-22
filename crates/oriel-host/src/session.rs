//! One streaming session with an authenticated client.

use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    time::Duration,
};

use anyhow::Result;
use oriel_net::{ControlStream, read_msg, write_msg};
use oriel_proto::{ClientMsg, Datagram, FrameHeader, HostMsg, InputEvent};
use tokio::sync::mpsc as tmpsc;

use crate::{
    capture::TestState,
    input,
    pipeline::{self, Cmd, Event, Shared},
    server::HostOptions,
};

pub async fn run(
    conn: quinn::Connection,
    mut ctl: ControlStream,
    options: Arc<HostOptions>,
    test: TestState,
    status: Arc<parking_lot::Mutex<crate::server::HostStatus>>,
) -> Result<()> {
    let settings = loop {
        match ctl.recv().await? {
            ClientMsg::StartStream(s) => break s,
            ClientMsg::Ping { id } => {
                ctl.send(&HostMsg::Pong {
                    id,
                    host_us: crate::host_us(),
                })
                .await?
            }
            ClientMsg::Bye => return Ok(()),
            other => tracing::debug!(?other, "ignoring message before stream start"),
        }
    };
    tracing::info!(?settings, "starting stream");

    let shared = Arc::new(Shared::default());
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (ev_tx, mut ev_rx) = tmpsc::unbounded_channel();
    let ev_tx_audio = ev_tx.clone();
    let mut codecs = settings.codecs.clone();
    if let Some(codec) = options.codec {
        codecs.retain(|c| *c == codec);
    }
    let pipeline = pipeline::spawn(
        pipeline::Config {
            source: options.source,
            display: settings.display,
            surface: 1,
            codecs,
            bitrate_kbps: if settings.bitrate_kbps == 0 {
                options.default_bitrate_kbps
            } else {
                settings.bitrate_kbps
            },
            max_fps: settings.max_fps.clamp(1, 240),
            encoder: options.encoder.clone(),
            auto_codec: settings.auto_codec,
            pyrowave_kbps: settings.pyrowave_kbps.max(10_000),
        },
        shared.clone(),
        test.clone(),
        cmd_rx,
        ev_tx,
    );

    let clip_events = ev_tx_audio.clone();
    let clipboard = crate::clipboard::start(move |text| {
        let _ = clip_events.send(Event::Clipboard(text));
    });

    // The display mode chosen by the client, restored when dropped.
    let mut mode_guard: Option<crate::capture::ModeGuard> = None;
    if let Some(res) = settings.resolution {
        match crate::capture::set_display_mode(options.source, settings.display, res) {
            Ok(g) => mode_guard = Some(g),
            Err(e) => {
                let _ = ev_tx_audio.send(Event::Notice(format!(
                    "Cannot change the resolution: {e:#}"
                )));
            }
        }
    }
    let mut display = settings.display;

    // Continues across device switches, so the client's jitter buffer
    // does not mistake the new packets for stale ones.
    let audio_seq = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut audio = if settings.audio {
        start_audio(
            &conn,
            options.source,
            settings.audio_device.clone(),
            audio_seq.clone(),
            &ev_tx_audio,
        )
    } else {
        None
    };

    // Input: a dedicated thread owns the injector (Windows wants input
    // injected from a thread attached to the input desktop).
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    let injector_shared = shared.clone();
    let source = options.source;
    let injector_test = test.clone();
    let injector = std::thread::Builder::new()
        .name("oriel-input".into())
        .spawn(move || {
            let mut injector = match input::open(source, &injector_test) {
                Ok(i) => i,
                Err(e) => {
                    tracing::error!("input injection unavailable: {e:#}");
                    return;
                }
            };
            while let Ok(event) = input_rx.recv() {
                let geometry = *injector_shared.geometry.lock();
                if let Err(e) = injector.inject(event, geometry) {
                    tracing::debug!(?event, "inject failed: {e:#}");
                }
            }
            let geometry = *injector_shared.geometry.lock();
            let _ = injector.inject(InputEvent::ReleaseAll, geometry);
        })?;

    let input_conn = conn.clone();
    let input_task = tokio::spawn(async move {
        let mut stream = input_conn.accept_uni().await?;
        while let Some(event) = read_msg::<InputEvent>(&mut stream).await? {
            if input_tx.send(event).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    });

    let (ctl_tx, mut ctl_rx) = tmpsc::unbounded_channel();
    let ControlStream {
        send: mut ctl_send,
        recv: mut ctl_recv,
    } = ctl;
    let ctl_task = tokio::spawn(async move {
        while let Some(msg) = read_msg::<ClientMsg>(&mut ctl_recv).await? {
            if ctl_tx.send(msg).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    });

    // Virtual gamepads, created when a client's pad first reports.
    let (pad_tx, pad_rx) = crossbeam_channel::unbounded();
    let rumble_events = ev_tx_audio.clone();
    let pad_notices = ev_tx_audio.clone();
    let gamepads = crate::gamepad::spawn(
        pad_rx,
        Arc::new(move |pad, strong, weak| {
            let _ = rumble_events.send(Event::Rumble { pad, strong, weak });
        }),
        move |text| {
            let _ = pad_notices.send(Event::Notice(text));
        },
    );

    let datagram_conn = conn.clone();
    let datagram_shared = shared.clone();
    let datagram_pads = pad_tx.clone();
    let datagram_task = tokio::spawn(async move {
        while let Ok(bytes) = datagram_conn.read_datagram().await {
            match oriel_proto::decode(&bytes) {
                Ok(Datagram::FrameAck { seq, .. }) => datagram_shared.decoded(seq),
                Ok(Datagram::Gamepad { pad, seq, state }) => {
                    let _ = datagram_pads.send(crate::gamepad::Cmd::State { pad, seq, state });
                }
                _ => {}
            }
        }
    });

    let rtt_conn = conn.clone();
    let rtt_shared = shared.clone();
    let rtt_task = tokio::spawn(async move {
        loop {
            rtt_shared
                .rtt_us
                .store(rtt_conn.rtt().as_micros() as u64, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    let result: Result<()> = async {
        loop {
            tokio::select! {
                event = ev_rx.recv() => match event {
                    None => break,
                    Some(Event::Frame(header, data)) => send_frame(&conn, header, data, &shared).await,
                    Some(Event::Surface(info)) => {
                        status.lock().set_codec(conn.stable_id(), info.codec.name());
                        write_msg(&mut ctl_send, &HostMsg::SurfaceStarted(info)).await?
                    }
                    Some(Event::CursorShape(shape)) => {
                        write_msg(&mut ctl_send, &HostMsg::CursorShape(shape)).await?
                    }
                    Some(Event::CursorState(state)) => {
                        write_msg(&mut ctl_send, &HostMsg::CursorState(state)).await?
                    }
                    Some(Event::CursorPos { x, y }) => {
                        let dg = Datagram::CursorPosition { surface: 1, x, y };
                        let _ = conn.send_datagram(oriel_proto::encode(&dg)?.into());
                    }
                    Some(Event::Rumble { pad, strong, weak }) => {
                        write_msg(&mut ctl_send, &HostMsg::Rumble { pad, strong, weak }).await?
                    }
                    Some(Event::Notice(text)) => {
                        write_msg(&mut ctl_send, &HostMsg::Notice { text }).await?
                    }
                    Some(Event::Clipboard(text)) => {
                        write_msg(&mut ctl_send, &HostMsg::Clipboard { text }).await?
                    }
                    Some(Event::Failed(text)) => {
                        write_msg(&mut ctl_send, &HostMsg::Notice { text }).await?;
                        break;
                    }
                },
                msg = ctl_rx.recv() => match msg {
                    None | Some(ClientMsg::Bye) => break,
                    Some(ClientMsg::RequestKeyframe) => { let _ = cmd_tx.send(Cmd::Keyframe); }
                    Some(ClientMsg::SelectDisplay { id }) => {
                        // A resolution change belongs to the previous display.
                        mode_guard = None;
                        display = Some(id);
                        let _ = cmd_tx.send(Cmd::Display(id));
                    }
                    Some(ClientMsg::SetCodecs { codecs, auto_codec }) => {
                        let _ = cmd_tx.send(Cmd::Codecs(codecs, auto_codec));
                    }
                    Some(ClientMsg::SetResolution { resolution }) => {
                        mode_guard = None;
                        if let Some(res) = resolution {
                            match crate::capture::set_display_mode(options.source, display, res) {
                                Ok(g) => mode_guard = Some(g),
                                Err(e) => write_msg(&mut ctl_send, &HostMsg::Notice {
                                    text: format!("Cannot change the resolution: {e:#}"),
                                }).await?,
                            }
                        }
                    }
                    Some(ClientMsg::SetBitrate { kbps }) => { let _ = cmd_tx.send(Cmd::Bitrate(kbps)); }
                    Some(ClientMsg::GamepadRemoved { pad }) => {
                        let _ = pad_tx.send(crate::gamepad::Cmd::Remove { pad });
                    }
                    Some(ClientMsg::SetAudioDevice { id }) => {
                        if settings.audio {
                            // Stop the old capture before opening the new one.
                            drop(audio.take());
                            audio = start_audio(&conn, options.source, id, audio_seq.clone(), &ev_tx_audio);
                        }
                    }
                    Some(ClientMsg::Ping { id }) => {
                        write_msg(&mut ctl_send, &HostMsg::Pong { id, host_us: crate::host_us() }).await?
                    }
                    Some(ClientMsg::Clipboard { text }) => {
                        if text.len() <= crate::clipboard::MAX_TEXT {
                            let _ = clipboard.set.send(text);
                        }
                    }
                    Some(other) => tracing::debug!(?other, "ignoring message"),
                },
                _ = conn.closed() => break,
            }
        }
        Ok(())
    }
    .await;

    let _ = cmd_tx.send(Cmd::Stop);
    drop(mode_guard);
    drop(ev_rx);
    input_task.abort();
    ctl_task.abort();
    datagram_task.abort();
    // Unplug the virtual pads (the datagram task held the other sender).
    drop(pad_tx);
    tokio::task::spawn_blocking(move || {
        let _ = gamepads.join();
    });
    rtt_task.abort();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = pipeline.join();
        let _ = injector.join();
    })
    .await;
    tracing::info!("session ended");
    result
}

/// Open the frame's stream now (stream IDs must follow frame order), and
/// write it in the background.
async fn send_frame(
    conn: &quinn::Connection,
    header: FrameHeader,
    data: Vec<u8>,
    shared: &Arc<Shared>,
) {
    let mut stream = match conn.open_uni().await {
        Ok(s) => s,
        Err(_) => {
            shared.delivered(header.seq);
            return;
        }
    };
    // Older frames first: the client decodes in order.
    let _ = stream.set_priority(i32::MAX - (header.seq % i32::MAX as u64) as i32);
    let shared = shared.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            stream
                .write_all(&oriel_proto::encode_framed(&header)?)
                .await?;
            stream.write_all(&data).await?;
            stream.finish()?;
            stream.stopped().await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(seq = header.seq, "frame send failed: {e:#}");
        }
        shared.delivered(header.seq);
    });
}

fn start_audio(
    conn: &quinn::Connection,
    source: crate::capture::SourceKind,
    device: Option<String>,
    seq: Arc<std::sync::atomic::AtomicU32>,
    events: &tmpsc::UnboundedSender<Event>,
) -> Option<oriel_audio::Capture> {
    use crate::capture::SourceKind;
    let source = match source {
        SourceKind::TestPattern => oriel_audio::CaptureSource::TestTone,
        SourceKind::Auto if cfg!(any(windows, target_os = "linux")) => {
            oriel_audio::CaptureSource::SystemOutput(device)
        }
        SourceKind::Auto => {
            let _ = events.send(Event::Notice(
                "Audio capture is not supported on this host yet".into(),
            ));
            return None;
        }
    };
    let conn = conn.clone();
    match oriel_audio::start_capture(source, 128_000, move |data| {
        let seq = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(bytes) = oriel_proto::encode(&Datagram::Audio { seq, data }) {
            let _ = conn.send_datagram(bytes.into());
        }
    }) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("audio capture unavailable: {e:#}");
            let _ = events.send(Event::Notice(format!("No audio: {e:#}")));
            None
        }
    }
}
