//! The SDP the softphone offers and answers with: G.711 and telephone-event, nothing else.

use std::net::{IpAddr, SocketAddr};

use crate::g711::Codec;

pub const TELEPHONE_EVENT: u8 = 101;

pub fn offer(addr: SocketAddr, session: u64) -> String {
    build(addr, session, &[Codec::Pcmu.payload_type(), Codec::Pcma.payload_type(), TELEPHONE_EVENT])
}

/// The answer to `remote`: its first G.711 codec, plus telephone-event when offered.
pub fn answer(addr: SocketAddr, session: u64, remote: &RemoteMedia) -> String {
    let mut pts = vec![remote.codec.payload_type()];
    if remote.telephone_event {
        pts.push(TELEPHONE_EVENT);
    }
    build(addr, session, &pts)
}

fn build(addr: SocketAddr, session: u64, pts: &[u8]) -> String {
    let ip = addr.ip();
    let fam = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let mut s = format!(
        "v=0\r\no=- {session} {session} IN {fam} {ip}\r\ns=softphone-use\r\nc=IN {fam} {ip}\r\nt=0 0\r\nm=audio {} RTP/AVP {}\r\n",
        addr.port(),
        pts.iter().map(u8::to_string).collect::<Vec<_>>().join(" ")
    );
    for pt in pts {
        match *pt {
            0 => s.push_str("a=rtpmap:0 PCMU/8000\r\n"),
            8 => s.push_str("a=rtpmap:8 PCMA/8000\r\n"),
            TELEPHONE_EVENT => s.push_str("a=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\n"),
            _ => {}
        }
    }
    s.push_str("a=ptime:20\r\na=sendrecv\r\n");
    s
}

/// Where the far end wants RTP, and in which codec.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteMedia {
    pub addr: SocketAddr,
    pub codec: Codec,
    pub telephone_event: bool,
}

pub fn parse_remote(sdp: &str) -> anyhow::Result<RemoteMedia> {
    let (mut ip, mut media_ip, mut port, mut pts) = (None, None, None, Vec::new());
    let mut in_audio = false;
    for line in sdp.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("m=") {
            in_audio = rest.starts_with("audio");
            if in_audio {
                let f: Vec<&str> = rest.split_whitespace().collect();
                port = f.get(1).and_then(|p| p.parse::<u16>().ok());
                pts = f.iter().skip(3).filter_map(|p| p.parse::<u8>().ok()).collect();
            }
        } else if let Some(rest) = line.strip_prefix("c=") {
            let addr = rest.split_whitespace().last().and_then(|a| a.split('/').next()?.parse::<IpAddr>().ok());
            if in_audio { media_ip = addr } else { ip = addr }
        }
    }
    let ip = media_ip.or(ip).ok_or_else(|| anyhow::anyhow!("SDP has no connection address"))?;
    let port = port.ok_or_else(|| anyhow::anyhow!("SDP has no audio stream"))?;
    let codec =
        pts.iter().find_map(|pt| Codec::from_payload_type(*pt)).ok_or_else(|| anyhow::anyhow!("no common codec: offered {pts:?}, want PCMU or PCMA"))?;
    Ok(RemoteMedia { addr: SocketAddr::new(ip, port), codec, telephone_event: pts.contains(&TELEPHONE_EVENT) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_g711_codec() {
        let r = parse_remote("v=0\r\nc=IN IP4 198.51.100.20\r\nm=audio 24484 RTP/AVP 9 8 0 101\r\n").unwrap();
        assert_eq!(r.addr, "198.51.100.20:24484".parse().unwrap());
        assert_eq!(r.codec, Codec::Pcma);
        assert!(r.telephone_event);
        assert!(parse_remote("c=IN IP4 1.2.3.4\r\nm=audio 1 RTP/AVP 9\r\n").is_err());
    }

    #[test]
    fn answer_mirrors_the_offer() {
        let remote = parse_remote("c=IN IP4 1.2.3.4\r\nm=audio 4000 RTP/AVP 0\r\n").unwrap();
        let a = answer("10.0.0.2:30000".parse().unwrap(), 7, &remote);
        assert!(a.contains("m=audio 30000 RTP/AVP 0\r\n"));
        assert!(!a.contains("telephone-event"));
        let back = parse_remote(&a).unwrap();
        assert_eq!(back.codec, Codec::Pcmu);
    }
}
