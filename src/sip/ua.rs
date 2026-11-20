//! The user agent: client and server transactions, registration and dialogs.
//!
//! Everything is sent to the proxy (an outbound proxy, as a desk phone uses one), and responses go
//! back to wherever their request came from.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::digest;
use super::message::{SipMsg, new_branch, param, random_token};
use super::transport::{Incoming, Transport};

/// Sent on every request; most PBXs show it on the device.
pub const USER_AGENT: &str = concat!("softphone-use/", env!("CARGO_PKG_VERSION"));
const ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, NOTIFY, INFO, UPDATE";
const T1: Duration = Duration::from_millis(500);
const TIMER_B: Duration = Duration::from_secs(32);

pub struct UaConfig {
    pub user: String,
    pub password: String,
    pub domain: String,
    /// `host:port` of the proxy, as configured.
    pub proxy: String,
}

/// What reaches an established dialog from the far end.
#[derive(Debug)]
pub enum DialogEvent {
    Bye,
    /// A re-INVITE (or UPDATE) with a new offer; the owner answers it with `Dialog::respond`.
    Offer {
        msg: SipMsg,
        from: SocketAddr,
    },
}

pub struct Dialog {
    ua: Arc<Ua>,
    pub call_id: String,
    local: String,
    remote: String,
    remote_target: String,
    route_set: Vec<String>,
    cseq: AtomicU32,
    /// Set when the ACK for our 200 arrives, which stops its retransmission.
    acked: AtomicBool,
    /// Our ACK for the far end's 200, resent when the 200 is.
    ack: Mutex<Option<SipMsg>>,
}

/// An INVITE that started a new call, until it is answered or rejected.
pub struct ServerInvite {
    ua: Arc<Ua>,
    pub req: SipMsg,
    from: SocketAddr,
    pub local_tag: String,
    state: Arc<ServerTx>,
}

struct ServerTx {
    last: Mutex<Option<SipMsg>>,
    from: SocketAddr,
    cancelled: watch::Sender<bool>,
}

pub struct Ua {
    pub cfg: UaConfig,
    pub transport: Arc<Transport>,
    clients: Mutex<HashMap<String, mpsc::UnboundedSender<SipMsg>>>,
    servers: Mutex<HashMap<String, Arc<ServerTx>>>,
    dialogs: Mutex<HashMap<String, (Arc<Dialog>, mpsc::UnboundedSender<DialogEvent>)>>,
    invites: mpsc::UnboundedSender<ServerInvite>,
    reg_call_id: String,
    reg_cseq: AtomicU32,
}

impl Ua {
    pub fn new(cfg: UaConfig, transport: Arc<Transport>, incoming: mpsc::UnboundedReceiver<Incoming>) -> (Arc<Ua>, mpsc::UnboundedReceiver<ServerInvite>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let ua = Arc::new(Ua {
            cfg,
            transport,
            clients: Mutex::default(),
            servers: Mutex::default(),
            dialogs: Mutex::default(),
            invites: tx,
            reg_call_id: format!("{}@softphone", random_token(20)),
            reg_cseq: AtomicU32::new(0),
        });
        tokio::spawn(dispatch(Arc::downgrade(&ua), incoming));
        (ua, rx)
    }

    fn aor(&self) -> String {
        format!("sip:{}@{}", self.cfg.user, self.cfg.domain)
    }

    pub fn contact(&self) -> String {
        let l = self.transport.local;
        format!("<sip:{}@{}:{};transport={}>", self.cfg.user, l.ip(), l.port(), self.transport.name.to_ascii_lowercase())
    }

    fn via(&self, branch: &str) -> String {
        let l = self.transport.local;
        format!("SIP/2.0/{} {}:{};branch={branch};rport", self.transport.name, l.ip(), l.port())
    }

    fn base_request(&self, method: &str, uri: &str, from: &str, to: &str, call_id: &str, cseq: u32) -> SipMsg {
        let mut m = SipMsg::request(method, uri);
        m.add("via", self.via(&new_branch()))
            .add("max-forwards", "70")
            .add("from", from)
            .add("to", to)
            .add("call-id", call_id)
            .add("cseq", format!("{cseq} {method}"))
            .add("user-agent", USER_AGENT);
        m
    }

    async fn send(self: &Arc<Self>, to: SocketAddr, msg: &SipMsg) -> anyhow::Result<()> {
        self.transport.send(to, msg).await
    }

    /// Starts a client transaction for `req`, retransmitting over UDP until a response arrives.
    async fn client(self: &Arc<Self>, req: &SipMsg) -> anyhow::Result<ClientTx> {
        let key = format!("{}|{}", req.branch(), req.method());
        let (tx, rx) = mpsc::unbounded_channel();
        self.clients.lock().unwrap().insert(key.clone(), tx);
        let answered = Arc::new(AtomicBool::new(false));
        let guard = ClientTx { ua: self.clone(), key, rx, answered: answered.clone() };
        self.send(self.transport.proxy, req).await?;
        if self.transport.name == "UDP" {
            let (ua, req) = (self.clone(), req.clone());
            tokio::spawn(async move {
                let mut wait = T1;
                let mut spent = Duration::ZERO;
                while spent < TIMER_B {
                    tokio::time::sleep(wait).await;
                    spent += wait;
                    if answered.load(Ordering::Relaxed) || Arc::strong_count(&answered) == 1 {
                        return;
                    }
                    let _ = ua.send(ua.transport.proxy, &req).await;
                    wait = (wait * 2).min(Duration::from_secs(4));
                }
            });
        }
        Ok(guard)
    }

    /// Sends a non-INVITE request and waits for its final response, answering one challenge.
    async fn request(self: &Arc<Self>, mut req: SipMsg, timeout: Duration) -> anyhow::Result<SipMsg> {
        for attempt in 0..2 {
            let mut tx = self.client(&req).await?;
            let res = tokio::time::timeout(timeout, tx.final_response()).await.map_err(|_| anyhow::anyhow!("{} timed out", req.method()))??;
            let code = res.code().unwrap_or(0);
            if (code == 401 || code == 407) && attempt == 0 {
                req = self.authorize(&req, &res)?;
                continue;
            }
            return Ok(res);
        }
        unreachable!()
    }

    /// `req` again with a new branch and CSeq, answering the challenge in `res`.
    fn authorize(&self, req: &SipMsg, res: &SipMsg) -> anyhow::Result<SipMsg> {
        let (header, answer) = if res.code() == Some(407) { ("proxy-authenticate", "proxy-authorization") } else { ("www-authenticate", "authorization") };
        let challenge = res.header(header).and_then(digest::parse_challenge).ok_or_else(|| anyhow::anyhow!("{} without a digest challenge", res.summary()))?;
        let mut next = req.clone();
        let (n, method) = req.cseq();
        next.set("cseq", format!("{} {method}", n + 1));
        next.set("via", self.via(&new_branch()));
        next.remove("authorization");
        next.remove("proxy-authorization");
        next.add(answer, digest::authorization(&challenge, &self.cfg.user, &self.cfg.password, method, req.uri()));
        if method == "REGISTER" {
            self.reg_cseq.fetch_add(1, Ordering::Relaxed);
        }
        Ok(next)
    }

    /// Registers once for `expires` seconds (0 unregisters) and returns the granted expiry.
    pub async fn register(self: &Arc<Self>, expires: u32) -> anyhow::Result<u32> {
        let cseq = self.reg_cseq.fetch_add(1, Ordering::Relaxed) + 1;
        let aor = format!("<{}>", self.aor());
        let mut req =
            self.base_request("REGISTER", &format!("sip:{}", self.cfg.domain), &format!("{aor};tag={}", random_token(10)), &aor, &self.reg_call_id, cseq);
        req.add("contact", self.contact()).add("expires", expires.to_string()).add("allow", ALLOW);
        let res = self.request(req, Duration::from_secs(32)).await?;
        match res.code() {
            Some(200..=299) => {
                let granted = res
                    .headers_all("contact")
                    .iter()
                    .find_map(|c| param(c, "expires").and_then(|e| e.parse().ok()))
                    .or_else(|| res.header("expires").and_then(|e| e.parse().ok()))
                    .unwrap_or(expires);
                Ok(granted)
            }
            _ => anyhow::bail!("REGISTER rejected: {}", res.summary()),
        }
    }

    /// Places a call. `on_response` sees every response; a true on `cancel` before the far end
    /// answers sends CANCEL. Returns the dialog and its events once the call is answered.
    pub async fn invite(
        self: &Arc<Self>,
        target: &str,
        call_id: &str,
        sdp: String,
        mut on_response: impl FnMut(&SipMsg),
        mut cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<(Arc<Dialog>, SipMsg, mpsc::UnboundedReceiver<DialogEvent>)> {
        let uri = format!("sip:{target}@{};transport={}", self.cfg.proxy, self.transport.name.to_ascii_lowercase());
        let from = format!("\"{}\" <{}>;tag={}", self.cfg.user, self.aor(), random_token(10));
        let to = format!("<sip:{target}@{}>", self.cfg.proxy);
        let mut req = self.base_request("INVITE", &uri, &from, &to, call_id, 1);
        req.add("contact", self.contact()).add("allow", ALLOW).add("content-type", "application/sdp");
        req.body = sdp.into_bytes();

        for attempt in 0..2 {
            let mut tx = self.client(&req).await?;
            let mut provisional = false;
            let mut cancelled = false;
            let res = loop {
                tokio::select! {
                    r = tx.rx.recv() => {
                        let Some(r) = r else { anyhow::bail!("transaction closed") };
                        tx.answered.store(true, Ordering::Relaxed);
                        on_response(&r);
                        if r.code().unwrap_or(0) >= 200 { break r; }
                        provisional = true;
                    }
                    _ = async { let _ = cancel.wait_for(|v| *v).await; }, if !cancelled && provisional => {
                        cancelled = true;
                        let mut c = req.clone();
                        c.start = super::message::Start::Request { method: "CANCEL".into(), uri: req.uri().into() };
                        c.set("cseq", format!("{} CANCEL", req.cseq().0));
                        c.remove("contact");
                        c.remove("content-type");
                        c.body.clear();
                        let mut ctx = self.client(&c).await?;
                        tokio::spawn(async move { let _ = tokio::time::timeout(TIMER_B, ctx.final_response()).await; });
                    }
                }
            };
            let code = res.code().unwrap_or(0);
            if code >= 300 {
                self.ack_error(&req, &res).await?;
                if (code == 401 || code == 407) && attempt == 0 && !cancelled {
                    req = self.authorize(&req, &res)?;
                    continue;
                }
                if cancelled {
                    anyhow::bail!("cancelled");
                }
                anyhow::bail!("{}", res.summary());
            }
            let mut route_set: Vec<String> = res.headers_all("record-route").iter().map(|s| s.to_string()).collect();
            route_set.reverse();
            let dialog = Arc::new(Dialog {
                ua: self.clone(),
                call_id: call_id.to_string(),
                local: req.header("from").unwrap_or_default().to_string(),
                remote: res.header("to").unwrap_or_default().to_string(),
                remote_target: res.contact_uri().unwrap_or_else(|| uri.clone()),
                route_set,
                cseq: AtomicU32::new(req.cseq().0),
                acked: AtomicBool::new(true),
                ack: Mutex::new(None),
            });
            let events = self.add_dialog(dialog.clone());
            let mut ack = dialog.request("ACK");
            ack.set("cseq", format!("{} ACK", req.cseq().0));
            self.send(self.transport.proxy, &ack).await?;
            *dialog.ack.lock().unwrap() = Some(ack);
            if cancelled {
                // The far end answered as the CANCEL crossed: hang up the call it answered.
                let _ = dialog.bye().await;
                anyhow::bail!("cancelled");
            }
            return Ok((dialog, res, events));
        }
        unreachable!()
    }

    /// The ACK for a non-2xx final response: part of the INVITE transaction, so the same branch.
    async fn ack_error(self: &Arc<Self>, invite: &SipMsg, res: &SipMsg) -> anyhow::Result<()> {
        let mut ack = SipMsg::request("ACK", invite.uri());
        ack.add("via", invite.header("via").unwrap_or_default())
            .add("max-forwards", "70")
            .add("from", invite.header("from").unwrap_or_default())
            .add("to", res.header("to").unwrap_or_default())
            .add("call-id", invite.call_id())
            .add("cseq", format!("{} ACK", invite.cseq().0));
        for r in invite.headers_all("route") {
            ack.add("route", r);
        }
        self.send(self.transport.proxy, &ack).await
    }

    fn add_dialog(&self, d: Arc<Dialog>) -> mpsc::UnboundedReceiver<DialogEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.dialogs.lock().unwrap().insert(d.call_id.clone(), (d, tx));
        rx
    }

    pub fn remove_dialog(&self, call_id: &str) {
        self.dialogs.lock().unwrap().remove(call_id);
    }

    async fn handle(self: &Arc<Self>, inc: Incoming) {
        let Incoming { msg, from } = inc;
        if msg.code().is_some() {
            let key = format!("{}|{}", msg.branch(), msg.method());
            let client = self.clients.lock().unwrap().get(&key).cloned();
            match client {
                Some(tx) => {
                    let _ = tx.send(msg);
                }
                None if msg.method() == "INVITE" && msg.code().is_some_and(|c| (200..300).contains(&c)) => {
                    // A retransmitted 200: the ACK it wants was lost.
                    let ack = self.dialogs.lock().unwrap().get(msg.call_id()).and_then(|(d, _)| d.ack.lock().unwrap().clone());
                    if let Some(ack) = ack {
                        let _ = self.send(self.transport.proxy, &ack).await;
                    }
                }
                None => {}
            }
            return;
        }
        let dialog = self.dialogs.lock().unwrap().get(msg.call_id()).cloned();
        match msg.method() {
            "ACK" => {
                if let Some((d, _)) = dialog {
                    d.acked.store(true, Ordering::Relaxed);
                }
            }
            "CANCEL" => {
                let server = self.servers.lock().unwrap().get(msg.branch()).cloned();
                match server {
                    Some(s) => {
                        let _ = self.send(from, &msg.response(200, "OK", None)).await;
                        s.cancelled.send_replace(true);
                    }
                    None => {
                        let _ = self.send(from, &msg.response(481, "Call/Transaction Does Not Exist", None)).await;
                    }
                }
            }
            "INVITE" if msg.to_tag().is_empty() => {
                let existing = self.servers.lock().unwrap().get(msg.branch()).cloned();
                if let Some(s) = existing {
                    let last = s.last.lock().unwrap().clone();
                    if let Some(last) = last {
                        let _ = self.send(s.from, &last).await;
                    }
                    return;
                }
                let state = Arc::new(ServerTx { last: Mutex::new(None), from, cancelled: watch::channel(false).0 });
                self.servers.lock().unwrap().insert(msg.branch().to_string(), state.clone());
                let invite = ServerInvite { ua: self.clone(), local_tag: random_token(10), req: msg, from, state };
                if let Err(e) = self.invites.send(invite) {
                    let _ = self.send(from, &e.0.req.response(480, "Temporarily Unavailable", Some(&e.0.local_tag))).await;
                }
            }
            "INVITE" | "UPDATE" | "BYE" => match dialog {
                Some((d, events)) => {
                    if msg.method() == "BYE" {
                        let _ = self.send(from, &msg.response(200, "OK", None)).await;
                        let _ = events.send(DialogEvent::Bye);
                    } else if msg.body.is_empty() {
                        d.respond(&msg, from, None).await;
                    } else {
                        let _ = events.send(DialogEvent::Offer { msg, from });
                    }
                }
                None => {
                    let _ = self.send(from, &msg.response(481, "Call/Transaction Does Not Exist", None)).await;
                }
            },
            "OPTIONS" | "NOTIFY" | "INFO" | "MESSAGE" => {
                let mut r = msg.response(200, "OK", None);
                r.add("allow", ALLOW).add("user-agent", USER_AGENT);
                let _ = self.send(from, &r).await;
            }
            _ => {
                let mut r = msg.response(501, "Not Implemented", None);
                r.add("allow", ALLOW);
                let _ = self.send(from, &r).await;
            }
        }
    }
}

async fn dispatch(ua: std::sync::Weak<Ua>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    while let Some(inc) = incoming.recv().await {
        let Some(ua) = ua.upgrade() else { return };
        ua.handle(inc).await;
    }
}

struct ClientTx {
    ua: Arc<Ua>,
    key: String,
    rx: mpsc::UnboundedReceiver<SipMsg>,
    answered: Arc<AtomicBool>,
}

impl ClientTx {
    async fn final_response(&mut self) -> anyhow::Result<SipMsg> {
        while let Some(r) = self.rx.recv().await {
            self.answered.store(true, Ordering::Relaxed);
            if r.code().unwrap_or(0) >= 200 {
                return Ok(r);
            }
        }
        anyhow::bail!("transaction closed")
    }
}

impl Drop for ClientTx {
    fn drop(&mut self) {
        self.ua.clients.lock().unwrap().remove(&self.key);
    }
}

impl Dialog {
    /// A new in-dialog request (RFC 3261 §12.2.1.1).
    fn request(&self, method: &str) -> SipMsg {
        let cseq = if method == "ACK" { self.cseq.load(Ordering::Relaxed) } else { self.cseq.fetch_add(1, Ordering::Relaxed) + 1 };
        let mut m = self.ua.base_request(method, &self.remote_target, &self.local, &self.remote, &self.call_id, cseq);
        for r in &self.route_set {
            m.add("route", r.clone());
        }
        m
    }

    /// Hangs up and waits up to 5 s for the far end to confirm.
    pub async fn bye(&self) -> anyhow::Result<()> {
        let req = self.request("BYE");
        let res = self.ua.request(req, Duration::from_secs(5)).await?;
        self.ua.remove_dialog(&self.call_id);
        match res.code() {
            Some(200..=299) | Some(481) => Ok(()),
            _ => anyhow::bail!("BYE answered {}", res.summary()),
        }
    }

    /// Answers an in-dialog offer with 200 and our SDP (or none).
    pub async fn respond(&self, req: &SipMsg, from: SocketAddr, sdp: Option<String>) {
        let mut r = req.response(200, "OK", None);
        r.add("contact", self.ua.contact()).add("user-agent", USER_AGENT);
        if let Some(sdp) = sdp {
            r.add("content-type", "application/sdp");
            r.body = sdp.into_bytes();
        }
        let _ = self.ua.send(from, &r).await;
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        tracing::debug!(call_id = %self.call_id, "Dialog dropped");
    }
}

impl ServerInvite {
    pub fn from_user(&self) -> String {
        super::message::uri_user(self.req.header("from").unwrap_or_default())
    }

    pub fn to_user(&self) -> String {
        super::message::uri_user(self.req.header("to").unwrap_or_default())
    }

    /// Resolves when the caller cancels.
    pub async fn cancelled(&self) {
        let mut rx = self.state.cancelled.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    async fn send(&self, r: SipMsg) {
        *self.state.last.lock().unwrap() = Some(r.clone());
        let _ = self.ua.send(self.from, &r).await;
    }

    pub async fn respond(&self, code: u16, reason: &str) {
        let mut r = self.req.response(code, reason, Some(&self.local_tag));
        r.add("user-agent", USER_AGENT);
        if (101..200).contains(&code) {
            r.add("contact", self.ua.contact());
        }
        self.send(r).await;
        if code >= 200 {
            self.forget();
        }
    }

    /// Answers with 200 and `sdp`, and returns the dialog. Over UDP the 200 is resent until
    /// the ACK arrives.
    pub async fn answer(self, sdp: String) -> (Arc<Dialog>, mpsc::UnboundedReceiver<DialogEvent>) {
        let mut r = self.req.response(200, "OK", Some(&self.local_tag));
        r.add("contact", self.ua.contact()).add("allow", ALLOW).add("user-agent", USER_AGENT).add("content-type", "application/sdp");
        r.body = sdp.into_bytes();
        let dialog = Arc::new(Dialog {
            ua: self.ua.clone(),
            call_id: self.req.call_id().to_string(),
            local: r.header("to").unwrap_or_default().to_string(),
            remote: self.req.header("from").unwrap_or_default().to_string(),
            remote_target: self.req.contact_uri().unwrap_or_default(),
            route_set: self.req.headers_all("record-route").iter().map(|s| s.to_string()).collect(),
            cseq: AtomicU32::new(rand::random::<u16>() as u32),
            acked: AtomicBool::new(false),
            ack: Mutex::new(None),
        });
        let events = self.ua.add_dialog(dialog.clone());
        self.send(r.clone()).await;
        if self.ua.transport.name == "UDP" {
            let (d, ua, from) = (Arc::downgrade(&dialog), self.ua.clone(), self.from);
            tokio::spawn(async move {
                let mut wait = T1;
                let mut spent = Duration::ZERO;
                while spent < TIMER_B {
                    tokio::time::sleep(wait).await;
                    spent += wait;
                    match d.upgrade() {
                        Some(d) if !d.acked.load(Ordering::Relaxed) => {
                            let _ = ua.send(from, &r).await;
                        }
                        _ => return,
                    }
                    wait = (wait * 2).min(Duration::from_secs(4));
                }
            });
        }
        self.forget();
        (dialog, events)
    }

    /// Drops the transaction after Timer H, so late retransmissions still get our response.
    fn forget(&self) {
        let (ua, branch) = (self.ua.clone(), self.req.branch().to_string());
        tokio::spawn(async move {
            tokio::time::sleep(TIMER_B).await;
            ua.servers.lock().unwrap().remove(&branch);
        });
    }
}
