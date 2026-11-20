//! HTTP digest authentication for SIP (RFC 2617, MD5, with and without `qop=auth`).

use md5::{Digest, Md5};

use super::message::{random_token, split_commas};

#[derive(Debug, Default, PartialEq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
    pub qop: Option<String>,
}

pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let rest = header.trim().strip_prefix("Digest").or_else(|| header.trim().strip_prefix("digest"))?;
    let mut c = Challenge::default();
    for part in split_commas(rest) {
        let Some((k, v)) = part.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "realm" => c.realm = v,
            "nonce" => c.nonce = v,
            "opaque" => c.opaque = Some(v),
            "algorithm" => c.algorithm = Some(v),
            "qop" => c.qop = Some(v),
            _ => {}
        }
    }
    (!c.nonce.is_empty()).then_some(c)
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The Authorization (or Proxy-Authorization) value answering `c`.
pub fn authorization(c: &Challenge, user: &str, password: &str, method: &str, uri: &str) -> String {
    authorization_with(c, user, password, method, uri, &random_token(16), 1)
}

pub fn authorization_with(c: &Challenge, user: &str, password: &str, method: &str, uri: &str, cnonce: &str, nc: u32) -> String {
    let ha1 = md5_hex(&format!("{user}:{}:{password}", c.realm));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let auth_qop = c.qop.as_deref().and_then(|q| q.split(',').map(str::trim).find(|q| *q == "auth"));
    let mut out = format!("Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\"", c.realm, c.nonce);
    let response = match auth_qop {
        Some(qop) => {
            let nc = format!("{nc:08x}");
            let r = md5_hex(&format!("{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}", c.nonce));
            out.push_str(&format!(", qop={qop}, nc={nc}, cnonce=\"{cnonce}\""));
            r
        }
        None => md5_hex(&format!("{ha1}:{}:{ha2}", c.nonce)),
    };
    out.push_str(&format!(", response=\"{response}\", algorithm=MD5"));
    if let Some(o) = &c.opaque {
        out.push_str(&format!(", opaque=\"{o}\""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2617_example() {
        // The worked example from RFC 2617 §3.5.
        let c = parse_challenge(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        let a = authorization_with(&c, "Mufasa", "Circle Of Life", "GET", "/dir/index.html", "0a4f113b", 1);
        assert!(a.contains(r#"response="6629fae49393a05397450978507c4ef1""#), "{a}");
        assert!(a.contains("nc=00000001") && a.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
    }

    #[test]
    fn without_qop() {
        let c = parse_challenge(r#"Digest realm="example", nonce="abc", algorithm=MD5"#).unwrap();
        let a = authorization_with(&c, "1001", "pw", "REGISTER", "sip:example", "x", 1);
        let ha1 = md5_hex("1001:example:pw");
        let ha2 = md5_hex("REGISTER:sip:example");
        assert!(a.contains(&md5_hex(&format!("{ha1}:abc:{ha2}"))));
        assert!(!a.contains("qop"));
    }
}
