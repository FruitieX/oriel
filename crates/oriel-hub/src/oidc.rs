//! Logging devices in with the OAuth device flow (RFC 8628) against the
//! OpenID provider (Pocket ID): the device shows a code, the user approves
//! it on any browser with a passkey. Blocking calls (ureq): run them on a
//! blocking thread.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;

pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Login needs membership in one of these groups (empty: anyone the
    /// provider lets through).
    pub allowed_groups: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    token_endpoint: String,
    device_authorization_endpoint: String,
}

pub struct Oidc {
    config: Config,
    discovery: Mutex<Option<Discovery>>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "five")]
    pub interval: u64,
}

fn five() -> u64 {
    5
}

#[derive(Debug, PartialEq)]
pub enum Poll {
    Pending,
    /// Poll less often.
    SlowDown,
    Done {
        sub: String,
        name: String,
    },
    Denied(String),
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    name: Option<String>,
    preferred_username: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

impl Oidc {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            discovery: Mutex::new(None),
        }
    }

    fn discovery(&self) -> Result<Discovery> {
        if let Some(d) = self.discovery.lock().unwrap().clone() {
            return Ok(d);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let d: Discovery = ureq::get(&url)
            .call()
            .with_context(|| format!("fetch {url}"))?
            .body_mut()
            .read_json()?;
        *self.discovery.lock().unwrap() = Some(d.clone());
        Ok(d)
    }

    fn post(&self, url: &str, body: &[(&str, &str)]) -> Result<(u16, serde_json::Value)> {
        let mut response = ureq::post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .config()
            .http_status_as_error(false)
            .build()
            .send(form(body))
            .with_context(|| format!("POST {url}"))?;
        let status = response.status().as_u16();
        Ok((status, response.body_mut().read_json()?))
    }

    pub fn start(&self) -> Result<DeviceAuth> {
        let d = self.discovery()?;
        let (status, body) = self.post(
            &d.device_authorization_endpoint,
            &[
                ("client_id", &self.config.client_id),
                ("client_secret", &self.config.client_secret),
                ("scope", "openid profile groups"),
            ],
        )?;
        if status != 200 {
            bail!("device authorization refused ({status}): {body}");
        }
        Ok(serde_json::from_value(body)?)
    }

    pub fn poll(&self, device_code: &str) -> Result<Poll> {
        let d = self.discovery()?;
        let (status, body) = self.post(
            &d.token_endpoint,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", &self.config.client_id),
                ("client_secret", &self.config.client_secret),
            ],
        )?;
        if status != 200 {
            return Ok(match body["error"].as_str().unwrap_or("") {
                "authorization_pending" => Poll::Pending,
                "slow_down" => Poll::SlowDown,
                "access_denied" => Poll::Denied("the login was declined".into()),
                "expired_token" => Poll::Denied("the code expired".into()),
                other => Poll::Denied(format!("login failed ({other})")),
            });
        }
        let id_token = body["id_token"].as_str().context("no ID token")?;
        self.check(&d, id_token)
    }

    /// The ID token came straight from the token endpoint over TLS, so its
    /// signature need not be checked (OpenID Connect Core 3.1.3.7); the
    /// claims still are.
    fn check(&self, d: &Discovery, id_token: &str) -> Result<Poll> {
        let payload = id_token.split('.').nth(1).context("malformed ID token")?;
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
        if claims.iss != d.issuer {
            bail!("ID token from unexpected issuer {}", claims.iss);
        }
        let ours = match &claims.aud {
            Audience::One(a) => a == &self.config.client_id,
            Audience::Many(a) => a.iter().any(|a| a == &self.config.client_id),
        };
        if !ours {
            bail!("ID token is for another client");
        }
        if claims.exp <= crate::db::now() {
            bail!("ID token has expired");
        }
        let allowed = &self.config.allowed_groups;
        if !allowed.is_empty() && !claims.groups.iter().any(|g| allowed.contains(g)) {
            return Ok(Poll::Denied("this account may not use oriel".into()));
        }
        let name = claims
            .name
            .or(claims.preferred_username)
            .unwrap_or_else(|| claims.sub.clone());
        Ok(Poll::Done {
            sub: format!("{}#{}", claims.iss, claims.sub),
            name,
        })
    }
}
