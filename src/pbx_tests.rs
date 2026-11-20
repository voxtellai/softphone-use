//! The phone against a fake PBX on localhost, over UDP and TCP: a challenged REGISTER, an
//! incoming call answered with a tone, a challenged outgoing call, and BYE in both directions.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::audio::Mode;
use crate::call::CallState;
use crate::config::Config;
use crate::phone::Phone;
use crate::sip::digest;
use crate::sip::message::{SipMsg, new_branch, param};
use crate::trace::split_sip_message;

enum Wire {
    Udp(UdpSocket, Option<SocketAddr>),
    Tcp(TcpListener, Option<TcpStream>, Vec<u8>),
}

struct Pbx {
    wire: Wire,
    addr: SocketAddr,
    via: String,
}

impl Pbx {
    async fn new(udp: bool) -> Pbx {
        let wire = if udp {
            Wire::Udp(UdpSocket::bind("127.0.0.1:0").await.unwrap(), None)
        } else {
            Wire::Tcp(TcpListener::bind("127.0.0.1:0").await.unwrap(), None, Vec::new())
        };
        let addr = match &wire {
            Wire::Udp(s, _) => s.local_addr().unwrap(),
            Wire::Tcp(l, ..) => l.local_addr().unwrap(),
        };
        let via = if udp { "UDP" } else { "TCP" }.to_string();
        Pbx { wire, addr, via }
    }

    async fn recv(&mut self) -> SipMsg {
        tokio::time::timeout(Duration::from_secs(5), self.recv_inner()).await.expect("PBX waited 5 s for a message")
    }

    async fn recv_inner(&mut self) -> SipMsg {
        match &mut self.wire {
            Wire::Udp(s, peer) => {
                let mut buf = vec![0u8; 65535];
                let (n, from) = s.recv_from(&mut buf).await.unwrap();
                *peer = Some(from);
                SipMsg::parse(&buf[..n]).unwrap()
            }
            Wire::Tcp(l, conn, buf) => loop {
                if let Some((msg, rest)) = split_sip_message(buf) {
                    *buf = rest;
                    return SipMsg::parse(&msg).unwrap();
                }
                if conn.is_none() {
                    *conn = Some(l.accept().await.unwrap().0);
                }
                let mut chunk = [0u8; 8192];
                let n = conn.as_mut().unwrap().read(&mut chunk).await.unwrap();
                assert!(n > 0, "phone closed the connection");
                buf.extend_from_slice(&chunk[..n]);
            },
        }
    }

    /// The next request with `method`, answering OPTIONS-free noise such as keepalives.
    async fn expect(&mut self, method: &str) -> SipMsg {
        let m = self.recv().await;
        assert_eq!(m.method(), method, "got {}", m.summary());
        m
    }

    async fn send(&mut self, m: &SipMsg) {
        let bytes = m.to_bytes();
        match &mut self.wire {
            Wire::Udp(s, peer) => {
                s.send_to(&bytes, peer.expect("phone spoke first")).await.unwrap();
            }
            Wire::Tcp(_, conn, _) => conn.as_mut().expect("phone connected first").write_all(&bytes).await.unwrap(),
        }
    }

    fn request(&self, method: &str, uri: &str, call_id: &str, cseq: u32, to_tag: Option<&str>) -> SipMsg {
        let mut m = SipMsg::request(method, uri);
        m.add("via", format!("SIP/2.0/{} {};branch={}", self.via, self.addr, new_branch()))
            .add("record-route", format!("<sip:{};lr>", self.addr))
            .add("from", "<sip:2002@example>;tag=pbx1")
            .add(
                "to",
                match to_tag {
                    Some(t) => format!("<sip:1001@example>;tag={t}"),
                    None => "<sip:1001@example>".into(),
                },
            )
            .add("call-id", call_id)
            .add("cseq", format!("{cseq} {method}"))
            .add("contact", format!("<sip:2002@{}>", self.addr));
        m
    }
}

fn config(proxy: SocketAddr, transport: &str, dir: &std::path::Path) -> Config {
    Config {
        user: "1001".into(),
        password: "secret".into(),
        domain: "example".into(),
        proxy: proxy.to_string(),
        transport: transport.into(),
        http_addr: "127.0.0.1:0".into(),
        data_dir: dir.into(),
        xai_key: String::new(),
        xai_model: String::new(),
        xai_voice: "ara".into(),
        auto_answer: None,
        auto_answer_after: Duration::from_secs(2),
        ring_timeout: Duration::from_secs(20),
    }
}

/// Checks the Authorization the phone sent against the challenge.
fn check_auth(req: &SipMsg, header: &str, challenge: &str) {
    let auth = req.header(header).unwrap_or_else(|| panic!("{} without {header}", req.summary()));
    let field = |s: &str, k: &str| {
        let rest = s.split(&format!("{k}=")).nth(1)?;
        Some(rest.trim_start_matches('"').split(['"', ',']).next()?.to_string())
    };
    let c = digest::parse_challenge(challenge).unwrap();
    let nc = field(auth, "nc").map_or(0, |n| u32::from_str_radix(&n, 16).unwrap());
    let cnonce = field(auth, "cnonce").unwrap_or_default();
    let want = digest::authorization_with(&c, "1001", "secret", req.method(), &field(auth, "uri").unwrap(), &cnonce, nc);
    assert_eq!(field(auth, "response"), field(&want, "response"), "digest response");
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn rtp_socket() -> (UdpSocket, u16) {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    (s, port)
}

fn sdp(port: u16, pts: &str) -> Vec<u8> {
    format!("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {port} RTP/AVP {pts}\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n").into_bytes()
}

async fn full_session(udp: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut pbx = Pbx::new(udp).await;
    let phone = Phone::new(config(pbx.addr, if udp { "udp" } else { "tcp" }, dir.path()), tokio::runtime::Handle::current());
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let run = tokio::spawn(phone.clone().run(stop));

    // REGISTER: challenge, then accept.
    let reg = pbx.expect("REGISTER").await;
    assert_eq!(reg.uri(), "sip:example");
    assert_eq!(reg.header("user-agent"), Some(crate::sip::ua::USER_AGENT));
    let contact = reg.contact_uri().unwrap();
    assert!(contact.contains(&format!("transport={}", if udp { "udp" } else { "tcp" })), "{contact}");
    let challenge = "Digest realm=\"example\", nonce=\"n1\", algorithm=MD5";
    let mut r = reg.response(401, "Unauthorized", None);
    r.add("www-authenticate", challenge);
    pbx.send(&r).await;
    let reg2 = pbx.expect("REGISTER").await;
    check_auth(&reg2, "authorization", challenge);
    assert_eq!(reg2.call_id(), reg.call_id(), "a registration keeps its Call-ID");
    assert!(reg2.cseq().0 > reg.cseq().0);
    let mut ok = reg2.response(200, "OK", Some("reg"));
    ok.add("contact", format!("<{contact}>;expires=300"));
    pbx.send(&ok).await;
    wait_until("registration", || phone.status().registered).await;

    // OPTIONS gets 200.
    let opt = pbx.request("OPTIONS", &contact, "opt-1", 1, None);
    pbx.send(&opt).await;
    assert_eq!(pbx.recv().await.code(), Some(200));

    // Incoming call, answered with a tone.
    let (media, media_port) = rtp_socket().await;
    let mut invite = pbx.request("INVITE", &contact, "in-1", 1, None);
    invite.add("content-type", "application/sdp");
    invite.body = sdp(media_port, "0 101");
    pbx.send(&invite).await;
    assert_eq!(pbx.recv().await.code(), Some(100));
    let ringing = pbx.recv().await;
    assert_eq!(ringing.code(), Some(180));
    let tag = ringing.to_tag().to_string();
    assert!(!tag.is_empty());
    wait_until("the ringing call", || phone.call("in-1").is_some_and(|c| c.state() == CallState::Ringing)).await;
    phone.answer("in-1", Mode::Tone).unwrap();
    let ok = pbx.recv().await;
    assert_eq!(ok.code(), Some(200));
    assert_eq!(ok.to_tag(), tag, "the 200 keeps the 180's tag");
    assert!(ok.header("record-route").is_some(), "dialog-creating responses copy Record-Route");
    let answer = crate::sip::sdp::parse_remote(&String::from_utf8_lossy(&ok.body)).unwrap();
    let mut ack = pbx.request("ACK", &contact, "in-1", 1, Some(&tag));
    ack.remove("content-type");
    pbx.send(&ack).await;

    // The tone arrives as PCMU, and our audio reaches the call's levels.
    let mut buf = [0u8; 1500];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), media.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(from.port(), answer.addr.port());
    let (pt, payload) = crate::rtp::parse(&buf[..n]).unwrap();
    assert_eq!((pt, payload.len()), (0, 160));
    assert!(payload.iter().any(|b| *b != 0xff), "tone, not silence");
    let mut pkt = vec![0x80, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7];
    pkt.extend([0x20u8; 160]);
    for _ in 0..60 {
        media.send_to(&pkt, answer.addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let call = phone.call("in-1").unwrap();
    wait_until("a second of levels", || !call.levels().0.is_empty()).await;
    assert!(call.snapshot().audio.inbound_audio_heard);

    // The far end hangs up.
    let bye = pbx.request("BYE", &contact, "in-1", 2, Some(&tag));
    pbx.send(&bye).await;
    assert_eq!(pbx.recv().await.code(), Some(200));
    wait_until("the call to end", || call.state() == CallState::Ended).await;
    assert_eq!(call.snapshot().end_reason, "remote hangup");
    let saved = dir.path().join("in-1").join("call.json");
    wait_until("the call to be saved", || saved.exists()).await;

    // Outgoing call: challenged, answered, then hung up by us.
    let out = phone.dial("200", Mode::Echo, "", "").unwrap();
    let inv = pbx.expect("INVITE").await;
    assert!(inv.uri().starts_with("sip:200@"), "{}", inv.uri());
    let challenge = "Digest realm=\"example\", nonce=\"n2\", qop=\"auth\"";
    let mut c = inv.response(407, "Proxy Authentication Required", Some("x"));
    c.add("proxy-authenticate", challenge);
    pbx.send(&c).await;
    let ack = pbx.expect("ACK").await;
    assert_eq!(ack.branch(), inv.branch(), "the ACK for an error is in the INVITE transaction");
    let inv2 = pbx.expect("INVITE").await;
    check_auth(&inv2, "proxy-authorization", challenge);
    assert_eq!(inv2.cseq().0, inv.cseq().0 + 1);
    let offer = crate::sip::sdp::parse_remote(&String::from_utf8_lossy(&inv2.body)).unwrap();
    pbx.send(&inv2.response(180, "Ringing", Some("far"))).await;
    let (media2, port2) = rtp_socket().await;
    let mut ok = inv2.response(200, "OK", Some("far"));
    ok.add("contact", format!("<sip:200@{}>", pbx.addr)).add("content-type", "application/sdp");
    ok.body = sdp(port2, "8");
    pbx.send(&ok).await;
    let ack = pbx.expect("ACK").await;
    assert_ne!(ack.branch(), inv2.branch(), "the ACK for a 200 is its own transaction");
    assert_eq!(ack.to_tag(), "far");
    assert_eq!(ack.uri(), format!("sip:200@{}", pbx.addr), "in-dialog requests go to the remote target");
    wait_until("the outgoing call to connect", || out.state() == CallState::Answered).await;
    assert_eq!(out.snapshot().codec, "PCMA");
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), media2.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(from.port(), offer.addr.port());
    assert_eq!(crate::rtp::parse(&buf[..n]).unwrap().0, 8);
    let events: Vec<String> = out.snapshot().events.into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|e| e == "407 Proxy Authentication Required"), "{events:?}");

    // Switching what talks mid-call keeps the call up.
    phone.answer(&out.id, Mode::Tone).unwrap();
    phone.answer(&out.id, Mode::Silent).unwrap();
    phone.answer(&out.id, Mode::Echo).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(out.state(), CallState::Answered, "{:?}", out.snapshot().events);

    phone.hangup(&out.id, "test").unwrap();
    let bye = pbx.expect("BYE").await;
    assert_eq!(bye.to_tag(), "far");
    assert!(bye.header("route").is_none(), "the PBX did not record-route this dialog");
    pbx.send(&bye.response(200, "OK", None)).await;
    wait_until("the outgoing call to end", || out.state() == CallState::Ended).await;
    assert_eq!(out.snapshot().end_reason, "local hangup");

    stop_tx.send_replace(true);
    let unreg = pbx.expect("REGISTER").await;
    assert_eq!(unreg.header("expires"), Some("0"));
    pbx.send(&unreg.response(200, "OK", Some("reg"))).await;
    tokio::time::timeout(Duration::from_secs(6), run).await.unwrap().unwrap().unwrap();
    let _: Arc<Phone> = phone;
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_over_udp() {
    full_session(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_over_tcp() {
    full_session(false).await;
}

#[test]
fn param_helper_sees_expires() {
    assert_eq!(param("<sip:1001@1.2.3.4>;expires=300", "expires"), Some("300"));
}
