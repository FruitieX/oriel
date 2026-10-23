//! Relaying connections that cannot be made directly (a client away from
//! home without the VPN): the hub forwards UDP datagrams between the client
//! and a host it reaches on the LAN. The QUIC connection inside stays
//! end-to-end encrypted and authenticated; the hub only moves packets.
//!
//! A relay is allocated over the authenticated API (a one-time token for
//! one host). The client then sends `RELAY_HELLO` + token from the socket
//! its connection will use; the hub latches that address, answers
//! `RELAY_READY`, and forwards everything between it and a socket of its
//! own connected to the host.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_proto::hub::{RELAY_HELLO, RELAY_READY};
use tokio::net::UdpSocket;

/// A relayed connection idle this long is dropped.
const IDLE: Duration = Duration::from_secs(60);
/// An allocated relay unused this long is dropped.
const UNCLAIMED: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Relay {
    state: Arc<Mutex<State>>,
    socket: Arc<UdpSocket>,
}

#[derive(Default)]
struct State {
    allocated: HashMap<[u8; 16], (SocketAddr, Instant)>,
    sessions: HashMap<SocketAddr, Arc<UdpSocket>>,
}

impl Relay {
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        let relay = Self {
            state: Default::default(),
            socket: Arc::new(UdpSocket::bind(addr).await?),
        };
        tracing::info!(%addr, "relay listening");
        tokio::spawn(relay.clone().run());
        Ok(relay)
    }

    /// A one-time token for relaying to `target`.
    pub fn allocate(&self, target: SocketAddr) -> String {
        let mut token = [0u8; 16];
        rand::fill(&mut token[..]);
        let mut state = self.state.lock().unwrap();
        state
            .allocated
            .retain(|_, (_, at)| at.elapsed() < UNCLAIMED);
        state.allocated.insert(token, (target, Instant::now()));
        hex::encode(token)
    }

    #[cfg(test)]
    pub fn sessions(&self) -> usize {
        self.state.lock().unwrap().sessions.len()
    }

    async fn run(self) {
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, from) = match self.socket.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("relay receive: {e}");
                    continue;
                }
            };
            let data = &buf[..n];
            let upstream = self.state.lock().unwrap().sessions.get(&from).cloned();
            if let Some(upstream) = upstream {
                if data.starts_with(RELAY_HELLO) {
                    // A repeated hello: the answer got lost.
                    let _ = self.socket.send_to(RELAY_READY, from).await;
                } else {
                    let _ = upstream.send(data).await;
                }
                continue;
            }
            if data.len() == RELAY_HELLO.len() + 16 && data.starts_with(RELAY_HELLO) {
                let token: [u8; 16] = data[RELAY_HELLO.len()..].try_into().unwrap();
                let target = self.state.lock().unwrap().allocated.remove(&token);
                if let Some((target, _)) = target {
                    if let Err(e) = self.open(from, target).await {
                        tracing::warn!(%target, "cannot relay: {e:#}");
                        continue;
                    }
                    let _ = self.socket.send_to(RELAY_READY, from).await;
                }
            }
        }
    }

    async fn open(&self, client: SocketAddr, target: SocketAddr) -> Result<()> {
        let bind: SocketAddr = if target.is_ipv4() {
            "0.0.0.0:0".parse()?
        } else {
            "[::]:0".parse()?
        };
        let upstream = Arc::new(UdpSocket::bind(bind).await?);
        upstream.connect(target).await?;
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(client, upstream.clone());
        tracing::info!(%client, %target, "relaying");
        let relay = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            // Until idle, or the host is gone.
            while let Ok(Ok(n)) = tokio::time::timeout(IDLE, upstream.recv(&mut buf)).await {
                let _ = relay.socket.send_to(&buf[..n], client).await;
            }
            relay.state.lock().unwrap().sessions.remove(&client);
            tracing::info!(%client, %target, "relay ended");
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_both_ways_after_hello() {
        let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let relay_addr = relay.socket.local_addr().unwrap();
        let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let token = relay.allocate(host.local_addr().unwrap());
        let mut hello = RELAY_HELLO.to_vec();
        hello.extend(hex::decode(&token).unwrap());
        // Anything before the hello is dropped.
        client.send_to(b"early", relay_addr).await.unwrap();
        client.send_to(&hello, relay_addr).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], RELAY_READY);
        client.send_to(b"ping", relay_addr).await.unwrap();
        let (n, from) = host.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        host.send_to(b"pong", from).await.unwrap();
        let (n, _) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong");
        // The token works once.
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        other.send_to(&hello, relay_addr).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), other.recv_from(&mut buf))
                .await
                .is_err()
        );
        assert_eq!(relay.sessions(), 1);
    }
}
