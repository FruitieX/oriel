//! Waking phones with a Firebase Cloud Messaging data message (HTTP v1
//! API, signed with a service account key). High priority messages wake
//! a sleeping Android device and start the app's messaging service.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;

#[derive(Clone)]
pub struct Fcm(Arc<Inner>);

struct Inner {
    project_id: String,
    client_email: String,
    token_uri: String,
    key: RsaKeyPair,
    /// (access token, valid until).
    access: Mutex<Option<(String, u64)>>,
}

#[derive(Deserialize)]
struct ServiceAccount {
    project_id: String,
    client_email: String,
    private_key: String,
    #[serde(default = "token_uri")]
    token_uri: String,
}

fn token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

impl Fcm {
    /// From the service account JSON key.
    pub fn new(json: &str) -> Result<Self> {
        let sa: ServiceAccount = serde_json::from_str(json).context("parse service account")?;
        let pem: String = sa
            .private_key
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = STANDARD.decode(pem.trim()).context("decode private key")?;
        let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| anyhow::anyhow!("private key: {e}"))?;
        Ok(Self(Arc::new(Inner {
            project_id: sa.project_id,
            client_email: sa.client_email,
            token_uri: sa.token_uri,
            key,
            access: Mutex::new(None),
        })))
    }

    fn access_token(&self) -> Result<String> {
        let now = crate::db::now();
        if let Some((token, until)) = self.0.access.lock().unwrap().clone()
            && until > now + 60
        {
            return Ok(token);
        }
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": self.0.client_email,
                "scope": "https://www.googleapis.com/auth/firebase.messaging",
                "aud": self.0.token_uri,
                "iat": now,
                "exp": now + 3600,
            })
            .to_string(),
        );
        let input = format!("{header}.{claims}");
        let mut sig = vec![0u8; self.0.key.public().modulus_len()];
        self.0
            .key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| anyhow::anyhow!("signing failed"))?;
        let jwt = format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig));
        let body = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}"
        );
        let resp: serde_json::Value = ureq::post(&self.0.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .send(body)
            .context("get an access token")?
            .body_mut()
            .read_json()?;
        let token = resp["access_token"]
            .as_str()
            .context("no access token")?
            .to_string();
        let expires = resp["expires_in"].as_u64().unwrap_or(3600);
        *self.0.access.lock().unwrap() = Some((token.clone(), now + expires));
        Ok(token)
    }

    /// Ask a device to start sharing. Blocking.
    pub fn wake(&self, push_token: &str, from: &str) -> Result<()> {
        let access = self.access_token()?;
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            self.0.project_id
        );
        let message = serde_json::json!({
            "message": {
                "token": push_token,
                "data": { "type": "wake", "from": from },
                "android": { "priority": "high", "ttl": "60s" },
            }
        });
        let mut resp = ureq::post(&url)
            .header("authorization", &format!("Bearer {access}"))
            .config()
            .http_status_as_error(false)
            .build()
            .send_json(&message)?;
        if resp.status() != 200 {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            bail!("push refused ({}): {text}", resp.status());
        }
        Ok(())
    }
}
