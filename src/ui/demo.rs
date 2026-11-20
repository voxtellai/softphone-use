//! Demo states for Rune's headless tooling: `softphone --snap ui.png --scene in-call`.
//! Every name, number, domain and address here is made up; the IPs are documentation ranges.

use chrono::{Duration, Utc};

use super::Ui;
use crate::audio::Mode;
use crate::call::{AudioSummary, CallEvent, CallSnapshot, CallState, Level};
use crate::phone::PhoneStatus;
use crate::trace::{SdpSummary, SipMessage};

pub const SCENES: &[(&str, fn(&mut Ui))] = &[
    ("idle", idle),
    ("dialing", dialing),
    ("ringing", ringing),
    ("in-call", in_call),
    ("agent", agent),
    ("offline", offline),
    ("debug", debug),
    ("debug-ringing", debug_ringing),
    ("debug-ended", debug_ended),
];

const ME: &str = "1001";
const PBX_IP: &str = "198.51.100.20";
const MEDIA: &str = "198.51.100.21:24484";
const LOCAL: &str = "192.0.2.10:31244";

fn debug(s: &mut Ui) {
    in_call(s);
    s.open_sip.insert(3);
    s.debug = true;
}

fn debug_ringing(s: &mut Ui) {
    ringing(s);
    s.debug = true;
}

fn debug_ended(s: &mut Ui) {
    idle(s);
    s.selected = s.calls.iter().find(|c| c.to == "300").map(|c| c.id.clone());
    s.sip = sip(s.selected.as_deref().unwrap_or_default(), true);
    s.levels = levels(37);
    s.debug = true;
}

fn registered(s: &mut Ui) {
    s.status = PhoneStatus {
        device: format!("{ME}@acme"),
        proxy: "pbx.example.net:5060".into(),
        transport: "tcp".into(),
        registered: true,
        registered_at: Some(Utc::now()),
        last_error: String::new(),
        auto_answer: "manual".into(),
        active_calls: 0,
    };
}

fn call(id: &str, from: &str, to: &str, state: CallState, ago: i64) -> CallSnapshot {
    let start = Utc::now() - Duration::seconds(ago);
    CallSnapshot {
        id: id.into(),
        outbound: from == ME,
        from: from.into(),
        to: to.into(),
        state,
        mode: None,
        started_at: start,
        answered_at: None,
        ended_at: None,
        end_reason: String::new(),
        offer: SdpSummary {
            connection: PBX_IP.into(),
            port: "24484".into(),
            payloads: vec!["0".into(), "101".into()],
            codecs: vec!["PCMU/8000".into(), "telephone-event/8000".into()],
            direction: "sendrecv".into(),
        },
        codec: String::new(),
        local_rtp: String::new(),
        remote_rtp: String::new(),
        events: vec![CallEvent {
            time: start,
            message: if from == ME { format!("dialing {to}") } else { format!("INVITE from {from}, offered [\"PCMU/8000\"]") },
        }],
        audio: AudioSummary::default(),
        wav_in: String::new(),
        wav_out: String::new(),
    }
}

fn ended(id: &str, from: &str, to: &str, ago: i64, secs: i64, reason: &str) -> CallSnapshot {
    let mut c = call(id, from, to, CallState::Ended, ago);
    c.codec = "PCMU".into();
    c.mode = Some(Mode::Echo);
    c.answered_at = Some(c.started_at + Duration::seconds(3));
    c.ended_at = Some(c.started_at + Duration::seconds(3 + secs));
    c.end_reason = reason.into();
    c.local_rtp = LOCAL.into();
    c.remote_rtp = MEDIA.into();
    c.audio = AudioSummary {
        seconds_in: secs as u32,
        packets_in: secs as u32 * 50,
        seconds_with_sound_in: secs as u32 - 4,
        seconds_digital_silent: 0,
        seconds_no_packets_in: 0,
        packets_out: secs as u32 * 50,
        inbound_audio_heard: true,
    };
    c
}

fn history(s: &mut Ui) {
    let day = 86_400;
    s.calls.push(ended("2b1e9c40@192.0.2.10", ME, "300", 1800, 37, "local hangup"));
    let mut lost = call("77aa@198.51.100.20", "15550104477", ME, CallState::Ended, 3600);
    lost.end_reason = "cancelled by caller".into();
    s.calls.push(lost);
    s.calls.push(ended("8f6d6a6e@198.51.100.20", "2002", ME, 5400, 37, "remote hangup"));
    s.calls.push(ended("5c2e@192.0.2.10", ME, "15550109821", day + 3000, 72, "remote hangup"));
    s.calls.push(ended("91ab@198.51.100.20", "2003", ME, day + 9000, 245, "remote hangup"));
    s.calls.push(ended("c01d@192.0.2.10", ME, "300", 2 * day + 600, 12, "local hangup"));
    s.calls.push(ended("e4f2@198.51.100.20", "15550104477", ME, 2 * day + 5000, 150, "remote hangup"));
}

fn idle(s: &mut Ui) {
    registered(s);
    let mut last = ended("a7f3e1@198.51.100.20", "2002", ME, 300, 41, "remote hangup");
    last.mode = Some(Mode::Ai);
    s.calls.push(last);
    history(s);
    s.mode = Mode::Ai;
    s.selected = s.calls.first().map(|c| c.id.clone());
    s.levels = levels(41);
}

fn dialing(s: &mut Ui) {
    idle(s);
    s.dial = rune::TextEdit::new("555 0199");
}

fn ringing(s: &mut Ui) {
    registered(s);
    s.calls.push(call("a7f3e1@198.51.100.20", "2002", ME, CallState::Ringing, 4));
    history(s);
    s.mode = Mode::Ai;
    s.selected = s.calls.first().map(|c| c.id.clone());
    s.sip = sip("a7f3e1@198.51.100.20", false).into_iter().take(3).collect();
}

fn in_call(s: &mut Ui) {
    registered(s);
    let mut c = call("a7f3e1@198.51.100.20", "2002", ME, CallState::Answered, 29);
    c.mode = Some(Mode::Ai);
    c.codec = "PCMU".into();
    c.answered_at = Some(c.started_at + Duration::seconds(3));
    c.local_rtp = LOCAL.into();
    c.remote_rtp = MEDIA.into();
    c.audio = AudioSummary {
        seconds_in: 23,
        packets_in: 1150,
        seconds_with_sound_in: 19,
        seconds_digital_silent: 0,
        seconds_no_packets_in: 0,
        packets_out: 1150,
        inbound_audio_heard: true,
    };
    let t = c.started_at;
    for (dt, m) in [
        (3, format!("answered as ai, codec PCMU, rtp {LOCAL} <-> {MEDIA}")),
        (4, "ai session started (voice ara)".to_string()),
        (6, "ai said: Hi, this is Ara at the front desk.".to_string()),
        (11, "heard: Hi, I have Sam on the line about an invoice. Can you take it?".to_string()),
        (14, "ai said: Sure, put them through.".to_string()),
    ] {
        c.events.push(CallEvent { time: t + Duration::seconds(dt), message: m });
    }
    s.calls.push(c);
    history(s);
    s.selected = s.calls.first().map(|c| c.id.clone());
    s.mode = Mode::Ai;
    s.live_db = (Some(-21.0), Some(-18.0));
    s.live = (super::glow(s.live_db.0), super::glow(s.live_db.1));
    for i in 0..140 {
        let t = i as f32 * 0.06;
        s.lanes.0.push_back(speech(t, 0.0));
        s.lanes.1.push_back(speech(t + 1.7, 3.0));
    }
    s.levels = levels(26);
    s.sip = sip("a7f3e1@198.51.100.20", false);
}

fn agent(s: &mut Ui) {
    dialing(s);
    s.agent_open = true;
    s.voice = "eve";
}

fn offline(s: &mut Ui) {
    idle(s);
    s.status.registered = false;
    s.status.last_error = format!("dial tcp {PBX_IP}:5060: i/o timeout");
}

/// Speech-like levels: syllables, with pauses between phrases.
fn speech(t: f32, seed: f32) -> f32 {
    let s = (t * 2.1 + seed).sin() * 0.5 + (t * 5.3 + seed * 2.0).sin() * 0.3 + (t * 11.7 + seed).sin() * 0.2;
    let gate = if (t * 0.6 + seed).sin() > -0.25 { 1.0 } else { 0.1 };
    ((0.48 + s * 0.45) * gate + ((t * 37.0).sin() * 0.5 + 0.5) * 0.06).clamp(0.04, 0.97)
}

fn levels(n: u32) -> (Vec<Level>, Vec<Level>) {
    let wave = |i: u32, k: f64| Level {
        second: i + 1,
        dbfs: Some(if i < 3 { -90.0 + i as f64 * 18.0 } else { -38.0 + 13.0 * ((i as f64 + k) * 0.7).sin() + 4.0 * (i as f64 * 2.3 + k).sin() }),
        packets: 50,
    };
    ((0..n).map(|i| wave(i, 0.0)).collect(), (0..n).map(|i| wave(i, 2.1)).collect())
}

fn sip(call_id: &str, with_bye: bool) -> Vec<SipMessage> {
    let t = Utc::now() - Duration::seconds(29);
    let sdp = "v=0\r\no=- 1 1 IN IP4 192.0.2.10\r\nc=IN IP4 192.0.2.10\r\nm=audio 31244 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\n";
    let mut msgs = vec![
        ("in", "INVITE sip:1001@192.0.2.10:50612;transport=tcp SIP/2.0", "1 INVITE", 0, true),
        ("out", "SIP/2.0 100 Trying", "1 INVITE", 7, false),
        ("out", "SIP/2.0 180 Ringing", "1 INVITE", 9, false),
        ("out", "SIP/2.0 200 OK", "1 INVITE", 2892, true),
        ("in", "ACK sip:1001@192.0.2.10:50612;transport=tcp SIP/2.0", "1 ACK", 2949, false),
    ];
    if with_bye {
        msgs.push(("out", "BYE sip:2002@198.51.100.20:5060;transport=tcp SIP/2.0", "2 BYE", 40_000, false));
        msgs.push(("in", "SIP/2.0 200 OK", "2 BYE", 40_031, false));
    }
    msgs.into_iter()
        .map(|(dir, first, cseq, ms, has_sdp)| SipMessage {
            time: t + Duration::milliseconds(ms),
            direction: dir.into(),
            remote: format!("{PBX_IP}:5060"),
            first_line: first.into(),
            call_id: call_id.into(),
            cseq: cseq.into(),
            sdp: if has_sdp { sdp.into() } else { String::new() },
            raw: format!(
                "{first}\r\nVia: SIP/2.0/TCP {PBX_IP}:5060;branch=z9hG4bK-4c2e\r\nTo: <sip:1001@pbx.example.net>;tag=5d19a7\r\nCall-ID: {call_id}\r\nCSeq: {cseq}\r\n{}\r\n{}",
                if has_sdp { format!("Content-Type: application/sdp\r\nContent-Length: {}", sdp.len()) } else { "Content-Length: 0".into() },
                if has_sdp { sdp } else { "" }
            ),
        })
        .collect()
}
