//! One call as the softphone sees it: state, events, levels and the audio paths.

use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::audio::{Listeners, Mode, PcmQueue};
use crate::trace::{SdpSummary, SipMessage, SipTrace};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallState {
    Ringing,
    Answered,
    Ended,
}

/// One second of audio in one direction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub second: u32,
    pub dbfs: Option<f64>,
    pub packets: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallEvent {
    pub time: DateTime<Utc>,
    pub message: String,
}

/// Flags the failures warm transfers have hit: no RTP at all, digital silence (exact zeros), or
/// a one-way call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSummary {
    pub seconds_in: u32,
    pub packets_in: u32,
    pub seconds_with_sound_in: u32,
    #[serde(rename = "secondsDigitalSilentIn")]
    pub seconds_digital_silent: u32,
    pub seconds_no_packets_in: u32,
    pub packets_out: u32,
    pub inbound_audio_heard: bool,
}

const SOUND_THRESHOLD_DBFS: f64 = -50.0;
const DIGITAL_SILENCE_DBFS: f64 = -95.0;

pub fn summarize_audio(levels_in: &[Level], levels_out: &[Level]) -> AudioSummary {
    let mut s = AudioSummary { seconds_in: levels_in.len() as u32, ..Default::default() };
    for l in levels_in {
        s.packets_in += l.packets;
        match l.dbfs {
            _ if l.packets == 0 => s.seconds_no_packets_in += 1,
            None => s.seconds_no_packets_in += 1,
            Some(v) if v <= DIGITAL_SILENCE_DBFS => s.seconds_digital_silent += 1,
            Some(v) if v >= SOUND_THRESHOLD_DBFS => s.seconds_with_sound_in += 1,
            _ => {}
        }
    }
    s.packets_out = levels_out.iter().map(|l| l.packets).sum();
    s.inbound_audio_heard = s.seconds_with_sound_in > 0;
    s
}

/// The JSON view of a call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallSnapshot {
    pub id: String,
    pub outbound: bool,
    pub from: String,
    pub to: String,
    pub state: CallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_reason: String,
    #[serde(default)]
    pub offer: SdpSummary,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codec: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub local_rtp: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote_rtp: String,
    #[serde(default)]
    pub events: Vec<CallEvent>,
    #[serde(default)]
    pub audio: AudioSummary,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wav_in: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wav_out: String,
}

pub struct CallInner {
    pub outbound: bool,
    pub from: String,
    pub to: String,
    pub state: CallState,
    pub mode: Option<Mode>,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: String,
    pub offer: SdpSummary,
    pub codec: String,
    pub local_rtp: String,
    pub remote_rtp: String,
    pub events: Vec<CallEvent>,
    pub levels_in: Vec<Level>,
    pub levels_out: Vec<Level>,
    pub wav_in: String,
    pub wav_out: String,
    /// The SIP messages of a call loaded from disk.
    pub saved_sip: Option<Vec<SipMessage>>,
    /// AI settings for `Mode::Ai`; empty means the defaults.
    pub ai_instructions: String,
    pub ai_voice: String,
    pub ai_task: Option<tokio::task::AbortHandle>,
    /// Stops the local microphone and speaker of `Mode::Human`.
    pub human_stop: Option<Arc<AtomicBool>>,
    /// The live level of the last 100 ms, in and out, for the UI.
    pub live_in: Option<f64>,
    pub live_out: Option<f64>,
}

/// A call. Fields are guarded by `inner`; read them through `snapshot`.
pub struct Call {
    pub id: String,
    pub inner: Mutex<CallInner>,
    /// Fans the decoded inbound audio out to the speaker and the AI session.
    pub inbound: Listeners,
    /// Microphone audio arrives in real time: keep under 0.5 s of backlog.
    pub human_out: PcmQueue,
    /// xAI streams faster than real time: hold a whole reply.
    pub ai_out: PcmQueue,
    /// Sends silence instead of the microphone in `Mode::Human`.
    pub muted: AtomicBool,
    answer: Mutex<Option<oneshot::Sender<Mode>>>,
    hangup: watch::Sender<bool>,
}

impl Call {
    /// A new call and the receiver its answer request arrives on.
    pub fn new(id: &str, from: &str, to: &str, offer: SdpSummary, now: DateTime<Utc>) -> (Arc<Call>, oneshot::Receiver<Mode>) {
        let (answer_tx, answer_rx) = oneshot::channel();
        let call = Call {
            id: id.to_string(),
            inner: Mutex::new(CallInner {
                outbound: false,
                from: from.to_string(),
                to: to.to_string(),
                state: CallState::Ringing,
                mode: None,
                started_at: now,
                answered_at: None,
                ended_at: None,
                end_reason: String::new(),
                offer,
                codec: String::new(),
                local_rtp: String::new(),
                remote_rtp: String::new(),
                events: Vec::new(),
                levels_in: Vec::new(),
                levels_out: Vec::new(),
                wav_in: String::new(),
                wav_out: String::new(),
                saved_sip: None,
                ai_instructions: String::new(),
                ai_voice: String::new(),
                ai_task: None,
                human_stop: None,
                live_in: None,
                live_out: None,
            }),
            inbound: Listeners::default(),
            human_out: PcmQueue::new(4000),
            ai_out: PcmQueue::new(8000 * 120),
            muted: AtomicBool::new(false),
            answer: Mutex::new(Some(answer_tx)),
            hangup: watch::channel(false).0,
        };
        (Arc::new(call), answer_rx)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, CallInner> {
        self.inner.lock().unwrap()
    }

    pub fn event(&self, msg: impl Into<String>) {
        self.lock().events.push(CallEvent { time: Utc::now(), message: msg.into() });
    }

    /// Adds an event and returns its index, for [`Self::replace_event`].
    pub fn event_at(&self, msg: impl Into<String>) -> usize {
        let mut g = self.lock();
        g.events.push(CallEvent { time: Utc::now(), message: msg.into() });
        g.events.len() - 1
    }

    /// Rewrites an event's text in place, keeping its time.
    pub fn replace_event(&self, index: usize, msg: impl Into<String>) {
        if let Some(e) = self.lock().events.get_mut(index) {
            e.message = msg.into();
        }
    }

    pub fn state(&self) -> CallState {
        self.lock().state
    }

    pub fn mode(&self) -> Option<Mode> {
        self.lock().mode
    }

    pub fn set_mode(&self, m: Mode) {
        self.lock().mode = Some(m);
    }

    pub fn add_level(&self, inbound: bool, l: Level) {
        let mut g = self.lock();
        if inbound { g.levels_in.push(l) } else { g.levels_out.push(l) }
    }

    /// Queues the answer for a ringing call; false when one is already queued.
    pub fn request_answer(&self, mode: Mode) -> bool {
        match self.answer.lock().unwrap().take() {
            Some(tx) => tx.send(mode).is_ok(),
            None => false,
        }
    }

    pub fn request_hangup(&self) {
        self.hangup.send_replace(true);
    }

    /// Turns true when a hangup is requested.
    pub fn hangup_rx(&self) -> watch::Receiver<bool> {
        self.hangup.subscribe()
    }

    /// Resolves once a hangup is requested.
    pub async fn hung_up(&self) {
        let mut rx = self.hangup.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    pub fn snapshot(&self) -> CallSnapshot {
        let g = self.lock();
        CallSnapshot {
            id: self.id.clone(),
            outbound: g.outbound,
            from: g.from.clone(),
            to: g.to.clone(),
            state: g.state,
            mode: g.mode,
            started_at: g.started_at,
            answered_at: g.answered_at,
            ended_at: g.ended_at,
            end_reason: g.end_reason.clone(),
            offer: g.offer.clone(),
            codec: g.codec.clone(),
            local_rtp: g.local_rtp.clone(),
            remote_rtp: g.remote_rtp.clone(),
            events: g.events.clone(),
            audio: summarize_audio(&g.levels_in, &g.levels_out),
            wav_in: g.wav_in.clone(),
            wav_out: g.wav_out.clone(),
        }
    }

    pub fn levels(&self) -> (Vec<Level>, Vec<Level>) {
        let g = self.lock();
        (g.levels_in.clone(), g.levels_out.clone())
    }

    /// The call's messages: from the live trace, or as saved for a call loaded after a restart.
    pub fn sip(&self, trace: &SipTrace) -> Vec<SipMessage> {
        if let Some(saved) = &self.lock().saved_sip {
            return saved.clone();
        }
        trace.for_call(&self.id)
    }

    pub fn stop_human(&self) {
        if let Some(stop) = self.lock().human_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_audio_flags_digital_silence_and_gaps() {
        let l = |s, d: Option<f64>, p| Level { second: s, dbfs: d, packets: p };
        let levels_in = [l(1, Some(-22.0), 50), l(2, Some(-120.0), 50), l(3, None, 0), l(4, Some(-70.0), 50)];
        let out = [l(1, None, 50), l(2, None, 50)];
        let s = summarize_audio(&levels_in, &out);
        assert_eq!((s.seconds_with_sound_in, s.seconds_digital_silent, s.seconds_no_packets_in), (1, 1, 1));
        assert!(s.inbound_audio_heard);
        assert_eq!((s.packets_in, s.packets_out), (150, 100));
        assert!(!summarize_audio(&[l(1, Some(-120.0), 50)], &[]).inbound_audio_heard, "digital silence is not heard");
    }

    #[test]
    fn second_answer_while_ringing_fails() {
        let (c, mut rx) = Call::new("id1", "2002", "1001", SdpSummary::default(), Utc::now());
        assert!(c.request_answer(Mode::Tone));
        assert!(!c.request_answer(Mode::Echo));
        assert_eq!(rx.try_recv().unwrap(), Mode::Tone);
    }
}
