//! SIP over UDP or TCP. Every byte in or out passes through the trace.
//!
//! TCP binds one local port for both the connection to the proxy and a listener, so the port
//! in Contact reaches us whether the proxy reuses the registration's connection or opens a new one.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{Notify, mpsc};

use super::message::SipMsg;
use crate::trace::{SipTrace, split_sip_message};

pub struct Incoming {
    pub msg: SipMsg,
    pub from: SocketAddr,
}

pub struct Transport {
    pub name: &'static str,
    pub local: SocketAddr,
    pub proxy: SocketAddr,
    trace: Arc<SipTrace>,
    kind: Kind,
    incoming: mpsc::UnboundedSender<Incoming>,
    /// Signalled when the connection to the proxy closes, so registration starts over at once.
    pub proxy_lost: Notify,
}

enum Kind {
    Udp(Arc<UdpSocket>),
    Tcp(Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Vec<u8>>>>),
}

/// The local address the OS would use to reach `proxy`.
pub fn outbound_ip(proxy: SocketAddr) -> std::io::Result<IpAddr> {
    let s = std::net::UdpSocket::bind(if proxy.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    s.connect(proxy)?;
    Ok(s.local_addr()?.ip())
}

impl Transport {
    pub async fn bind(transport: &str, proxy: SocketAddr, trace: Arc<SipTrace>) -> anyhow::Result<(Arc<Transport>, mpsc::UnboundedReceiver<Incoming>)> {
        let ip = outbound_ip(proxy)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let t = if transport == "udp" {
            let sock = Arc::new(UdpSocket::bind(SocketAddr::new(ip, 0)).await?);
            let local = sock.local_addr()?;
            let t = Arc::new(Transport { name: "UDP", local, proxy, trace, kind: Kind::Udp(sock.clone()), incoming: tx, proxy_lost: Notify::new() });
            let weak = Arc::downgrade(&t);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65535];
                while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                    let Some(t) = weak.upgrade() else { return };
                    t.trace.read("UDP", &from.to_string(), &buf[..n]);
                    t.deliver(&buf[..n], from);
                }
            });
            t
        } else {
            let listener = reusable(ip)?;
            listener.bind(SocketAddr::new(ip, 0))?;
            let local = listener.local_addr()?;
            let listener: TcpListener = listener.listen(16)?;
            let t =
                Arc::new(Transport { name: "TCP", local, proxy, trace, kind: Kind::Tcp(Mutex::new(HashMap::new())), incoming: tx, proxy_lost: Notify::new() });
            let weak = Arc::downgrade(&t);
            tokio::spawn(async move {
                while let Ok((stream, peer)) = listener.accept().await {
                    let Some(t) = weak.upgrade() else { return };
                    t.adopt(stream, peer);
                }
            });
            t
        };
        Ok((t, rx))
    }

    fn deliver(&self, bytes: &[u8], from: SocketAddr) {
        match SipMsg::parse(bytes) {
            Ok(msg) => {
                let _ = self.incoming.send(Incoming { msg, from });
            }
            Err(e) => tracing::warn!(%from, error = %e, "Dropped an unparseable SIP message"),
        }
    }

    /// Sends to `to`, connecting first over TCP when no connection to it is open.
    pub async fn send(self: &Arc<Self>, to: SocketAddr, msg: &SipMsg) -> anyhow::Result<()> {
        let bytes = msg.to_bytes();
        match &self.kind {
            Kind::Udp(sock) => {
                self.trace.write("UDP", &to.to_string(), &bytes);
                sock.send_to(&bytes, to).await?;
            }
            Kind::Tcp(conns) => {
                let existing = conns.lock().unwrap().get(&to).cloned();
                let tx = match existing {
                    Some(tx) => tx,
                    None => {
                        let sock = reusable(self.local.ip())?;
                        sock.bind(self.local)?;
                        let stream = tokio::time::timeout(std::time::Duration::from_secs(10), sock.connect(to))
                            .await
                            .map_err(|_| anyhow::anyhow!("dial tcp {to}: i/o timeout"))??;
                        self.adopt(stream, to)
                    }
                };
                self.trace.write("TCP", &to.to_string(), &bytes);
                tx.send(bytes).map_err(|_| anyhow::anyhow!("connection to {to} closed"))?;
            }
        }
        Ok(())
    }

    /// Sends CRLFCRLF keepalives on TCP (RFC 5626 §4.4.1); UDP needs none here.
    pub fn keepalive(&self) {
        if let Kind::Tcp(conns) = &self.kind
            && let Some(tx) = conns.lock().unwrap().get(&self.proxy)
        {
            let _ = tx.send(b"\r\n\r\n".to_vec());
        }
    }

    fn adopt(self: &Arc<Self>, stream: TcpStream, peer: SocketAddr) -> mpsc::UnboundedSender<Vec<u8>> {
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        if let Kind::Tcp(conns) = &self.kind {
            conns.lock().unwrap().insert(peer, tx.clone());
        }
        tokio::spawn(async move {
            while let Some(b) = rx.recv().await {
                if wr.write_all(&b).await.is_err() {
                    return;
                }
            }
        });
        let weak = Arc::downgrade(self);
        let mine = tx.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 16384];
            loop {
                let n = match rd.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let Some(t) = weak.upgrade() else { return };
                t.trace.read("TCP", &peer.to_string(), &chunk[..n]);
                buf.extend_from_slice(&chunk[..n]);
                while let Some((msg, rest)) = split_sip_message(&buf) {
                    t.deliver(&msg, peer);
                    buf = rest;
                }
                if buf.iter().all(|b| *b == b'\r' || *b == b'\n') {
                    buf.clear();
                }
            }
            if let Some(t) = weak.upgrade() {
                if let Kind::Tcp(conns) = &t.kind {
                    let mut g = conns.lock().unwrap();
                    if g.get(&peer).is_some_and(|c| c.same_channel(&mine)) {
                        g.remove(&peer);
                    }
                }
                if peer == t.proxy {
                    tracing::warn!(%peer, "Connection to the proxy closed");
                    t.proxy_lost.notify_one();
                }
            }
        });
        tx
    }
}

fn reusable(ip: IpAddr) -> std::io::Result<TcpSocket> {
    let s = if ip.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    s.set_reuseaddr(true)?;
    #[cfg(unix)]
    s.set_reuseport(true)?;
    Ok(s)
}
