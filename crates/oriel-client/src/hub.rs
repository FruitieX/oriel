//! The hub account in the app: logging in (the device flow: show a code,
//! approve on any browser with a passkey), and keeping the account's
//! device list fresh, which adds its hosts to the known hosts (see
//! `oriel_net::hub`). Network calls run on the runtime, wake the UI when
//! they finish, and the UI polls.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use oriel_net::{Paths, hub};
use oriel_proto::hub::{Device, LoginStatus};
use parking_lot::Mutex;

/// What the Settings → App account section shows.
#[derive(Clone, Default)]
pub struct HubView {
    /// The hub's address (editable while logged out).
    pub url: String,
    /// Logged in as.
    pub user: Option<String>,
    pub login: Option<LoginView>,
    pub error: Option<String>,
    pub devices: Vec<Device>,
}

#[derive(Clone)]
pub struct LoginView {
    pub code: String,
    pub uri: String,
    /// The page with the code filled in.
    pub open: String,
}

pub enum HubAction {
    Login,
    Cancel,
    Logout,
    Remove(String),
    Wake(String),
    Open(String),
}

pub const DEFAULT_URL: &str = "https://oriel.fruitiex.org";
const SYNC_EVERY: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Shared {
    login: Option<LoginView>,
    error: Option<String>,
    user: Option<Option<String>>,
    devices: Option<Vec<Device>>,
    /// The known hosts changed.
    hosts_changed: bool,
}

pub struct HubState {
    pub view: HubView,
    paths: Paths,
    /// Where else the login goes: the Windows service's directory.
    extra: Option<PathBuf>,
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    synced: Option<Instant>,
    wake: Wake,
}

/// Asks the UI to poll again.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

impl HubState {
    pub fn new(paths: Paths, extra: Option<PathBuf>) -> Self {
        let account = hub::account(&paths);
        Self {
            view: HubView {
                url: account
                    .as_ref()
                    .map_or_else(|| DEFAULT_URL.to_string(), |a| a.url.clone()),
                user: account.as_ref().map(|a| a.user.clone()),
                devices: if account.is_some() {
                    hub::cached_devices(&paths)
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            paths,
            extra,
            shared: Default::default(),
            cancel: Default::default(),
            synced: None,
            wake: Arc::new(|| {}),
        }
    }

    pub fn set_wake(&mut self, wake: Wake) {
        self.wake = wake;
    }

    pub fn logged_in(&self) -> bool {
        self.view.user.is_some()
    }

    /// Take what the background work found. Returns whether the known
    /// hosts changed.
    pub fn poll(&mut self, rt: &tokio::runtime::Handle) -> bool {
        let mut s = self.shared.lock();
        if s.login.is_some() || self.view.login.is_some() {
            self.view.login = s.login.clone();
        }
        if let Some(e) = s.error.take() {
            self.view.error = Some(e);
        }
        if let Some(user) = s.user.take() {
            self.view.user = user;
            if self.view.user.is_none() {
                self.view.devices.clear();
            }
        }
        if let Some(d) = s.devices.take() {
            self.view.devices = d;
        }
        let changed = std::mem::take(&mut s.hosts_changed);
        drop(s);
        if self.logged_in() && self.synced.is_none_or(|t| t.elapsed() >= SYNC_EVERY) {
            self.sync(rt);
        }
        changed
    }

    /// Fetch the account's devices now.
    pub fn sync(&mut self, rt: &tokio::runtime::Handle) {
        self.synced = Some(Instant::now());
        let (paths, shared, extra) = (self.paths.clone(), self.shared.clone(), self.extra.clone());
        let wake = self.wake.clone();
        rt.spawn_blocking(move || {
            let Some(account) = hub::account(&paths) else {
                return;
            };
            match account.devices() {
                Ok(list) => {
                    let _ = hub::store_devices(&paths, &list);
                    let mut s = shared.lock();
                    s.devices = Some(list.devices);
                    s.hosts_changed = true;
                }
                Err(e) if e.is::<hub::Unauthorized>() => {
                    forget(&paths, extra.as_ref());
                    let mut s = shared.lock();
                    s.user = Some(None);
                    s.error = Some(format!("{e:#}"));
                }
                Err(e) => tracing::debug!("hub sync: {e:#}"),
            }
            wake();
        });
    }

    pub fn apply(&mut self, action: HubAction, rt: &tokio::runtime::Handle) {
        match action {
            HubAction::Login => self.login(rt),
            HubAction::Cancel => {
                self.cancel.store(true, Ordering::Relaxed);
                self.shared.lock().login = None;
                self.view.login = None;
            }
            HubAction::Logout => {
                forget(&self.paths, self.extra.as_ref());
                self.view.user = None;
                self.view.devices.clear();
                self.view.error = None;
            }
            HubAction::Remove(id) => self.call(rt, move |a| a.remove(&id)),
            HubAction::Wake(id) => self.call(rt, move |a| a.wake(&id)),
            HubAction::Open(url) => open_url(&url),
        }
    }

    fn call(
        &mut self,
        rt: &tokio::runtime::Handle,
        f: impl FnOnce(&hub::Account) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (paths, shared, wake) = (self.paths.clone(), self.shared.clone(), self.wake.clone());
        self.synced = None;
        rt.spawn_blocking(move || {
            if let Some(account) = hub::account(&paths)
                && let Err(e) = f(&account)
            {
                shared.lock().error = Some(format!("{e:#}"));
            }
            wake();
        });
    }

    fn login(&mut self, rt: &tokio::runtime::Handle) {
        let url = self.view.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return;
        }
        self.view.error = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        let (paths, shared, cancel, extra) = (
            self.paths.clone(),
            self.shared.clone(),
            self.cancel.clone(),
            self.extra.clone(),
        );
        let wake = self.wake.clone();
        rt.spawn(async move {
            let result = login(paths, url, shared.clone(), cancel, extra, wake.clone()).await;
            let mut s = shared.lock();
            s.login = None;
            match result {
                Ok(Some(user)) => {
                    s.user = Some(Some(user));
                    s.hosts_changed = true;
                }
                Ok(None) => {}
                Err(e) => s.error = Some(format!("{e:#}")),
            }
            drop(s);
            wake();
        });
    }
}

fn forget(paths: &Paths, extra: Option<&PathBuf>) {
    hub::forget_account(paths);
    if let Some(p) = extra {
        let _ = std::fs::remove_file(p);
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// Run the device flow; `None` when cancelled.
async fn login(
    paths: Paths,
    url: String,
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    extra: Option<PathBuf>,
    wake: Wake,
) -> anyhow::Result<Option<String>> {
    let fingerprint = oriel_net::Identity::load_or_create(&paths.config_dir)?.fingerprint;
    let name = crate::connection::device_name();
    let u = url.clone();
    let started = blocking(move || hub::login_start(&u, &fingerprint, &name)).await?;
    shared.lock().login = Some(LoginView {
        code: started.user_code.clone(),
        uri: started.verification_uri.clone(),
        open: started
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| started.verification_uri.clone()),
    });
    wake();
    let deadline = Instant::now() + Duration::from_secs(started.expires_in);
    let interval = Duration::from_secs(started.interval.max(2));
    while Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (u, id) = (url.clone(), started.login_id.clone());
        match blocking(move || hub::login_poll(&u, &id)).await? {
            LoginStatus::Pending => {}
            LoginStatus::Failed { reason } => anyhow::bail!("{reason}"),
            LoginStatus::Done {
                token,
                device_id,
                user,
            } => {
                let account = hub::Account {
                    url,
                    token,
                    device_id,
                    user: user.clone(),
                };
                hub::save_account(&paths.hub_account(), &account)?;
                if let Some(p) = &extra
                    && let Err(e) = hub::save_account(p, &account)
                {
                    tracing::warn!("cannot share the login with the service: {e:#}");
                }
                let p = paths.clone();
                if let Ok(list) = blocking(move || account.devices()).await {
                    let _ = hub::store_devices(&p, &list);
                    shared.lock().devices = Some(list.devices);
                }
                return Ok(Some(user));
            }
        }
    }
    anyhow::bail!("the code expired; log in again")
}

/// Open a web page in the default browser.
pub fn open_url(url: &str) {
    #[cfg(target_os = "android")]
    crate::android::open_url(url);
    #[cfg(not(target_os = "android"))]
    {
        let cmd = if cfg!(windows) {
            ("cmd", vec!["/C", "start", "", url])
        } else if cfg!(target_os = "macos") {
            ("open", vec![url])
        } else {
            ("xdg-open", vec![url])
        };
        if let Err(e) = std::process::Command::new(cmd.0).args(&cmd.1).spawn() {
            tracing::warn!("cannot open {url}: {e}");
        }
    }
}
