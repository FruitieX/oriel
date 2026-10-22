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
        },
        sink,
    );
    let start = Instant::now();
    let mut frames = 0u64;
    let mut last_report = Instant::now();
    let mut wiggle = 0i32;
    let still = std::env::var_os("ORIEL_HEADLESS_STILL").is_some();
    let fake_pad = std::env::var_os("ORIEL_DEBUG_GAMEPAD").is_some();
    let mut pad_seq = 0u32;
    // Debugging aid: press this HID usage (hex) once, two seconds in.
    let mut press: Option<u16> = std::env::var("ORIEL_DEBUG_PRESS")
        .ok()
        .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let mut latency_ms = 0f32;
    while start.elapsed() < duration {
        let ev = match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => Some(ev),
            Ok(None) => break,
            Err(_) => None,
        };
        match ev {
            Some(UiEvent::FrameReady) => {
                if let Some(ready) = s.frames.take() {
                    frames += 1;
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
            if let Some(a) = &s.audio {
                let a = *a.lock();
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
