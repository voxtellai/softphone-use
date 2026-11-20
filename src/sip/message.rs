//! SIP messages: parsing, building and the header lookups the user agent needs.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    Request { method: String, uri: String },
    Response { code: u16, reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SipMsg {
    pub start: Start,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Compact header forms (RFC 3261 §7.3.3) mapped to their full names.
fn canonical(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "i" => "call-id",
        "m" => "contact",
        "f" => "from",
        "t" => "to",
        "v" => "via",
        "l" => "content-length",
        "c" => "content-type",
        "k" => "supported",
        _ => return lower,
    }
    .to_string()
}

impl SipMsg {
    pub fn request(method: &str, uri: &str) -> SipMsg {
        SipMsg { start: Start::Request { method: method.into(), uri: uri.into() }, headers: Vec::new(), body: Vec::new() }
    }

    pub fn parse(bytes: &[u8]) -> anyhow::Result<SipMsg> {
        let end = crate::trace::find(bytes, b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no end of headers"))?;
        let head = std::str::from_utf8(&bytes[..end])?;
        let body = bytes[end + 4..].to_vec();
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or_default();
        let start = if let Some(rest) = first.strip_prefix("SIP/2.0 ") {
            let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            Start::Response { code: code.parse()?, reason: reason.to_string() }
        } else {
            let mut parts = first.split(' ');
            let (Some(method), Some(uri), Some("SIP/2.0")) = (parts.next(), parts.next(), parts.next()) else {
                anyhow::bail!("bad start line {first:?}");
            };
            Start::Request { method: method.into(), uri: uri.into() }
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if line.starts_with([' ', '\t']) {
                // Folded continuation of the previous header.
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
                continue;
            }
            let Some((name, value)) = line.split_once(':') else { continue };
            let name = canonical(name);
            // Via and Contact may carry several values in one header; split them so each one
            // is its own entry. Other comma lists stay whole.
            if name == "via" || name == "record-route" || name == "route" {
                for v in split_commas(value) {
                    headers.push((name.clone(), v.trim().to_string()));
                }
            } else {
                headers.push((name, value.trim().to_string()));
            }
        }
        Ok(SipMsg { start, headers, body })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = String::new();
        match &self.start {
            Start::Request { method, uri } => writeln_crlf(&mut s, &format!("{method} {uri} SIP/2.0")),
            Start::Response { code, reason } => writeln_crlf(&mut s, &format!("SIP/2.0 {code} {reason}")),
        }
        for (name, value) in &self.headers {
            if name == "content-length" {
                continue;
            }
            writeln_crlf(&mut s, &format!("{}: {value}", display_name(name)));
        }
        writeln_crlf(&mut s, &format!("Content-Length: {}", self.body.len()));
        s.push_str("\r\n");
        let mut out = s.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }

    pub fn method(&self) -> &str {
        match &self.start {
            Start::Request { method, .. } => method,
            Start::Response { .. } => self.cseq().1,
        }
    }

    pub fn code(&self) -> Option<u16> {
        match self.start {
            Start::Response { code, .. } => Some(code),
            Start::Request { .. } => None,
        }
    }

    pub fn uri(&self) -> &str {
        match &self.start {
            Start::Request { uri, .. } => uri,
            Start::Response { .. } => "",
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = canonical(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    pub fn headers_all(&self, name: &str) -> Vec<&str> {
        let name = canonical(name);
        self.headers.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect()
    }

    pub fn add(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.headers.push((canonical(name), value.into()));
        self
    }

    pub fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        let name = canonical(name);
        let value = value.into();
        match self.headers.iter_mut().find(|(n, _)| *n == name) {
            Some(h) => h.1 = value,
            None => self.headers.push((name, value)),
        }
        self
    }

    pub fn remove(&mut self, name: &str) {
        let name = canonical(name);
        self.headers.retain(|(n, _)| *n != name);
    }

    pub fn call_id(&self) -> &str {
        self.header("call-id").unwrap_or_default()
    }

    /// The CSeq number and method.
    pub fn cseq(&self) -> (u32, &str) {
        let v = self.header("cseq").unwrap_or_default();
        let (n, m) = v.split_once(' ').unwrap_or((v, ""));
        (n.trim().parse().unwrap_or(0), m.trim())
    }

    /// The branch of the top Via.
    pub fn branch(&self) -> &str {
        self.header("via").and_then(|v| param(v, "branch")).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn from_tag(&self) -> &str {
        self.header("from").and_then(|v| param(v, "tag")).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn is_sdp(&self) -> bool {
        self.header("content-type").is_some_and(|v| v.to_ascii_lowercase().starts_with("application/sdp"))
    }

    pub fn to_tag(&self) -> &str {
        self.header("to").and_then(|v| param(v, "tag")).unwrap_or_default()
    }

    /// The URI in the Contact header, without angle brackets.
    pub fn contact_uri(&self) -> Option<String> {
        self.header("contact").map(name_addr_uri)
    }

    /// A response to this request with Via, From, To, Call-ID and CSeq copied (RFC 3261 §8.2.6).
    /// `to_tag` is added to To when it has none.
    pub fn response(&self, code: u16, reason: &str, to_tag: Option<&str>) -> SipMsg {
        let mut r = SipMsg { start: Start::Response { code, reason: reason.into() }, headers: Vec::new(), body: Vec::new() };
        for v in self.headers_all("via") {
            r.add("via", v);
        }
        if let Some(f) = self.header("from") {
            r.add("from", f);
        }
        if let Some(t) = self.header("to") {
            let to = match to_tag {
                Some(tag) if param(t, "tag").is_none() => format!("{t};tag={tag}"),
                _ => t.to_string(),
            };
            r.add("to", to);
        }
        r.add("call-id", self.call_id());
        if let Some(c) = self.header("cseq") {
            r.add("cseq", c);
        }
        if (101..300).contains(&code) {
            for rr in self.headers_all("record-route") {
                r.add("record-route", rr);
            }
        }
        r
    }

    /// The first line, for logs and events.
    pub fn summary(&self) -> String {
        match &self.start {
            Start::Request { method, uri } => format!("{method} {uri}"),
            Start::Response { code, reason } => format!("{code} {reason}"),
        }
    }
}

fn writeln_crlf(s: &mut String, line: &str) {
    let _ = write!(s, "{line}\r\n");
}

fn display_name(canonical: &str) -> String {
    match canonical {
        "call-id" => "Call-ID".into(),
        "cseq" => "CSeq".into(),
        "www-authenticate" => "WWW-Authenticate".into(),
        _ => canonical
            .split('-')
            .map(|w| {
                let mut c = w.chars();
                c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("-"),
    }
}

/// Splits on commas outside quotes and angle brackets.
pub fn split_commas(v: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut angle) = (Vec::new(), String::new(), false, false);
    for c in v.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// A `;name=value` parameter of a header value, outside its `<uri>`.
pub fn param<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let params = match value.rfind('>') {
        Some(i) => &value[i + 1..],
        None => value,
    };
    params.split(';').skip(if value.contains('>') { 0 } else { 1 }).find_map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

/// The URI of a name-addr (`"Name" <sip:x@y>;tag=1`) or addr-spec (`sip:x@y;tag=1`).
pub fn name_addr_uri(v: &str) -> String {
    match (v.find('<'), v.find('>')) {
        (Some(a), Some(b)) if a < b => v[a + 1..b].to_string(),
        _ => v.split(';').next().unwrap_or(v).trim().to_string(),
    }
}

/// The user part of a SIP URI or name-addr.
pub fn uri_user(v: &str) -> String {
    let uri = name_addr_uri(v);
    let rest = uri.split_once(':').map(|(_, r)| r).unwrap_or(&uri);
    rest.split_once('@').map(|(u, _)| u.to_string()).unwrap_or_default()
}

pub fn random_token(n: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..n).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

pub fn new_branch() -> String {
    format!("z9hG4bK{}", random_token(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "INVITE sip:1001@10.0.0.2:5060;transport=tcp SIP/2.0\r\n\
        Via: SIP/2.0/TCP 198.51.100.20:5060;branch=z9hG4bKabc;rport, SIP/2.0/TCP 10.1.1.1;branch=z9hG4bKdef\r\n\
        Record-Route: <sip:198.51.100.20:5060;transport=tcp;lr>\r\n\
        From: \"Front desk\" <sip:2002@example>;tag=from1\r\n\
        t: <sip:1001@example>\r\n\
        i: call-1@host\r\n\
        CSeq: 101 INVITE\r\n\
        m: <sip:2002@198.51.100.20:5060;transport=tcp>\r\n\
        Content-Type: application/sdp\r\n\
        Content-Length: 4\r\n\r\nv=0\n";

    #[test]
    fn parses_requests_with_compact_and_multi_value_headers() {
        let m = SipMsg::parse(INVITE.as_bytes()).unwrap();
        assert_eq!(m.method(), "INVITE");
        assert_eq!(m.call_id(), "call-1@host");
        assert_eq!(m.cseq(), (101, "INVITE"));
        assert_eq!(m.branch(), "z9hG4bKabc");
        assert_eq!(m.headers_all("via").len(), 2);
        assert_eq!(m.from_tag(), "from1");
        assert_eq!(m.to_tag(), "");
        assert_eq!(m.contact_uri().unwrap(), "sip:2002@198.51.100.20:5060;transport=tcp");
        assert_eq!(uri_user(m.header("from").unwrap()), "2002");
        assert!(m.is_sdp());
        assert_eq!(m.body, b"v=0\n");
    }

    #[test]
    fn responses_copy_dialog_headers_and_add_the_to_tag() {
        let m = SipMsg::parse(INVITE.as_bytes()).unwrap();
        let r = m.response(180, "Ringing", Some("me1"));
        assert_eq!(r.headers_all("via").len(), 2);
        assert_eq!(r.to_tag(), "me1");
        assert_eq!(r.header("record-route").unwrap(), "<sip:198.51.100.20:5060;transport=tcp;lr>");
        let bytes = r.to_bytes();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("SIP/2.0 180 Ringing\r\n"));
        assert!(text.contains("Call-ID: call-1@host\r\n"));
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"));
        let back = SipMsg::parse(&bytes).unwrap();
        assert_eq!(back.code(), Some(180));
        assert_eq!(back.cseq(), (101, "INVITE"));
    }

    #[test]
    fn params_ignore_the_uri() {
        assert_eq!(param("<sip:a@b;tag=wrong>;tag=right", "tag"), Some("right"));
        assert_eq!(param("SIP/2.0/UDP 1.2.3.4;branch=z9hG4bK1;rport", "branch"), Some("z9hG4bK1"));
        assert_eq!(param("SIP/2.0/UDP 1.2.3.4;branch=z9hG4bK1;rport", "rport"), Some(""));
        assert_eq!(param("<sip:a@b>", "tag"), None);
    }
}
