//! The oriel hub: an optional service that logs devices in (OpenID Connect
//! device flow), keeps the account's device list (so every logged-in
//! client may reach every logged-in host), relays connections that cannot
//! be made directly, and wakes phones with a push.
//!
//! Configuration (environment):
//! - `ORIEL_HUB_BIND` (HTTP, default 0.0.0.0:8080), `ORIEL_HUB_DATA_DIR`
//! - `ORIEL_HUB_OIDC_ISSUER`, `ORIEL_HUB_OIDC_CLIENT_ID`,
//!   `ORIEL_HUB_OIDC_CLIENT_SECRET`, `ORIEL_HUB_OIDC_ALLOWED_GROUPS`
//!   (comma separated)
//! - `ORIEL_HUB_RELAY_BIND` (UDP, unset: no relaying) and
//!   `ORIEL_HUB_RELAY_PUBLIC` (the "host:port" clients reach it at)
//! - `ORIEL_HUB_FCM_SERVICE_ACCOUNT` (the JSON key, or a path to it; unset:
//!   no push wake-ups)

mod api;
mod db;
mod oidc;
mod push;
mod relay;

use std::sync::Arc;

use anyhow::{Context, Result};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn required(name: &str) -> Result<String> {
    env(name).with_context(|| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oriel_hub=info".into()),
        )
        .init();
    let data = std::path::PathBuf::from(env("ORIEL_HUB_DATA_DIR").unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&data)?;
    let db = db::Db::open(&data.join("hub.sqlite"))?;
    let oidc = oidc::Oidc::new(oidc::Config {
        issuer: required("ORIEL_HUB_OIDC_ISSUER")?,
        client_id: required("ORIEL_HUB_OIDC_CLIENT_ID")?,
        client_secret: required("ORIEL_HUB_OIDC_CLIENT_SECRET")?,
        allowed_groups: env("ORIEL_HUB_OIDC_ALLOWED_GROUPS")
            .map(|g| g.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default(),
    });
    let relay = match env("ORIEL_HUB_RELAY_BIND") {
        Some(bind) => {
            Some(relay::Relay::bind(bind.parse().context("ORIEL_HUB_RELAY_BIND")?).await?)
        }
        None => None,
    };
    let relay_public = env("ORIEL_HUB_RELAY_PUBLIC").unwrap_or_default();
    let push = match env("ORIEL_HUB_FCM_SERVICE_ACCOUNT") {
        Some(v) => {
            let json = if v.trim_start().starts_with('{') {
                v
            } else {
                std::fs::read_to_string(&v).with_context(|| format!("read {v}"))?
            };
            Some(push::Fcm::new(&json)?)
        }
        None => None,
    };
    tracing::info!(
        relay = relay.is_some(),
        push = push.is_some(),
        "oriel hub starting"
    );
    let app = Arc::new(api::App::new(db, oidc, relay, relay_public, push));
    let bind = env("ORIEL_HUB_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "listening");
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
