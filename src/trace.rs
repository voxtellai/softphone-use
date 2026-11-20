//! The SIP trace: every message as it crossed the wire, and a summary of SDP bodies.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One SIP request or response as it crossed the wire.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SipMessage {
    pub time: DateTime<Utc>,
    /// "in" or "out".
    pub direction: String,
    pub remote: String,
    pub first_line: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub cseq: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sdp: String,
    pub raw: String,
}

const MAX_TRACE_MESSAGES: usize = 5000;

/// Stream transports deliver arbitrary chunks, so bytes are buffered per connection and
/// direction and split on Content-Length.
#[derive(Default)]
pub struct SipTrace {
    inner: Mutex<TraceInner>,
}

#[derive(Default)]
struct TraceInner {
    buffers: HashMap<String, Vec<u8>>,
    messages: Vec<SipMessage>,
}

impl SipTrace {
    pub fn read(&self, transport: &str, remote: &str, chunk: &[u8]) {
        self.feed("in", transport, remote, chunk);
    }

    pub fn write(&self, transport: &str, remote: &str, chunk: &[u8]) {
        self.feed("out", transport, remote, chunk);
    }

    fn feed(&self, dir: &str, transport: &str, remote: &str, chunk: &[u8]) {
        let mut g = self.inner.lock().unwrap();
        let key = format!("{dir}|{transport}|{remote}");
        let mut buf = g.buffers.remove(&key).unwrap_or_default();
        buf.extend_from_slice(chunk);
        while let Some((msg, rest)) = split_sip_message(&buf) {
            if let Some(mut parsed) = parse_sip_message(&msg) {
                parsed.time = Utc::now();
                parsed.direction = dir.to_string();
                parsed.remote = remote.to_string();
                g.messages.push(parsed);
                let over = g.messages.len().saturating_sub(MAX_TRACE_MESSAGES);
                g.messages.drain(..over);
            }
            buf = rest;
        }
        if !buf.is_empty() {
            g.buffers.insert(key, buf);
        }
    }

    /// The messages for one Call-ID, oldest first.
    pub fn for_call(&self, call_id: &str) -> Vec<SipMessage> {
        self.inner.lock().unwrap().messages.iter().filter(|m| m.call_id == call_id).cloned().collect()
    }

    /// The last n messages, oldest first; 0 means all.
    pub fn recent(&self, n: usize) -> Vec<SipMessage> {
        let g = self.inner.lock().unwrap();
        let n = if n == 0 || n > g.messages.len() { g.messages.len() } else { n };
        g.messages[g.messages.len() - n..].to_vec()
    }
}

/// The first complete message in buf and the bytes after it. Keepalive CRLFs before a message
/// are dropped.
pub fn split_sip_message(buf: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let start = buf.iter().position(|&b| b != b'\r' && b != b'\n')?;
    let buf = &buf[start..];
    let end = find(buf, b"\r\n\r\n")?;
    let header_end = end + 4;
    let length = content_length(&String::from_utf8_lossy(&buf[..end]));
    if buf.len() < header_end + length {
        return None;
    }
    Some((buf[..header_end + length].to_vec(), buf[header_end + length..].to_vec()))
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn content_length(headers: &str) -> usize {
    for line in headers.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" || name == "l" {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

pub fn parse_sip_message(msg: &[u8]) -> Option<SipMessage> {
    let text = String::from_utf8_lossy(msg).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let first = lines.next().filter(|l| !l.is_empty())?;
    let mut out = SipMessage { first_line: first.to_string(), ..Default::default() };
    let mut is_sdp = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "call-id" | "i" => out.call_id = value.to_string(),
            "cseq" => out.cseq = value.to_string(),
            "content-type" | "c" => is_sdp = value.to_ascii_lowercase().starts_with("application/sdp"),
            _ => {}
        }
    }
    if is_sdp {
        out.sdp = body.to_string();
    }
    out.raw = text.clone();
    Some(out)
}

/// The media address and offered codecs of an SDP body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SdpSummary {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connection: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub port: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codecs: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub direction: String,
}

pub fn summarize_sdp(sdp: &str) -> SdpSummary {
    let mut s = SdpSummary::default();
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("c=") {
            if let Some(last) = line.split_whitespace().last() {
                s.connection = last.to_string();
            }
        } else if line.starts_with("m=audio") {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4 {
                s.port = fields[1].to_string();
                s.payloads = fields[3..].iter().map(|f| f.to_string()).collect();
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((_, codec)) = rest.split_once(' ') {
                s.codecs.push(codec.to_string());
            }
        } else if matches!(line, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive") {
            s.direction = line[2..].to_string();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE_SDP: &str = "v=0\r\nc=IN IP4 198.51.100.20\r\nm=audio 24484 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

    fn invite() -> String {
        format!(
            "INVITE sip:1001@10.0.0.2:5060;transport=tcp SIP/2.0\r\nCall-ID: abc123\r\nCSeq: 1 INVITE\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{INVITE_SDP}",
            INVITE_SDP.len()
        )
    }

    #[test]
    fn splits_chunked_tcp() {
        let tr = SipTrace::default();
        let options = "OPTIONS sip:x SIP/2.0\r\nCall-ID: keepalive\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n";
        let stream = format!("\r\n\r\n{}{options}", invite());
        for chunk in stream.as_bytes().chunks(17) {
            tr.read("TCP", "r:1", chunk);
        }
        let got = tr.recent(0);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].call_id.as_str(), got[0].cseq.as_str(), got[0].direction.as_str()), ("abc123", "1 INVITE", "in"));
        assert!(got[0].sdp.contains("m=audio 24484"));
        assert_eq!(got[1].call_id, "keepalive");
        assert!(got[1].sdp.is_empty());
        assert_eq!(tr.for_call("abc123").len(), 1);
    }

    #[test]
    fn waits_for_body() {
        let tr = SipTrace::default();
        tr.write("TCP", "r", b"SIP/2.0 200 OK\r\nCall-ID: x\r\nContent-Length: 10\r\n\r\n12345");
        assert!(tr.recent(0).is_empty(), "emitted before the body arrived");
        tr.write("TCP", "r", b"67890");
        let got = tr.recent(0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].direction, "out");
    }

    #[test]
    fn summarizes_sdp() {
        let s = summarize_sdp(
            "v=0\r\nc=IN IP4 198.51.100.20\r\nm=audio 24484 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        );
        assert_eq!((s.connection.as_str(), s.port.as_str(), s.direction.as_str()), ("198.51.100.20", "24484", "sendrecv"));
        assert_eq!(s.codecs, vec!["PCMU/8000", "telephone-event/8000"]);
    }
}
