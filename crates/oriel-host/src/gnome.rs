//! GNOME (Mutter) remote desktop session over D-Bus: a screen cast of a
//! monitor plus input injection, without a permission dialog. This is the
//! interface gnome-remote-desktop uses. One session is shared by the
//! capturer and the injector and stopped when neither uses it any more.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use oriel_proto::DisplayInfo;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use zbus::{
    Connection, proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::capture::Geometry;

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop",
    default_service = "org.gnome.Mutter.RemoteDesktop",
    default_path = "/org/gnome/Mutter/RemoteDesktop"
)]
trait RemoteDesktop {
    fn create_session(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop.Session",
    default_service = "org.gnome.Mutter.RemoteDesktop"
)]
trait RemoteDesktopSession {
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn notify_keyboard_keycode(&self, keycode: u32, state: bool) -> zbus::Result<()>;
    fn notify_pointer_motion_relative(&self, dx: f64, dy: f64) -> zbus::Result<()>;
    fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_pointer_button(&self, button: i32, state: bool) -> zbus::Result<()>;
    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> zbus::Result<()>;
    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<String>;
    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait ScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>)
    -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastSession {
    fn record_monitor(
        &self,
        connector: &str,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastStream {
    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn parameters(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

type MonitorSpec = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        HashMap<String, OwnedValue>,
    )>;
}

/// Commands for the input task (ordered, fire and forget).
pub enum InputCmd {
    Key(u32, bool),
    MoveAbsolute(f64, f64),
    MoveRelative(f64, f64),
    Button(i32, bool),
    Scroll { axis: u32, steps: i32 },
}

pub struct GnomeSession {
    rt: tokio::runtime::Runtime,
    rd: RemoteDesktopSessionProxy<'static>,
    pub node_id: u32,
    pub geometry: Geometry,
    #[allow(dead_code)]
    pub connector: String,
    input: mpsc::UnboundedSender<InputCmd>,
}

impl GnomeSession {
    /// Whether this looks like a GNOME session with Mutter's interfaces.
    pub fn available() -> bool {
        std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"))
            || std::env::var_os("ORIEL_FORCE_GNOME").is_some()
    }

    /// The shared session for `display`, created on first use.
    pub fn shared(display: Option<u32>) -> Result<Arc<Self>> {
        static CURRENT: Mutex<Option<(Option<u32>, Weak<GnomeSession>)>> = Mutex::new(None);
        let mut current = CURRENT.lock();
        if let Some((d, weak)) = current.as_ref()
            && (display.is_none() || *d == display)
            && let Some(s) = weak.upgrade()
        {
            return Ok(s);
        }
        let session = Arc::new(Self::start(display)?);
        *current = Some((display, Arc::downgrade(&session)));
        Ok(session)
    }

    fn start(display: Option<u32>) -> Result<Self> {
        // A worker thread, so the input task runs on its own.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("oriel-gnome")
            .enable_all()
            .build()?;
        let (rd, node_id, geometry, connector, rd_for_input) = rt.block_on(async {
            let conn = Connection::session().await.context("connect to the session bus")?;
            let monitors = monitors(&conn).await?;
            let monitor = match display {
                Some(i) => monitors.get(i as usize).cloned().with_context(|| format!("no display {i}"))?,
                None => monitors.first().cloned().context("no monitors")?,
            };
            let rd = RemoteDesktopProxy::new(&conn).await?;
            let rd_path = rd
                .create_session()
                .await
                .context("create a Mutter remote desktop session")?;
            let rd_session = RemoteDesktopSessionProxy::builder(&conn).path(rd_path)?.build().await?;
            let session_id = rd_session.session_id().await?;
            let sc = ScreenCastProxy::new(&conn).await?;
            let sc_path = sc
                .create_session(HashMap::from([("remote-desktop-session-id", Value::from(session_id.as_str()))]))
                .await
                .context("create a Mutter screen cast session")?;
            let sc_session = ScreenCastSessionProxy::builder(&conn).path(sc_path)?.build().await?;
            // cursor-mode 2: the cursor travels as stream metadata.
            let stream_path = sc_session
                .record_monitor(&monitor.connector, HashMap::from([("cursor-mode", Value::U32(2))]))
                .await
                .with_context(|| format!("record monitor {}", monitor.connector))?;
            let stream = ScreenCastStreamProxy::builder(&conn)
                .path(stream_path.clone())?
                .build()
                .await?;
            let mut added = stream.receive_pipe_wire_stream_added().await?;
            rd_session.start().await.context("start the remote desktop session")?;
            let node_id = tokio::time::timeout(Duration::from_secs(5), added.next())
                .await
                .context("Mutter did not start the screen cast")?
                .context("screen cast stream closed")?
                .args()?
                .node_id;
            let params = stream.parameters().await.unwrap_or_default();
            let (left, top) = pair(&params, "position").unwrap_or((monitor.x, monitor.y));
            let (width, height) = pair(&params, "size").unwrap_or((monitor.width as i32, monitor.height as i32));
            tracing::info!(connector = %monitor.connector, node_id, width, height, "GNOME screen cast started");
            let geometry = Geometry {
                left,
                top,
                width: width as u32,
                height: height as u32,
            };
            anyhow::Ok((
                rd_session.clone(),
                node_id,
                geometry,
                monitor.connector.clone(),
                (rd_session, stream_path.to_string()),
            ))
        })?;

        let (input, mut input_rx) = mpsc::unbounded_channel::<InputCmd>();
        let (rd_input, stream_path) = rd_for_input;
        rt.spawn(async move {
            while let Some(cmd) = input_rx.recv().await {
                let result = match cmd {
                    InputCmd::Key(code, down) => rd_input.notify_keyboard_keycode(code, down).await,
                    InputCmd::MoveAbsolute(x, y) => {
                        rd_input
                            .notify_pointer_motion_absolute(&stream_path, x, y)
                            .await
                    }
                    InputCmd::MoveRelative(dx, dy) => {
                        rd_input.notify_pointer_motion_relative(dx, dy).await
                    }
                    InputCmd::Button(button, down) => {
                        rd_input.notify_pointer_button(button, down).await
                    }
                    InputCmd::Scroll { axis, steps } => {
                        rd_input.notify_pointer_axis_discrete(axis, steps).await
                    }
                };
                if let Err(e) = result {
                    tracing::debug!("input injection failed: {e}");
                }
            }
        });
        Ok(Self {
            rt,
            rd,
            node_id,
            geometry,
            connector,
            input,
        })
    }

    pub fn input(&self, cmd: InputCmd) {
        let _ = self.input.send(cmd);
    }
}

impl Drop for GnomeSession {
    fn drop(&mut self) {
        let rd = self.rd.clone();
        let _ = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), rd.stop()).await });
        tracing::info!("GNOME remote desktop session stopped");
    }
}

fn pair(params: &HashMap<String, OwnedValue>, key: &str) -> Option<(i32, i32)> {
    let v: (i32, i32) = params.get(key)?.try_clone().ok()?.try_into().ok()?;
    Some(v)
}

#[derive(Debug, Clone)]
struct MonitorInfo {
    connector: String,
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    refresh: f64,
    primary: bool,
}

/// Monitors in use, primary first.
async fn monitors(conn: &Connection) -> Result<Vec<MonitorInfo>> {
    let config = DisplayConfigProxy::new(conn).await?;
    let (_, monitors, logical, _) = config
        .get_current_state()
        .await
        .context("read the display configuration from Mutter")?;
    let mut out = Vec::new();
    for (x, y, _scale, _transform, primary, specs, _) in logical {
        for spec in specs {
            let Some((_, modes, props)) = monitors.iter().find(|(s, ..)| *s == spec) else {
                continue;
            };
            let current = modes.iter().find(|m| {
                m.6.get("is-current")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false)
            });
            let Some(mode) = current else { continue };
            let name = props
                .get("display-name")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                .unwrap_or_else(|| spec.0.clone());
            out.push(MonitorInfo {
                connector: spec.0.clone(),
                name,
                x,
                y,
                width: mode.1 as u32,
                height: mode.2 as u32,
                refresh: mode.3,
                primary,
            });
        }
    }
    out.sort_by_key(|m| !m.primary);
    if out.is_empty() {
        bail!("Mutter reports no active monitors");
    }
    Ok(out)
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    // Callers may be inside a tokio runtime; use our own on a fresh thread.
    std::thread::spawn(list_displays_blocking)
        .join()
        .map_err(|_| anyhow!("display listing panicked"))?
}

fn list_displays_blocking() -> Result<Vec<DisplayInfo>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let conn = Connection::session().await?;
        Ok(monitors(&conn)
            .await?
            .into_iter()
            .enumerate()
            .map(|(i, m)| DisplayInfo {
                id: i as u32,
                name: m.name,
                width: m.width,
                height: m.height,
                refresh_hz: m.refresh.round() as u32,
                primary: m.primary,
            })
            .collect())
    })
    .map_err(|e: anyhow::Error| anyhow!("{e:#}"))
}
