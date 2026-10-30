//! The HTTP API (see `oriel_proto::hub`).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use oriel_proto::hub::*;

use crate::{
    db::{Db, DeviceRow, now},
    oidc::{Oidc, Poll},
};

/// Devices that reported in this recently count as online.
pub const ONLINE_SECS: u64 = 120;
/// Logins waiting for approval, at most.
const MAX_LOGINS: usize = 64;

pub struct App {
    pub db: Db,
    pub oidc: Oidc,
    pub relay: Option<crate::relay::Relay>,
    /// What clients send relay datagrams to ("host:port").
    pub relay_public: String,
    pub push: Option<crate::push::Fcm>,
    logins: Mutex<HashMap<String, PendingLogin>>,
}

struct PendingLogin {
    device_code: String,
    start: LoginStart,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
}

impl App {
    pub fn new(
        db: Db,
        oidc: Oidc,
        relay: Option<crate::relay::Relay>,
        relay_public: String,
        push: Option<crate::push::Fcm>,
    ) -> Self {
        Self {
            db,
            oidc,
            relay,
            relay_public,
            push,
            logins: Default::default(),
        }
    }
}

type Shared = State<Arc<App>>;

pub struct Error(StatusCode, String);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        tracing::warn!("request failed: {e:#}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn bad(status: StatusCode, msg: &str) -> Error {
    Error(status, msg.into())
}

/// The device making the request (Bearer token).
pub struct Me(DeviceRow);

impl FromRequestParts<Arc<App>> for Me {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Error> {
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| bad(StatusCode::UNAUTHORIZED, "not logged in"))?;
        app.db
            .by_token(token.trim())?
            .map(Me)
            .ok_or_else(|| bad(StatusCode::UNAUTHORIZED, "this device was logged out"))
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/login/start", post(login_start))
        .route("/api/login/poll", post(login_poll))
        .route("/api/presence", post(presence))
        .route("/api/devices", get(devices))
        .route("/api/devices/{id}", delete(remove))
        .route("/api/relay", post(allocate_relay))
        .route("/api/wake", post(wake))
        .with_state(app)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Error> {
    Ok(tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))??)
}

async fn login_start(
    State(app): Shared,
    Json(req): Json<LoginStart>,
) -> Result<Json<LoginStarted>, Error> {
    if req.fingerprint.len() != 64 || hex::decode(&req.fingerprint).is_err() {
        return Err(bad(StatusCode::BAD_REQUEST, "bad fingerprint"));
    }
    if req.name.trim().is_empty() || req.name.len() > 64 || req.platform.len() > 32 {
        return Err(bad(StatusCode::BAD_REQUEST, "bad device name"));
    }
    {
        let mut logins = app.logins.lock().unwrap();
        logins.retain(|_, l| l.expires > Instant::now());
        if logins.len() >= MAX_LOGINS {
            return Err(bad(
                StatusCode::TOO_MANY_REQUESTS,
                "too many logins at once",
            ));
        }
    }
    let a = app.clone();
    let auth = blocking(move || a.oidc.start()).await?;
    let login_id = crate::db::random_hex(16);
    let interval = Duration::from_secs(auth.interval.max(2));
    app.logins.lock().unwrap().insert(
        login_id.clone(),
        PendingLogin {
            device_code: auth.device_code,
            start: req,
            expires: Instant::now() + Duration::from_secs(auth.expires_in),
            next_poll: Instant::now() + interval,
            interval,
        },
    );
    Ok(Json(LoginStarted {
        login_id,
        user_code: auth.user_code,
        verification_uri: auth.verification_uri,
        verification_uri_complete: auth.verification_uri_complete,
        interval: interval.as_secs(),
        expires_in: auth.expires_in,
    }))
}

async fn login_poll(
    State(app): Shared,
    Json(req): Json<LoginPoll>,
) -> Result<Json<LoginStatus>, Error> {
    let device_code = {
        let mut logins = app.logins.lock().unwrap();
        let Some(login) = logins.get_mut(&req.login_id) else {
            return Ok(Json(LoginStatus::Failed {
                reason: "the login expired; start again".into(),
            }));
        };
        // The device polls as often as it likes; the provider only at its
        // interval.
        if Instant::now() < login.next_poll {
            return Ok(Json(LoginStatus::Pending));
        }
        login.next_poll = Instant::now() + login.interval;
        login.device_code.clone()
    };
    let a = app.clone();
    let result = blocking(move || a.oidc.poll(&device_code)).await?;
    let mut logins = app.logins.lock().unwrap();
    match result {
        Poll::Pending => Ok(Json(LoginStatus::Pending)),
        Poll::SlowDown => {
            if let Some(l) = logins.get_mut(&req.login_id) {
                l.interval += Duration::from_secs(5);
                l.next_poll = Instant::now() + l.interval;
            }
            Ok(Json(LoginStatus::Pending))
        }
        Poll::Denied(reason) => {
            logins.remove(&req.login_id);
            Ok(Json(LoginStatus::Failed { reason }))
        }
        Poll::Done { sub, name } => {
            let Some(login) = logins.remove(&req.login_id) else {
                return Ok(Json(LoginStatus::Pending));
            };
            drop(logins);
            let s = &login.start;
            let (device_id, token) =
                app.db
                    .register(&sub, &name, &s.fingerprint, s.name.trim(), &s.platform)?;
            tracing::info!(device = %s.name, %device_id, user = %name, "device logged in");
            Ok(Json(LoginStatus::Done {
                token,
                device_id,
                user: name,
            }))
        }
    }
}

fn list(app: &App, me: &DeviceRow) -> Result<Devices, Error> {
    let now = now();
    let devices = app
        .db
        .list(&me.owner)?
        .into_iter()
        .map(|d| Device {
            online: now.saturating_sub(d.last_seen) <= ONLINE_SECS,
            this_device: d.id == me.id,
            wakeable: d.push_token.is_some() && app.push.is_some(),
            id: d.id,
            name: d.name,
            fingerprint: d.fingerprint,
            platform: d.platform,
            host: d.host,
            last_seen: d.last_seen,
        })
        .collect();
    Ok(Devices {
        devices,
        user: me.owner_name.clone(),
    })
}

async fn presence(
    State(app): Shared,
    Me(me): Me,
    Json(p): Json<Presence>,
) -> Result<Json<Devices>, Error> {
    app.db.presence(&me.id, &p)?;
    Ok(Json(list(&app, &me)?))
}

async fn devices(State(app): Shared, Me(me): Me) -> Result<Json<Devices>, Error> {
    Ok(Json(list(&app, &me)?))
}

async fn remove(
    State(app): Shared,
    Me(me): Me,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    if app.db.remove(&me.owner, &id)? {
        tracing::info!(%id, by = %me.name, "device removed");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(bad(StatusCode::NOT_FOUND, "no such device"))
    }
}

/// The host's address the hub can reach: its private (LAN) IPv4 addresses
/// first.
fn target(host: &HostInfo) -> Option<SocketAddr> {
    let addrs: Vec<IpAddr> = host
        .addresses
        .iter()
        .filter_map(|a| a.parse().ok())
        .collect();
    let private = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(_) => false,
    };
    addrs
        .iter()
        .find(|ip| private(ip))
        .or_else(|| addrs.first())
        .map(|ip| SocketAddr::new(*ip, host.port))
}

async fn allocate_relay(
    State(app): Shared,
    Me(me): Me,
    Json(req): Json<RelayRequest>,
) -> Result<Json<Relay>, Error> {
    let relay = app
        .relay
        .as_ref()
        .ok_or_else(|| bad(StatusCode::NOT_IMPLEMENTED, "this hub does not relay"))?;
    let device = app
        .db
        .get(&me.owner, &req.device_id)?
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such device"))?;
    let host = device
        .host
        .as_ref()
        .filter(|_| now().saturating_sub(device.last_seen) <= ONLINE_SECS)
        .ok_or_else(|| bad(StatusCode::CONFLICT, "that computer is not sharing"))?;
    let target = target(host).ok_or_else(|| bad(StatusCode::CONFLICT, "no address"))?;
    let token = relay.allocate(target);
    tracing::info!(from = %me.name, to = %device.name, %target, "relay allocated");
    Ok(Json(Relay {
        address: app.relay_public.clone(),
        token,
    }))
}

async fn wake(
    State(app): Shared,
    Me(me): Me,
    Json(req): Json<WakeRequest>,
) -> Result<StatusCode, Error> {
    let push = app
        .push
        .as_ref()
        .ok_or_else(|| bad(StatusCode::NOT_IMPLEMENTED, "this hub cannot wake devices"))?;
    let device = app
        .db
        .get(&me.owner, &req.device_id)?
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such device"))?;
    let token = device
        .push_token
        .ok_or_else(|| bad(StatusCode::CONFLICT, "that device cannot be woken"))?;
    let push = push.clone();
    let from = me.name.clone();
    blocking(move || push.wake(&token, &from)).await?;
    tracing::info!(from = %me.name, to = %device.name, "woke device");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn app() -> Arc<App> {
        let oidc = Oidc::new(crate::oidc::Config {
            issuer: "http://127.0.0.1:9".into(),
            client_id: "oriel".into(),
            client_secret: "secret".into(),
            allowed_groups: vec![],
        });
        Arc::new(App::new(
            Db::memory().unwrap(),
            oidc,
            None,
            String::new(),
            None,
        ))
    }

    async fn call(
        app: &Arc<App>,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = match body {
            Some(json) => req
                .header("content-type", "application/json")
                .body(Body::from(json.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = router(app.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    // Smoke tests of the HTTP API: health, authentication, and a logged-in
    // device reporting in and listing the account's devices.
    #[tokio::test]
    async fn health_answers() {
        let (status, body) = call(&app(), "GET", "/healthz", None, None).await;
        assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"ok"[..]));
    }

    #[tokio::test]
    async fn api_needs_a_device_token() {
        let app = app();
        let (status, body) = call(&app, "GET", "/api/devices", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let err: ApiError = serde_json::from_slice(&body).unwrap();
        assert_eq!(err.error, "not logged in");
        let (status, _) = call(&app, "GET", "/api/devices", Some("nope"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn presence_and_device_list() {
        let app = app();
        let (id, token) = app
            .db
            .register("sub1", "Rasmus", &"ab".repeat(32), "laptop", "linux")
            .unwrap();
        let presence = serde_json::json!({
            "name": "laptop",
            "platform": "linux",
            "host": { "port": 48600, "addresses": ["192.168.1.2"] },
        });
        let (status, body) =
            call(&app, "POST", "/api/presence", Some(&token), Some(presence)).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Devices = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed.user, "Rasmus");
        let me = &listed.devices[0];
        assert_eq!(me.id, id);
        assert!(me.online && me.this_device);
        assert_eq!(me.host.as_ref().unwrap().port, 48600);

        let (status, body) = call(&app, "GET", "/api/devices", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        let again: Devices = serde_json::from_slice(&body).unwrap();
        assert_eq!(again.devices, listed.devices);

        // This hub has no relay.
        let req = serde_json::json!({ "device_id": id });
        let (status, _) = call(&app, "POST", "/api/relay", Some(&token), Some(req)).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn relay_target_prefers_lan_addresses() {
        let host = HostInfo {
            port: 48600,
            addresses: vec!["100.64.1.2".into(), "192.168.1.166".into()],
        };
        assert_eq!(target(&host), Some("192.168.1.166:48600".parse().unwrap()));
    }
}
