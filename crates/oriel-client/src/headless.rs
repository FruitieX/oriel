//! Stream without a window: decode and report statistics. For testing and
//! benchmarking.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use oriel_proto::{HostMsg, InputEvent, StreamSettings};

use crate::{
    connection,
    session::{self, UiEvent},
};

pub async fn run(
    paths: &oriel_net::Paths,
    target: &str,
    settings: StreamSettings,
    hardware: bool,
    duration: Duration,
) -> Result<()> {
    let connected = connection::connect(paths, target).await?;
    println!(
        "connected to {} ({} displays)",
        connected.host_name,
        connected.displays.len()
    );
    for d in &connected.audio_devices {
        println!("host sound output: {} ({})", d.name, d.id);
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: session::Sink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });
    let s = session::start(
        &tokio::runtime::Handle::current(),
        connected.conn.clone(),
        connected.ctl,
        session::SessionOptions {
            settings,
            hardware_decode: hardware,
            playback_device: None,
            gamepads: false,
            microphone: false,
            microphone_device: None,
        },
        sink,
    );
    // Automatic bitrate, as in the window (ORIEL_HEADLESS_FIXED_BITRATE
    // keeps the chosen one).
    s.send(oriel_proto::ClientMsg::SetAutoBitrate {
        enabled: std::env::var_os("ORIEL_HEADLESS_FIXED_BITRATE").is_none(),
    });
    let start = Instant::now();
    let mut frames = 0u64;
    let mut last_report = Instant::now();
    let mut wiggle = 0i32;
    let still = std::env::var_os("ORIEL_HEADLESS_STILL").is_some();
    let fake_pad = std::env::var_os("ORIEL_DEBUG_GAMEPAD").is_some();
    // ORIEL_DEBUG_MIC: send the test tone as the microphone.
    let _mic = std::env::var_os("ORIEL_DEBUG_MIC").map(|_| {
        let conn = s.conn.clone();
        let mut seq = 0u32;
        oriel_audio::start_capture(oriel_audio::CaptureSource::TestTone, 64_000, move |data| {
            if let Ok(bytes) = oriel_proto::encode(&oriel_proto::Datagram::Microphone { seq, data })
            {
                let _ = conn.send_datagram(bytes.into());
            }
            seq = seq.wrapping_add(1);
        })
    });
    let mut pad_seq = 0u32;
    // Debugging aid: press this HID usage (hex) once, two seconds in.
    let mut press: Option<u16> = std::env::var("ORIEL_DEBUG_PRESS")
        .ok()
        .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let mut latency_ms = 0f32;
    let trace_frames = std::env::var_os("ORIEL_DEBUG_FRAMES").is_some();
    while start.elapsed() < duration {
        let ev = match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => Some(ev),
            Ok(None) => break,
            Err(_) => None,
        };
        match ev {
            Some(UiEvent::FrameReady) => {
                if let Some(ready) = s.frames.take(Instant::now()) {
                    frames += 1;
                    // Debugging aid: ORIEL_DEBUG_FRAMES=1 prints each
                    // frame's size and mean brightness (black frames).
                    if trace_frames && let crate::decode::Picture::Cpu(f) = &ready.picture {
                        println!(
                            "frame {} {}x{} luma {:.0} at {:.2} s",
                            ready.header.seq,
                            f.width(),
                            f.height(),
                            mean_luma(f),
                            start.elapsed().as_secs_f32()
                        );
                    }
                    if let Some(l) = s
                        .clock
                        .lock()
                        .since_capture(ready.header.capture_us, Instant::now())
                    {
                        latency_ms = latency_ms * 0.9 + l.as_secs_f32() * 1000.0 * 0.1;
                    }
                }
            }
            Some(UiEvent::Host(HostMsg::SurfaceStarted(info))) => println!("surface: {info:?}"),
            Some(UiEvent::Host(HostMsg::Notice { text })) => println!("host: {text}"),
            Some(UiEvent::Host(HostMsg::Health(h))) => println!(
                "host: sent {} of {} fps, target {:.1} Mbit/s, loss {:.1}%{}",
                h.sent_fps,
                h.wanted_fps,
                h.bitrate_kbps as f32 / 1000.0,
                h.loss_permille as f32 / 10.0,
                if h.network_limited {
                    ", network-limited"
                } else {
                    ""
                }
            ),
            Some(UiEvent::Host(HostMsg::CursorShape(c))) => {
                let opaque = c.rgba.chunks(4).filter(|p| p[3] > 0).count();
                println!(
                    "cursor shape {}: {}x{} hotspot {},{}, {opaque} visible pixels",
                    c.id, c.width, c.height, c.hotspot_x, c.hotspot_y
                );
            }
            Some(UiEvent::Host(HostMsg::CursorState(c))) => println!("cursor state: {c:?}"),
            Some(UiEvent::Closed { reason, .. }) => bail!("session closed: {reason}"),
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(2)
            && let Some(usage) = press.take()
        {
            println!("pressing key {usage:#x}");
            s.input(InputEvent::Key { usage, down: true });
            s.input(InputEvent::Key { usage, down: false });
        }
        // Exercise the input path: the test pattern draws the pointer.
        // ORIEL_HEADLESS_STILL keeps the host's mouse still (on a real
        // desktop, hover effects would keep the screen changing).
        // ORIEL_DEBUG_GAMEPAD: a synthetic controller pressing A on and
        // off every second, to test the host's virtual pads.
        if fake_pad {
            let state = oriel_proto::GamepadState {
                buttons: if start.elapsed().as_secs().is_multiple_of(2) {
                    oriel_proto::gamepad::A
                } else {
                    0
                },
                ..Default::default()
            };
            pad_seq += 1;
            let dg = oriel_proto::Datagram::Gamepad {
                pad: 0,
                seq: pad_seq,
                state,
            };
            if let Ok(bytes) = oriel_proto::encode(&dg) {
                let _ = s.conn.send_datagram(bytes.into());
            }
        }
        if !still {
            wiggle += 1;
            s.input(InputEvent::MouseMove {
                surface: 1,
                x: 400 + (wiggle % 200),
                y: 300,
            });
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let summary = s.stats.lock().summary();
            let st = s.stats.lock();
            println!(
                "{} {}x{} via {}: {:.0} fps, {:.1} Mbit/s, encode {:.2} ms, decode {:.2} ms, rtt {:.2} ms, capture→decoded {:.2} ms, presented {frames}",
                st.codec,
                st.resolution.0,
                st.resolution.1,
                st.decoder,
                summary.fps,
                summary.mbps,
                summary.encode_ms,
                summary.decode_ms,
                s.rtt().as_secs_f32() * 1000.0,
                latency_ms
            );
            if let Some(a) = s.audio_stats() {
                println!(
                    "audio: buffered {:.1} ms, lost {}, underruns {}",
                    a.buffered_ms, a.lost, a.underruns
                );
            }
            last_report = Instant::now();
        }
    }
    s.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    if frames == 0 {
        bail!("no frames were decoded");
    }
    Ok(())
}

/// Mean of a sparse grid of luma samples.
fn mean_luma(frame: &oriel_media::DecodedFrame) -> f32 {
    let (y, stride) = frame.plane(0);
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let mut sum = 0u64;
    let mut n = 0u64;
    for row in (0..h).step_by(h.div_ceil(32).max(1)) {
        for col in (0..w).step_by(w.div_ceil(32).max(1)) {
            sum += y[row * stride + col] as u64;
            n += 1;
        }
    }
    sum as f32 / n.max(1) as f32
}
