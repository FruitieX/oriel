//! One streaming session with an authenticated client.

use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    time::Duration,
};

use anyhow::{Context, Result};
use oriel_net::{ControlStream, read_frame, read_known, write_msg};
use oriel_proto::{
    ClientMsg, Datagram, FrameHeader, HostMsg, InputEvent, PROTOCOL_VERSION, START_STREAM_TAG,
    VirtualMode,
};
use tokio::sync::mpsc as tmpsc;

use crate::{
    capture::{Target, TestState},
    input,
    pipeline::{self, Cmd, Event, Shared},
    server::HostOptions,
};

pub async fn run(
    conn: quinn::Connection,
    mut ctl: ControlStream,
    client_protocol: u32,
    options: Arc<HostOptions>,
    test: TestState,
    status: Arc<parking_lot::Mutex<crate::server::HostStatus>>,
) -> Result<()> {
    let settings = loop {
        let bytes = read_frame(&mut ctl.recv)
            .await?
            .context("control stream closed by peer")?;
        let msg = match oriel_proto::decode::<ClientMsg>(&bytes) {
            Ok(msg) => msg,
            // Without stream settings there is nothing to stream.
            Err(e) if bytes.first() == Some(&START_STREAM_TAG) => {
                tracing::warn!("cannot read the client's stream settings: {e}");
                ctl.send(&HostMsg::Rejected {
                    reason: format!(
                        "the host runs another version of oriel (protocol {PROTOCOL_VERSION}, \
                         this client {client_protocol}) and cannot read this client's \
                         settings; update both"
                    ),
                })
                .await?;
                return Ok(());
            }
            Err(e) => {
                tracing::debug!("skipping a message: {e}");
                continue;
            }
        };
        match msg {
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

    // A virtual monitor replaces the chosen display while set.
    let mut virtual_mode = settings.virtual_display;
    // Switched to a virtual display because no monitor showed anything.
    let mut auto_virtual = false;
    if virtual_mode.is_some() && !crate::capture::virtual_display_available(options.source) {
        virtual_mode = None;
        ctl.send(&HostMsg::Notice {
            text: "This host cannot create virtual displays".into(),
        })
        .await?;
    }
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
            target: match virtual_mode {
                Some(mode) => Target::Virtual(mode),
                None => Target::Display(settings.display),
            },
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
            full_chroma: settings.full_chroma.clone(),
            software_encode: settings.software_encode,
            graphics_capture: settings.graphics_capture,
            pyrowave_kbps: settings.pyrowave_kbps.max(10_000),
            // The client turns it on with SetAutoBitrate right after.
            auto_bitrate: false,
            rate_sensitivity: Default::default(),
        },
        shared.clone(),
        test.clone(),
        cmd_rx,
        ev_tx,
    );

    let clip_events = ev_tx_audio.clone();
    let clipboard = oriel_clipboard::start(
        oriel_clipboard::Options {
            text: true,
            active: None,
        },
        move |content| {
            let _ = clip_events.send(Event::Clipboard(content));
        },
    );

    // The display mode chosen by the client, given up when dropped.
    let mut mode_guard: Option<crate::modes::ModeClaim> = None;
    if let Some(res) = settings.resolution.filter(|_| virtual_mode.is_none()) {
        mode_guard = set_mode(options.source, settings.display, res, &ev_tx_audio);
    }
    let mut display = settings.display;
    // The client's resolution, applied to whichever display is streamed.
    let mut resolution = settings.resolution;

    // Continues across device switches, so the client's jitter buffer
    // does not mistake the new packets for stale ones.
    let audio_seq = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut audio_enabled = settings.audio;
    let mut audio_device = settings.audio_device.clone();
    // Opus bitrate; clients set it with `SetAudioBitrate`.
    let mut audio_kbps = 128u32;
    // The host's speakers, muted for the client (`SetMuteHost`).
    let mut mute: Option<oriel_audio::HostMute> = None;
    let mut audio = if settings.audio {
        start_audio(
            &conn,
            options.source,
            settings.audio_device.clone(),
            audio_kbps,
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
            loop {
                let event = match injector.next_tick() {
                    Some(at) => match input_rx
                        .recv_timeout(at.saturating_duration_since(std::time::Instant::now()))
                    {
                        Ok(event) => event,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            injector.tick();
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                    None => match input_rx.recv() {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                };
                let geometry = *injector_shared.geometry.lock();
                if let Err(e) = injector.inject(event, geometry) {
                    tracing::debug!("inject failed: {e:#}");
                }
            }
            let geometry = *injector_shared.geometry.lock();
            let _ = injector.inject(InputEvent::ReleaseAll, geometry);
        })?;

    let input_conn = conn.clone();
    let input_task = tokio::spawn(async move {
        let mut stream = input_conn.accept_uni().await?;
        while let Some(event) = read_known::<InputEvent>(&mut stream).await? {
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
        while let Some(msg) = read_known::<ClientMsg>(&mut ctl_recv).await? {
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
    let mic_notices = ev_tx_audio.clone();
    let datagram_task = tokio::spawn(async move {
        // The client's microphone, set up when its first packet arrives.
        let mut mic: Option<oriel_audio::Player> = None;
        let mut mic_failed = false;
        while let Ok(bytes) = datagram_conn.read_datagram().await {
            match oriel_proto::decode(&bytes) {
                Ok(Datagram::Microphone { seq, data }) => {
                    if mic.is_none() && !mic_failed {
                        match oriel_audio::microphone_output().and_then(oriel_audio::Player::start)
                        {
                            Ok(p) => mic = Some(p),
                            Err(e) => {
                                mic_failed = true;
                                tracing::warn!("cannot play the client's microphone: {e:#}");
                                let _ = mic_notices
                                    .send(Event::Notice(format!("Microphone unavailable: {e:#}")));
                            }
                        }
                    }
                    if let Some(p) = &mic {
                        p.push(seq, data);
                    }
                }
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
        let mut stalled = false;
        loop {
            // Frames waiting on the client for long: log what QUIC sees.
            match rtt_shared.blocked() {
                Some((held, frames)) if held > Duration::from_secs(2) && !stalled => {
                    stalled = true;
                    let s = rtt_conn.stats();
                    tracing::warn!(
                        "frames held back {:.1} s waiting for the client: {frames}; \
                         rtt {:?}, cwnd {}, sent {} lost {} packets, congestion events {}, \
                         black holes {}",
                        held.as_secs_f32(),
                        s.path.rtt,
                        s.path.cwnd,
                        s.path.sent_packets,
                        s.path.lost_packets,
                        s.path.congestion_events,
                        s.path.black_holes_detected,
                    );
                }
                None if stalled => {
                    stalled = false;
                    tracing::warn!("frames moving to the client again");
                }
                _ => {}
            }
            rtt_shared
                .rtt_us
                .store(rtt_conn.rtt().as_micros() as u64, Ordering::Relaxed);
            let path = rtt_conn.stats().path;
            rtt_shared
                .sent_packets
                .store(path.sent_packets, Ordering::Relaxed);
            rtt_shared
                .lost_packets
                .store(path.lost_packets, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    // Packets sent and lost so far, for the loss in each health report.
    let mut packets = (0u64, 0u64);
    // Windows resets a monitor's mode when it sleeps or reconnects.
    let mut mode_check = tokio::time::interval(Duration::from_secs(2));
    mode_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = mode_check.tick() => {
                    if mode_guard.as_ref().is_some_and(|g| g.reapply()) {
                        tracing::info!("display mode set again");
                    }
                }
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
                    // Nothing to see (monitors off or on another input):
                    // stream a virtual display instead, once per session.
                    Some(Event::NoPicture) => {
                        // Windows only for now: GNOME's first frame can
                        // take a moment to arrive.
                        if cfg!(windows)
                            && virtual_mode.is_none()
                            && !auto_virtual
                            && crate::capture::virtual_display_available(options.source)
                        {
                            auto_virtual = true;
                            let (width, height) = resolution.unwrap_or((1920, 1080));
                            let mode = VirtualMode { width, height, refresh_hz: 60 };
                            tracing::info!(?mode, "no monitor shows a picture; using a virtual display");
                            mode_guard = None;
                            virtual_mode = Some(mode);
                            let _ = cmd_tx.send(Cmd::Target(Target::Virtual(mode)));
                            write_msg(&mut ctl_send, &HostMsg::Notice {
                                text: "No monitor shows this computer's screen (switched off or on \
                                       another input), so it streams an extra display instead"
                                    .into(),
                            }).await?;
                        }
                    }
                    Some(Event::Health(mut health)) => {
                        let path = conn.stats().path;
                        let (sent, lost) = (
                            path.sent_packets - packets.0,
                            path.lost_packets - packets.1,
                        );
                        packets = (path.sent_packets, path.lost_packets);
                        health.loss_permille = (lost * 1000 / sent.max(1)).min(1000) as u16;
                        write_msg(&mut ctl_send, &HostMsg::Health(health)).await?
                    }
                    Some(Event::Clipboard(oriel_clipboard::Content::Text(text))) => {
                        write_msg(&mut ctl_send, &HostMsg::Clipboard { text }).await?
                    }
                    Some(Event::Clipboard(oriel_clipboard::Content::Image(png))) => {
                        write_msg(&mut ctl_send, &HostMsg::ClipboardImage { png }).await?
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
                        // The previous display gets its own mode back; the
                        // new one gets the client's resolution, before
                        // capture starts on it.
                        mode_guard = None;
                        display = Some(id);
                        virtual_mode = None;
                        if let Some(res) = resolution {
                            mode_guard = set_mode(options.source, display, res, &ev_tx_audio);
                        }
                        let _ = cmd_tx.send(Cmd::Target(Target::Display(display)));
                    }
                    Some(ClientMsg::SetVirtualDisplay { mode }) => {
                        if mode.is_some() && !crate::capture::virtual_display_available(options.source) {
                            write_msg(&mut ctl_send, &HostMsg::Notice {
                                text: "This host cannot create virtual displays".into(),
                            }).await?;
                            continue;
                        }
                        // The physical display goes back to its own mode,
                        // or to the client's when it is streamed again.
                        mode_guard = None;
                        virtual_mode = mode;
                        if let (None, Some(res)) = (mode, resolution) {
                            mode_guard = set_mode(options.source, display, res, &ev_tx_audio);
                        }
                        let target = match mode {
                            Some(m) => Target::Virtual(m),
                            None => Target::Display(display),
                        };
                        let _ = cmd_tx.send(Cmd::Target(target));
                    }
                    Some(ClientMsg::SetCodecs {
                        codecs,
                        auto_codec,
                        full_chroma,
                        software_encode,
                    }) => {
                        let _ = cmd_tx.send(Cmd::Codecs(
                            codecs,
                            auto_codec,
                            full_chroma,
                            software_encode,
                        ));
                    }
                    Some(ClientMsg::SetResolution { resolution }) if virtual_mode.is_some() => {
                        // Resize the virtual monitor (None: leave it).
                        if let (Some(m), Some((width, height))) = (virtual_mode, resolution) {
                            let mode = VirtualMode { width, height, ..m };
                            virtual_mode = Some(mode);
                            let _ = cmd_tx.send(Cmd::Target(Target::Virtual(mode)));
                        }
                    }
                    Some(ClientMsg::SetResolution { resolution: res }) => {
                        resolution = res;
                        // The new mode replaces the old directly: giving up
                        // the old claim first would switch to the display's
                        // own mode and back.
                        mode_guard = match res {
                            Some(res) => set_mode(options.source, display, res, &ev_tx_audio),
                            None => None,
                        };
                    }
                    Some(ClientMsg::SetBitrate { kbps }) => { let _ = cmd_tx.send(Cmd::Bitrate(kbps)); }
                    Some(ClientMsg::SetGraphicsCapture { enabled }) => {
                        let _ = cmd_tx.send(Cmd::GraphicsCapture(enabled));
                    }
                    Some(ClientMsg::SetAutoBitrate { enabled }) => { let _ = cmd_tx.send(Cmd::AutoBitrate(enabled)); }
                    Some(ClientMsg::SetRateSensitivity { sensitivity }) => {
                        let _ = cmd_tx.send(Cmd::RateSensitivity(sensitivity));
                    }
                    Some(ClientMsg::ClipboardImage { png }) => {
                        if png.len() <= oriel_clipboard::MAX_IMAGE {
                            let _ = clipboard.set.send(oriel_clipboard::Content::Image(png));
                        }
                    }
                    Some(ClientMsg::GamepadRemoved { pad }) => {
                        let _ = pad_tx.send(crate::gamepad::Cmd::Remove { pad });
                    }
                    Some(ClientMsg::SetAudioDevice { id }) => {
                        audio_device = id;
                        if audio_enabled {
                            // Stop the old capture before opening the new one.
                            drop(audio.take());
                            audio = start_audio(&conn, options.source, audio_device.clone(), audio_kbps, audio_seq.clone(), &ev_tx_audio);
                        }
                    }
                    Some(ClientMsg::SetAudio { enabled }) => {
                        audio_enabled = enabled;
                        drop(audio.take());
                        if enabled {
                            audio = start_audio(&conn, options.source, audio_device.clone(), audio_kbps, audio_seq.clone(), &ev_tx_audio);
                        }
                    }
                    Some(ClientMsg::SetAudioBitrate { kbps }) => {
                        // Opus' range.
                        let kbps = kbps.clamp(6, 510);
                        if kbps != audio_kbps {
                            audio_kbps = kbps;
                            if audio_enabled {
                                drop(audio.take());
                                audio = start_audio(&conn, options.source, audio_device.clone(), audio_kbps, audio_seq.clone(), &ev_tx_audio);
                            }
                        }
                    }
                    Some(ClientMsg::SetMuteHost { enabled }) => {
                        if !enabled {
                            mute = None;
                        } else if mute.is_none() {
                            match oriel_audio::mute_host_output() {
                                Ok(m) => mute = Some(m),
                                Err(e) => {
                                    tracing::warn!("cannot mute the host's speakers: {e:#}");
                                    let _ = ev_tx_audio.send(Event::Notice(format!("Cannot mute the host's speakers: {e:#}")));
                                }
                            }
                        }
                    }
                    Some(ClientMsg::SetMaxFps { fps }) => { let _ = cmd_tx.send(Cmd::MaxFps(fps)); }
                    Some(ClientMsg::SetPyroWaveBitrate { kbps }) => { let _ = cmd_tx.send(Cmd::PyroWaveBitrate(kbps)); }
                    Some(ClientMsg::Ping { id }) => {
                        write_msg(&mut ctl_send, &HostMsg::Pong { id, host_us: crate::host_us() }).await?
                    }
                    Some(ClientMsg::Clipboard { text }) => {
                        if text.len() <= oriel_clipboard::MAX_TEXT {
                            let _ = clipboard.set.send(oriel_clipboard::Content::Text(text));
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
    // Stop sound first: waiting for the pipeline below can take a while,
    // and a capture left running overlapped the next session's.
    drop(audio.take());
    let _ = mute.take();
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
/// Switch the display to the client's resolution (or the closest it
/// supports), telling the client when that is not what it asked for.
fn set_mode(
    source: crate::capture::SourceKind,
    display: Option<u32>,
    res: (u32, u32),
    events: &tmpsc::UnboundedSender<Event>,
) -> Option<crate::modes::ModeClaim> {
    match crate::modes::claim(source, display, res) {
        Ok(g) => {
            let (w, h) = g.size();
            if (w, h) != res {
                let _ = events.send(Event::Notice(format!(
                    "This display cannot show {}×{}; using {w}×{h}",
                    res.0, res.1
                )));
            }
            Some(g)
        }
        Err(e) => {
            let _ = events.send(Event::Notice(format!(
                "Cannot change the resolution: {e:#}"
            )));
            None
        }
    }
}

async fn send_frame(
    conn: &quinn::Connection,
    header: FrameHeader,
    data: Vec<u8>,
    shared: &Arc<Shared>,
) {
    let mut stream = match conn.open_uni().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(seq = header.seq, "cannot open a frame stream: {e}");
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
            shared.written(header.seq);
            stream.stopped().await?;
            Ok(())
        }
        .await;
        // Counted as delivered either way (the client will not decode it),
        // but say why: a stream that stops midway holds up the client.
        if let Err(e) = result {
            tracing::warn!(seq = header.seq, "frame send failed: {e:#}");
        }
        shared.delivered(header.seq);
    });
}

fn start_audio(
    conn: &quinn::Connection,
    source: crate::capture::SourceKind,
    device: Option<String>,
    kbps: u32,
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
    match oriel_audio::start_capture(source, kbps as i32 * 1000, move |data| {
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
