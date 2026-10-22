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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: session::Sink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });
    let s = session::start(
        &tokio::runtime::Handle::current(),
        connected.conn.clone(),
        connected.ctl,
        settings,
        hardware,
        sink,
    );
    let start = Instant::now();
    let mut frames = 0u64;
    let mut last_report = Instant::now();
    let mut wiggle = 0i32;
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
        // Exercise the input path: the test pattern draws the pointer.
        wiggle += 1;
        s.input(InputEvent::MouseMove {
            surface: 1,
            x: 400 + (wiggle % 200),
            y: 300,
        });
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
