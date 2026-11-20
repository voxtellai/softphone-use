//! RTP for one G.711 stream (RFC 3550), with symmetric RTP: once a packet arrives, replies go
//! to where it came from, as a phone behind NAT needs.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::net::UdpSocket;

use crate::g711::Codec;

pub struct Rtp {
    sock: UdpSocket,
    remote: Mutex<SocketAddr>,
    learned: AtomicBool,
    codec: Mutex<Codec>,
    out: Mutex<(u16, u32, bool)>,
    ssrc: u32,
}

pub const SAMPLES_PER_FRAME: usize = 160;

impl Rtp {
    pub async fn bind(ip: std::net::IpAddr) -> std::io::Result<Rtp> {
        Ok(Rtp {
            sock: UdpSocket::bind(SocketAddr::new(ip, 0)).await?,
            remote: Mutex::new(SocketAddr::from(([0, 0, 0, 0], 0))),
            learned: AtomicBool::new(false),
            codec: Mutex::new(Codec::Pcmu),
            out: Mutex::new((rand::random(), rand::random(), true)),
            ssrc: rand::random(),
        })
    }

    pub fn local(&self) -> SocketAddr {
        self.sock.local_addr().expect("bound socket")
    }

    pub fn remote(&self) -> SocketAddr {
        *self.remote.lock().unwrap()
    }

    pub fn codec(&self) -> Codec {
        *self.codec.lock().unwrap()
    }

    /// Points the stream at the far end's SDP. A re-INVITE may move it.
    pub fn set_remote(&self, addr: SocketAddr, codec: Codec) {
        *self.remote.lock().unwrap() = addr;
        *self.codec.lock().unwrap() = codec;
        self.learned.store(false, Ordering::Relaxed);
    }

    /// The next packet in the stream's codec: its payload copied into `payload`. Packets of other
    /// payload types (telephone-event) are skipped.
    pub async fn recv(&self, buf: &mut [u8], payload: &mut Vec<u8>) -> std::io::Result<()> {
        loop {
            let (n, from) = self.sock.recv_from(buf).await?;
            let Some((pt, body)) = parse(&buf[..n]) else { continue };
            if !self.learned.swap(true, Ordering::Relaxed) {
                *self.remote.lock().unwrap() = from;
            }
            if pt != self.codec().payload_type() {
                continue;
            }
            payload.clear();
            payload.extend_from_slice(body);
            return Ok(());
        }
    }

    pub async fn send(&self, payload: &[u8]) -> std::io::Result<()> {
        let pkt = {
            let mut o = self.out.lock().unwrap();
            let (seq, ts, first) = *o;
            let mut p = Vec::with_capacity(12 + payload.len());
            p.push(0x80);
            p.push(self.codec().payload_type() | if first { 0x80 } else { 0 });
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&ts.to_be_bytes());
            p.extend_from_slice(&self.ssrc.to_be_bytes());
            p.extend_from_slice(payload);
            *o = (seq.wrapping_add(1), ts.wrapping_add(payload.len() as u32), false);
            p
        };
        let to = self.remote();
        if to.port() == 0 {
            return Ok(());
        }
        self.sock.send_to(&pkt, to).await.map(|_| ())
    }
}

/// The payload type and payload of an RTP packet, past CSRCs, the extension and padding.
pub fn parse(pkt: &[u8]) -> Option<(u8, &[u8])> {
    if pkt.len() < 12 || pkt[0] >> 6 != 2 {
        return None;
    }
    let cc = (pkt[0] & 0x0f) as usize;
    let mut start = 12 + cc * 4;
    if pkt[0] & 0x10 != 0 {
        let ext = pkt.get(start + 2..start + 4)?;
        start += 4 + u16::from_be_bytes([ext[0], ext[1]]) as usize * 4;
    }
    let mut end = pkt.len();
    if pkt[0] & 0x20 != 0 {
        end = end.checked_sub(*pkt.last()? as usize)?;
    }
    (start <= end).then(|| (pkt[1] & 0x7f, &pkt[start..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_with_csrcs_extension_and_padding() {
        let mut p = vec![0x80 | 0x20 | 0x10 | 1, 0x80, 0, 1, 0, 0, 0, 160, 1, 2, 3, 4];
        p.extend([9, 9, 9, 9]); // one CSRC
        p.extend([0xbe, 0xde, 0, 1, 7, 7, 7, 7]); // a one-word extension
        p.extend([0xff, 0xfe]); // payload
        p.extend([0, 2]); // two bytes of padding
        let (pt, body) = parse(&p).unwrap();
        assert_eq!(pt, 0);
        assert_eq!(body, &[0xff, 0xfe]);
        assert!(parse(&[0x40; 12]).is_none(), "version 1 is rejected");
    }

    #[tokio::test]
    async fn sends_and_receives_between_two_streams() {
        let ip = "127.0.0.1".parse().unwrap();
        let (a, b) = (Rtp::bind(ip).await.unwrap(), Rtp::bind(ip).await.unwrap());
        a.set_remote(b.local(), Codec::Pcma);
        b.set_remote("127.0.0.1:9".parse().unwrap(), Codec::Pcma);
        a.send(&[1, 2, 3]).await.unwrap();
        let (mut buf, mut payload) = (vec![0u8; 1500], Vec::new());
        b.recv(&mut buf, &mut payload).await.unwrap();
        assert_eq!(payload, vec![1, 2, 3]);
        assert_eq!(b.remote(), a.local(), "symmetric RTP replies to the source");
    }
}
