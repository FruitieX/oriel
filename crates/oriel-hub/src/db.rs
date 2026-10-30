//! The device list (SQLite). A device is one identity fingerprint, owned
//! by the account (OIDC subject) that logged it in; its credential is a
//! random token stored as a SHA-256 hash.

use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result};
use oriel_proto::hub::{HostInfo, Presence};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub struct Db(Mutex<Connection>);

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: String,
    pub owner: String,
    pub owner_name: String,
    pub fingerprint: String,
    pub name: String,
    pub platform: String,
    pub host: Option<HostInfo>,
    pub push_token: Option<String>,
    pub last_seen: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `now()` as SQLite stores integers (rusqlite has no `u64` conversions).
fn now_sql() -> i64 {
    i64::try_from(now()).unwrap_or(i64::MAX)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

const COLUMNS: &str = "id, owner, owner_name, fingerprint, name, platform, host_port, host_addresses, push_token, last_seen";

fn row(r: &rusqlite::Row) -> rusqlite::Result<DeviceRow> {
    let port: Option<u16> = r.get(6)?;
    let addresses: Option<String> = r.get(7)?;
    Ok(DeviceRow {
        id: r.get(0)?,
        owner: r.get(1)?,
        owner_name: r.get(2)?,
        fingerprint: r.get(3)?,
        name: r.get(4)?,
        platform: r.get(5)?,
        host: port.map(|port| HostInfo {
            port,
            addresses: addresses
                .and_then(|a| serde_json::from_str(&a).ok())
                .unwrap_or_default(),
        }),
        push_token: r.get(8)?,
        last_seen: u64::try_from(r.get::<_, i64>(9)?).unwrap_or(0),
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = DELETE;
             CREATE TABLE IF NOT EXISTS devices (
                 id TEXT PRIMARY KEY,
                 owner TEXT NOT NULL,
                 owner_name TEXT NOT NULL,
                 fingerprint TEXT NOT NULL UNIQUE,
                 name TEXT NOT NULL,
                 platform TEXT NOT NULL,
                 token_hash TEXT NOT NULL UNIQUE,
                 host_port INTEGER,
                 host_addresses TEXT,
                 push_token TEXT,
                 last_seen INTEGER NOT NULL,
                 created INTEGER NOT NULL
             );",
        )?;
        Ok(Self(Mutex::new(conn)))
    }

    /// Log a device in: a new credential, replacing what the same
    /// fingerprint had. Returns (device id, token).
    pub fn register(
        &self,
        owner: &str,
        owner_name: &str,
        fingerprint: &str,
        name: &str,
        platform: &str,
    ) -> Result<(String, String)> {
        let token = random_hex(32);
        let conn = self.0.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM devices WHERE fingerprint = ?1",
                [fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(|| random_hex(8));
        conn.execute(
            "INSERT INTO devices (id, owner, owner_name, fingerprint, name, platform, token_hash, last_seen, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(fingerprint) DO UPDATE SET owner = ?2, owner_name = ?3, name = ?5,
                 platform = ?6, token_hash = ?7, last_seen = ?8",
            params![id, owner, owner_name, fingerprint, name, platform, hash(&token), now_sql()],
        )?;
        Ok((id, token))
    }

    pub fn by_token(&self, token: &str) -> Result<Option<DeviceRow>> {
        let conn = self.0.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM devices WHERE token_hash = ?1"),
                [hash(token)],
                row,
            )
            .optional()?)
    }

    pub fn presence(&self, id: &str, p: &Presence) -> Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE devices SET name = ?2, platform = ?3, host_port = ?4, host_addresses = ?5,
                 push_token = COALESCE(?6, push_token), last_seen = ?7 WHERE id = ?1",
            params![
                id,
                p.name.chars().take(64).collect::<String>(),
                p.platform.chars().take(32).collect::<String>(),
                p.host.as_ref().map(|h| h.port),
                p.host
                    .as_ref()
                    .map(|h| serde_json::to_string(&h.addresses).unwrap_or_default()),
                p.push_token,
                now_sql()
            ],
        )?;
        Ok(())
    }

    pub fn list(&self, owner: &str) -> Result<Vec<DeviceRow>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM devices WHERE owner = ?1 ORDER BY name"
        ))?;
        let rows = stmt.query_map([owner], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, owner: &str, id: &str) -> Result<Option<DeviceRow>> {
        Ok(self.list(owner)?.into_iter().find(|d| d.id == id))
    }

    pub fn remove(&self, owner: &str, id: &str) -> Result<bool> {
        let conn = self.0.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM devices WHERE owner = ?1 AND id = ?2",
            params![owner, id],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_presence_and_revoke() {
        let db = Db::memory().unwrap();
        let fp = "ab".repeat(32);
        let (id, token) = db
            .register("sub1", "Rasmus", &fp, "laptop", "linux")
            .unwrap();
        let me = db.by_token(&token).unwrap().unwrap();
        assert_eq!((me.id.as_str(), me.name.as_str()), (id.as_str(), "laptop"));
        db.presence(
            &id,
            &Presence {
                name: "laptop".into(),
                platform: "linux".into(),
                host: Some(HostInfo {
                    port: 48600,
                    addresses: vec!["192.168.1.2".into()],
                }),
                push_token: None,
            },
        )
        .unwrap();
        let listed = db.list("sub1").unwrap();
        assert_eq!(listed[0].host.as_ref().unwrap().port, 48600);
        assert!(db.list("someone else").unwrap().is_empty());
        // Logging in again replaces the credential, keeps the id.
        let (id2, token2) = db
            .register("sub1", "Rasmus", &fp, "laptop", "linux")
            .unwrap();
        assert_eq!(id, id2);
        assert!(db.by_token(&token).unwrap().is_none());
        assert!(db.by_token(&token2).unwrap().is_some());
        assert!(db.remove("sub1", &id).unwrap());
        assert!(db.by_token(&token2).unwrap().is_none());
    }
}
