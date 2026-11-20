//! The phone: a SIP device registered on a PBX that answers calls with scripted audio,
//! a person or an AI, and records what it hears.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Notify, mpsc, watch};

use crate::audio::{Mode, ToneGenerator, WavWriter, dbfs, i16s_to_le};
use crate::call::{Call, CallState, Level};
use crate::config::Config;
use crate::rtp::{Rtp, SAMPLES_PER_FRAME};
use crate::sip::sdp;
use crate::sip::transport::Transport;
use crate::sip::ua::{Dialog, DialogEvent, ServerInvite, Ua, UaConfig};
use crate::store;
use crate::trace::{SipTrace, summarize_sdp};

const REGISTER_EXPIRES: u32 = 300;
const SAMPLE_RATE: u32 = 8000;

pub struct Phone {
    pub cfg: Config,
    pub trace: Arc<SipTrace>,
    rt: tokio::runtime::Handle,
    ua: Mutex<Option<Arc<Ua>>>,
    state: Mutex<PhoneState>,
    new_call: Notify,
}

#[derive(Default)]
struct PhoneState {
    registered: bool,
    register_err: String,
    registered_at: Option<DateTime<Utc>>,
    calls: HashMap<String, Arc<Call>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneStatus {
    pub device: String,
    pub proxy: String,
    pub transport: String,
    pub registered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    pub auto_answer: String,
    pub active_calls: usize,
}

impl Phone {
    pub fn new(cfg: Config, rt: tokio::runtime::Handle) -> Arc<Phone> {
        let (saved, errs) = store::load_calls(&cfg.data_dir);
        for e in errs {
            tracing::warn!(error = %e, "Skipped a saved call");
        }
        if !saved.is_empty() {
            tracing::info!(count = saved.len(), dir = %cfg.data_dir.display(), "Loaded saved calls");
        }
        let calls = saved.into_iter().map(|c| (c.id.clone(), c)).collect();
        Arc::new(Phone {
            cfg,
            trace: Arc::new(SipTrace::default()),
            rt,
            ua: Mutex::new(None),
            state: Mutex::new(PhoneState { calls, ..Default::default() }),
            new_call: Notify::new(),
        })
    }

    /// Registers the device and serves calls until `shutdown` turns true.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        let proxy = tokio::net::lookup_host(&self.cfg.proxy).await?.next().ok_or_else(|| anyhow::anyhow!("{} has no address", self.cfg.proxy))?;
        let (transport, incoming) = Transport::bind(&self.cfg.transport, proxy, self.trace.clone()).await?;
        let cfg = UaConfig { user: self.cfg.user.clone(), password: self.cfg.password.clone(), domain: self.cfg.domain.clone(), proxy: self.cfg.proxy.clone() };
        let (ua, mut invites) = Ua::new(cfg, transport.clone(), incoming);
        *self.ua.lock().unwrap() = Some(ua.clone());
        tracing::info!(local = %transport.local, proxy = %proxy, transport = transport.name, "SIP transport ready");

        let me = self.clone();
        tokio::spawn(async move {
            while let Some(inv) = invites.recv().await {
                tokio::spawn(me.clone().handle_invite(inv));
            }
        });

        loop {
            let err = tokio::select! {
                r = self.registration(&ua, &transport) => r.err(),
                _ = shutdown.wait_for(|v| *v) => None,
            };
            {
                let mut s = self.state.lock().unwrap();
                s.registered = false;
                if let Some(e) = &err {
                    s.register_err = e.to_string();
                }
            }
            if *shutdown.borrow() {
                let _ = tokio::time::timeout(Duration::from_secs(5), ua.register(0)).await;
                return Ok(());
            }
            tracing::error!(error = %err.map(|e| e.to_string()).unwrap_or_default(), "Registration ended, retrying in 10s");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                _ = shutdown.wait_for(|v| *v) => return Ok(()),
            }
        }
    }

    /// Keeps one registration alive until it fails.
    async fn registration(&self, ua: &Arc<Ua>, transport: &Arc<Transport>) -> anyhow::Result<()> {
        let mut expires = ua.register(REGISTER_EXPIRES).await?;
        {
            let mut s = self.state.lock().unwrap();
            s.registered = true;
            s.register_err.clear();
            s.registered_at = Some(Utc::now());
        }
        tracing::info!(aor = %format!("{}@{}", self.cfg.user, self.cfg.domain), proxy = %self.cfg.proxy, "Registered");
        let mut keepalive = tokio::time::interval(Duration::from_secs(30));
        loop {
            let refresh = Duration::from_secs((expires / 2).max(15) as u64);
            let deadline = tokio::time::Instant::now() + refresh;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    _ = keepalive.tick() => transport.keepalive(),
                    _ = transport.proxy_lost.notified() => anyhow::bail!("connection to {} closed", self.cfg.proxy),
                }
            }
            expires = ua.register(REGISTER_EXPIRES).await?;
        }
    }

    fn ua(&self) -> anyhow::Result<Arc<Ua>> {
        self.ua.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("phone is not running"))
    }

    pub fn add_call(&self, c: Arc<Call>) {
        self.state.lock().unwrap().calls.insert(c.id.clone(), c);
        self.new_call.notify_waiters();
    }

    pub fn end_call(&self, c: &Call, reason: &str) {
        {
            let mut g = c.lock();
            g.state = CallState::Ended;
            g.ended_at = Some(Utc::now());
            g.end_reason = reason.to_string();
            g.live_in = None;
            g.live_out = None;
        }
        c.event(format!("ended: {reason}"));
        tracing::info!(id = %c.id, reason, "Call ended");
        if let Err(e) = store::save_call(&self.cfg.data_dir, c, self.trace.for_call(&c.id)) {
            tracing::error!(id = %c.id, error = %e, "Failed to save call");
        }
    }

    pub fn call(&self, id: &str) -> Option<Arc<Call>> {
        self.state.lock().unwrap().calls.get(id).cloned()
    }

    /// All calls, newest first.
    pub fn calls(&self) -> Vec<Arc<Call>> {
        let mut out: Vec<_> = self.state.lock().unwrap().calls.values().cloned().collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.lock().started_at));
        out
    }

    /// The newest ringing call, waiting up to `timeout` for one.
    pub async fn wait_for_call(&self, timeout: Duration) -> anyhow::Result<Arc<Call>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let woken = self.new_call.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if let Some(c) = self.calls().into_iter().find(|c| c.state() == CallState::Ringing) {
                return Ok(c);
            }
            tokio::select! {
                _ = woken => {}
                _ = tokio::time::sleep_until(deadline) => anyhow::bail!("no incoming call before timeout"),
            }
        }
    }

    pub fn status(&self) -> PhoneStatus {
        let active = self.calls().iter().filter(|c| c.state() != CallState::Ended).count();
        let s = self.state.lock().unwrap();
        PhoneStatus {
            device: format!("{}@{}", self.cfg.user, self.cfg.domain),
            proxy: self.cfg.proxy.clone(),
            transport: self.cfg.transport.clone(),
            registered: s.registered,
            registered_at: s.registered_at,
            last_error: s.register_err.clone(),
            auto_answer: self.cfg.auto_answer.map_or("manual", Mode::as_str).to_string(),
            active_calls: active,
        }
    }

    /// Agent mode needs an xAI key; without one the agent stays silent, so refuse it up front.
    fn check_mode(&self, mode: Mode) -> anyhow::Result<()> {
        if mode == Mode::Ai && self.cfg.xai_key.is_empty() {
            anyhow::bail!("Agent needs XAI_API_KEY in .env.local");
        }
        Ok(())
    }

    pub fn answer(self: &Arc<Self>, id: &str, mode: Mode) -> anyhow::Result<()> {
        self.check_mode(mode)?;
        let c = self.call(id).ok_or_else(|| anyhow::anyhow!("unknown call {id}"))?;
        match c.state() {
            CallState::Ringing => {
                if !c.request_answer(mode) {
                    anyhow::bail!("answer already requested");
                }
            }
            CallState::Answered => {
                c.set_mode(mode);
                c.event(format!("mode switched to {}", mode.as_str()));
                self.apply_mode(&c);
            }
            CallState::Ended => anyhow::bail!("call has ended"),
        }
        Ok(())
    }

    /// Ends a call. `source` says who asked ("window", "api"), for the call's events.
    pub fn hangup(&self, id: &str, source: &str) -> anyhow::Result<()> {
        let c = self.call(id).ok_or_else(|| anyhow::anyhow!("unknown call {id}"))?;
        if c.state() == CallState::Ended {
            anyhow::bail!("call has ended");
        }
        c.event(format!("hang up requested ({source})"));
        tracing::info!(call = id, source, "Hang up requested");
        c.request_hangup();
        Ok(())
    }

    /// Sets the AI instructions and voice a call uses in `Mode::Ai`; empty keeps what it has.
    pub fn set_ai(&self, c: &Call, instructions: &str, voice: &str) {
        let mut g = c.lock();
        if !instructions.is_empty() {
            g.ai_instructions = instructions.to_string();
        }
        if !voice.is_empty() {
            g.ai_voice = voice.to_string();
        }
    }

    /// Calls `target` (an extension or number) as the device. The PBX authenticates the
    /// INVITE as the device and routes it with the domain's dial plan.
    pub fn dial(self: &Arc<Self>, target: &str, mode: Mode, instructions: &str, voice: &str) -> anyhow::Result<Arc<Call>> {
        self.check_mode(mode)?;
        let target = dialable(target);
        if target.is_empty() {
            anyhow::bail!("nothing to dial");
        }
        let target = target.as_str();
        let ua = self.ua()?;
        let call_id = uuid::Uuid::new_v4().to_string();
        let (call, _) = Call::new(&call_id, &self.cfg.user, target, Default::default(), Utc::now());
        call.lock().outbound = true;
        self.set_ai(&call, instructions, voice);
        call.set_mode(mode);
        self.add_call(call.clone());
        call.event(format!("dialing {target}"));

        let (me, c, target) = (self.clone(), call.clone(), target.to_string());
        let ring_timeout = self.cfg.ring_timeout;
        self.rt.spawn(async move {
            let timer = {
                let c = c.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(ring_timeout).await;
                    if c.state() == CallState::Ringing {
                        c.event("no answer before the ring timeout");
                        c.request_hangup();
                    }
                })
            };
            let result = async {
                let rtp = Rtp::bind(ua.transport.local.ip()).await?;
                let offer = sdp::offer(rtp.local(), rand::random::<u32>() as u64);
                let ev = c.clone();
                let (dialog, res, events) = ua.invite(&target, &c.id, offer, |r| ev.event(r.summary()), c.hangup_rx()).await?;
                let remote = sdp::parse_remote(&String::from_utf8_lossy(&res.body))?;
                rtp.set_remote(remote.addr, remote.codec);
                anyhow::Ok((dialog, events, rtp))
            }
            .await;
            timer.abort();
            match result {
                Ok((dialog, events, rtp)) => {
                    me.mark_answered(&c, &rtp, "answered");
                    let reason = me.clone().in_call(&c, dialog, events, Arc::new(rtp)).await;
                    me.end_call(&c, &reason);
                }
                Err(e) => me.end_call(&c, &format!("dial failed: {e}")),
            }
        });
        Ok(call)
    }

    async fn handle_invite(self: Arc<Self>, inv: ServerInvite) {
        let offer_sdp = String::from_utf8_lossy(&inv.req.body).into_owned();
        let (call, answer_rx) = Call::new(inv.req.call_id(), &inv.from_user(), &inv.to_user(), summarize_sdp(&offer_sdp), Utc::now());
        self.add_call(call.clone());
        call.event(format!("INVITE from {}, offered {:?}", inv.from_user(), call.lock().offer.codecs));
        tracing::info!(id = %call.id, from = %inv.from_user(), "Incoming call");

        inv.respond(100, "Trying").await;
        inv.respond(180, "Ringing").await;

        let auto = self.cfg.auto_answer;
        let auto_after = self.cfg.auto_answer_after;
        let mode = tokio::select! {
            m = answer_rx => match m { Ok(m) => m, Err(_) => return },
            _ = tokio::time::sleep(auto_after), if auto.is_some() => {
                let m = auto.unwrap();
                call.event(format!("auto-answer as {}", m.as_str()));
                m
            }
            _ = call.hung_up() => {
                inv.respond(486, "Busy Here").await;
                return self.end_call(&call, "rejected");
            }
            _ = tokio::time::sleep(self.cfg.ring_timeout) => {
                inv.respond(480, "Temporarily Unavailable").await;
                return self.end_call(&call, "ring timeout");
            }
            _ = inv.cancelled() => {
                inv.respond(487, "Request Terminated").await;
                return self.end_call(&call, "cancelled by caller");
            }
        };

        let remote = match sdp::parse_remote(&offer_sdp) {
            Ok(r) => r,
            Err(e) => {
                inv.respond(488, "Not Acceptable Here").await;
                return self.end_call(&call, &format!("answer failed: {e}"));
            }
        };
        let rtp = match Rtp::bind(self.ua().map(|u| u.transport.local.ip()).unwrap_or([0, 0, 0, 0].into())).await {
            Ok(r) => r,
            Err(e) => {
                inv.respond(500, "Server Internal Error").await;
                return self.end_call(&call, &format!("answer failed: {e}"));
            }
        };
        rtp.set_remote(remote.addr, remote.codec);
        call.set_mode(mode);
        let answer = sdp::answer(rtp.local(), rand::random::<u32>() as u64, &remote);
        let (dialog, events) = inv.answer(answer).await;
        self.mark_answered(&call, &rtp, &format!("answered as {}", mode.as_str()));
        let reason = self.clone().in_call(&call, dialog, events, Arc::new(rtp)).await;
        self.end_call(&call, &reason);
    }

    fn mark_answered(&self, call: &Call, rtp: &Rtp, what: &str) {
        let (codec, local, remote) = (rtp.codec().name(), rtp.local().to_string(), rtp.remote().to_string());
        {
            let mut g = call.lock();
            g.state = CallState::Answered;
            g.answered_at = Some(Utc::now());
            g.codec = codec.to_string();
            g.local_rtp = local.clone();
            g.remote_rtp = remote.clone();
        }
        call.event(format!("{what}, codec {codec}, rtp {local} <-> {remote}"));
    }

    /// Runs an answered call until either side hangs up, and returns why it ended.
    async fn in_call(self: Arc<Self>, call: &Arc<Call>, dialog: Arc<Dialog>, mut events: mpsc::UnboundedReceiver<DialogEvent>, rtp: Arc<Rtp>) -> String {
        let (stop_tx, stop) = watch::channel(false);
        let media = self.start_media(call.clone(), rtp.clone(), stop);
        let reason = loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(DialogEvent::Offer { msg, from }) => {
                        match sdp::parse_remote(&String::from_utf8_lossy(&msg.body)) {
                            Ok(remote) => {
                                rtp.set_remote(remote.addr, remote.codec);
                                dialog.respond(&msg, from, Some(sdp::answer(rtp.local(), rand::random::<u32>() as u64, &remote))).await;
                                {
                                    let mut g = call.lock();
                                    g.codec = remote.codec.name().to_string();
                                    g.remote_rtp = remote.addr.to_string();
                                }
                                call.event(format!("media updated (re-INVITE): codec {}", remote.codec.name()));
                            }
                            Err(e) => {
                                dialog.respond(&msg, from, None).await;
                                call.event(format!("re-INVITE without usable SDP: {e}"));
                            }
                        }
                    }
                    Some(DialogEvent::Bye) | None => break "remote hangup".to_string(),
                },
                _ = call.hung_up() => {
                    if let Err(e) = dialog.bye().await {
                        call.event(format!("BYE failed: {e}"));
                    }
                    break "local hangup".to_string();
                }
            }
        };
        let _ = stop_tx.send(true);
        for task in media {
            let _ = task.await;
        }
        if let Ok(ua) = self.ua() {
            ua.remove_dialog(&call.id);
        }
        reason
    }

    fn start_media(self: &Arc<Self>, call: Arc<Call>, rtp: Arc<Rtp>, stop: watch::Receiver<bool>) -> Vec<tokio::task::JoinHandle<()>> {
        let dir = store::call_dir(&self.cfg.data_dir, &call.id);
        let _ = std::fs::create_dir_all(&dir);
        let wav_in = WavWriter::create(&dir.join("in.wav"), SAMPLE_RATE).ok();
        let wav_out = WavWriter::create(&dir.join("out.wav"), SAMPLE_RATE).ok();
        {
            let mut g = call.lock();
            g.wav_in = wav_in.as_ref().map(|w| w.path().display().to_string()).unwrap_or_default();
            g.wav_out = wav_out.as_ref().map(|w| w.path().display().to_string()).unwrap_or_default();
        }
        let meters = Arc::new(Meters::default());
        let (echo_tx, echo_rx) = mpsc::channel::<Vec<i16>>(50);
        let mut tasks = Vec::new();

        // Inbound: RTP payload -> PCM -> meter, WAV, echo, listeners.
        {
            let (call, rtp, meters, mut stop) = (call.clone(), rtp.clone(), meters.clone(), stop.clone());
            let mut wav = wav_in;
            tasks.push(tokio::spawn(async move {
                let (mut buf, mut payload, mut pcm, mut le) = (vec![0u8; 2048], Vec::new(), Vec::new(), Vec::new());
                loop {
                    tokio::select! {
                        r = rtp.recv(&mut buf, &mut payload) => {
                            if let Err(e) = r {
                                call.event(format!("RTP read stopped: {e}"));
                                break;
                            }
                            rtp.codec().decode(&payload, &mut pcm);
                            meters.add(true, &pcm);
                            if let Some(w) = wav.as_mut() {
                                i16s_to_le(&pcm, &mut le);
                                let _ = w.write(&le);
                            }
                            let _ = echo_tx.try_send(pcm.clone());
                            call.inbound.publish(&pcm);
                        }
                        _ = stop.wait_for(|v| *v) => break,
                    }
                }
                if let Some(w) = wav {
                    let _ = w.close();
                }
            }));
        }

        // Outbound: one frame every 20 ms.
        {
            let (call, rtp, meters, mut stop) = (call.clone(), rtp.clone(), meters.clone(), stop.clone());
            let mut wav = wav_out;
            let mut echo = echo_rx;
            tasks.push(tokio::spawn(async move {
                let mut tone = ToneGenerator::new(SAMPLE_RATE);
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let (mut pcm, mut le, mut enc) = (vec![0i16; SAMPLES_PER_FRAME], Vec::new(), Vec::new());
                loop {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = stop.wait_for(|v| *v) => break,
                    }
                    match call.mode().unwrap_or_default() {
                        Mode::Tone => tone.fill(&mut pcm),
                        Mode::Echo => match echo.try_recv() {
                            Ok(frame) => {
                                pcm.fill(0);
                                let n = frame.len().min(pcm.len());
                                pcm[..n].copy_from_slice(&frame[..n]);
                            }
                            Err(_) => pcm.fill(0),
                        },
                        Mode::Human => call.human_out.pop(&mut pcm),
                        Mode::Ai => call.ai_out.pop(&mut pcm),
                        Mode::Silent => pcm.fill(0),
                    }
                    rtp.codec().encode(&pcm, &mut enc);
                    if let Err(e) = rtp.send(&enc).await {
                        call.event(format!("RTP write stopped: {e}"));
                        break;
                    }
                    meters.add(false, &pcm);
                    if let Some(w) = wav.as_mut() {
                        i16s_to_le(&pcm, &mut le);
                        let _ = w.write(&le);
                    }
                }
                if let Some(w) = wav {
                    let _ = w.close();
                }
            }));
        }

        // Levels: live every 100 ms for the UI, and per second, so seconds without packets show.
        {
            let (call, meters, mut stop) = (call.clone(), meters.clone(), stop.clone());
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                tick.tick().await;
                for n in 1u32.. {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = stop.wait_for(|v| *v) => break,
                    }
                    let (live_in, live_out) = meters.live();
                    {
                        let mut g = call.lock();
                        g.live_in = live_in;
                        g.live_out = live_out;
                    }
                    if n % 10 == 0 {
                        let (l_in, l_out) = meters.second(n / 10);
                        call.add_level(true, l_in);
                        call.add_level(false, l_out);
                    }
                }
            }));
        }

        {
            let (me, call, mut stop) = (self.clone(), call.clone(), stop);
            tasks.push(tokio::spawn(async move {
                let _ = stop.wait_for(|v| *v).await;
                me.stop_ai(&call);
                call.stop_human();
                call.inbound.close_all();
            }));
        }
        self.apply_mode(&call);
        tasks
    }

    /// Starts or stops the AI session and the local audio to match the call's mode.
    pub fn apply_mode(self: &Arc<Self>, c: &Arc<Call>) {
        let _guard = self.rt.enter();
        match c.mode() {
            Some(Mode::Ai) => {
                c.stop_human();
                self.start_ai(c);
            }
            Some(Mode::Human) => {
                self.stop_ai(c);
                crate::human::start(c);
            }
            _ => {
                self.stop_ai(c);
                c.stop_human();
            }
        }
    }
}

/// Accumulates PCM per direction, flushed per second and per 100 ms.
#[derive(Default)]
struct Meters {
    inner: Mutex<[Acc; 4]>,
}

#[derive(Default, Clone, Copy)]
struct Acc {
    sum: f64,
    n: usize,
    packets: u32,
}

impl Meters {
    fn add(&self, inbound: bool, pcm: &[i16]) {
        let sum: f64 = pcm.iter().map(|&s| s as f64 * s as f64).sum();
        let mut g = self.inner.lock().unwrap();
        let (sec, live) = if inbound { (0, 2) } else { (1, 3) };
        for i in [sec, live] {
            g[i].sum += sum;
            g[i].n += pcm.len();
            g[i].packets += 1;
        }
    }

    fn second(&self, second: u32) -> (Level, Level) {
        let mut g = self.inner.lock().unwrap();
        let level = |a: Acc| Level { second, dbfs: dbfs(a.sum, a.n), packets: a.packets };
        let out = (level(g[0]), level(g[1]));
        g[0] = Acc::default();
        g[1] = Acc::default();
        out
    }

    fn live(&self) -> (Option<f64>, Option<f64>) {
        let mut g = self.inner.lock().unwrap();
        let out = (dbfs(g[2].sum, g[2].n), dbfs(g[3].sum, g[3].n));
        g[2] = Acc::default();
        g[3] = Acc::default();
        out
    }
}

/// The number as the PBX expects it: "(407) 555-0100" becomes "4075550100". Keeps a leading +,
/// digits, letters (extensions such as 200ap), * and #.
pub fn dialable(input: &str) -> String {
    let t = input.trim();
    let plus = t.starts_with('+');
    let rest: String = t.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '#')).collect();
    if plus { format!("+{rest}") } else { rest }
}

#[cfg(test)]
mod dial_tests {
    use super::dialable;

    #[test]
    fn strips_formatting() {
        assert_eq!(dialable("(407) 555-0100"), "4075550100");
        assert_eq!(dialable(" +1 407.555.0100 "), "+14075550100");
        assert_eq!(dialable("200ap"), "200ap");
        assert_eq!(dialable("*97"), "*97");
    }
}
