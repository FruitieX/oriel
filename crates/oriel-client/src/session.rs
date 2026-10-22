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
    CursorPosition { x: i32, y: i32 },
    FrameReady,
    Closed(String),
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
    rt: tokio::runtime::Handle,
}

impl Session {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.ctl.send(msg);
    }

    pub fn input(&self, event: InputEvent) {
        let _ = self.input.send(event);
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

pub fn start(
    rt: &tokio::runtime::Handle,
    conn: quinn::Connection,
    ctl: ControlStream,
    settings: StreamSettings,
    hardware_decode: bool,
    sink: Sink,
) -> Session {
    let _guard = rt.enter();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let stats = Arc::new(Mutex::new(Stats::default()));
    let frames = decode::FrameSlot::default();
    let decoder = decode::spawn(
        conn.clone(),
        ctl_tx.clone(),
        frames.clone(),
        stats.clone(),
        hardware_decode,
        sink.clone(),
    );

    let ControlStream { mut send, mut recv } = ctl;
    let _ = ctl_tx.send(ClientMsg::StartStream(settings));
    tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    let reader_sink = sink.clone();
    tokio::spawn(async move {
        loop {
            match read_msg::<HostMsg>(&mut recv).await {
                Ok(Some(msg)) => reader_sink(UiEvent::Host(msg)),
                Ok(None) | Err(_) => break,
            }
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
    tokio::spawn(async move {
        let result: Result<()> = async {
            loop {
                let mut stream = frame_conn.accept_uni().await?;
                let Some(header) = read_msg::<FrameHeader>(&mut stream).await? else {
                    continue;
                };
                let data = stream.read_to_end(256 * 1024 * 1024).await?;
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
            if let Ok(Datagram::CursorPosition { x, y, .. }) = oriel_proto::decode(&bytes) {
                dg_sink(UiEvent::CursorPosition { x, y });
            }
        }
    });

    let closed_conn = conn.clone();
    tokio::spawn(async move {
        let reason = closed_conn.closed().await;
        let text = match reason {
            quinn::ConnectionError::ApplicationClosed(c) => {
                String::from_utf8_lossy(&c.reason).into_owned()
            }
            quinn::ConnectionError::LocallyClosed => "disconnected".into(),
            quinn::ConnectionError::TimedOut => "connection timed out".into(),
            other => other.to_string(),
        };
        sink(UiEvent::Closed(text));
    });

    Session {
        conn,
        ctl: ctl_tx,
        input: input_tx,
        frames,
        stats,
        rt: rt.clone(),
    }
}
