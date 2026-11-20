//! Saved calls: `<data dir>/<call id>/call.json` next to the call's WAVs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::call::{Call, CallSnapshot, CallState, Level};
use crate::trace::SipMessage;

/// What `call.json` holds: enough to show a past call after a restart. The SIP messages are
/// copied out of the in-memory trace, which does not survive a restart.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCall {
    call: CallSnapshot,
    #[serde(default)]
    levels_in: Option<Vec<Level>>,
    #[serde(default)]
    levels_out: Option<Vec<Level>>,
    #[serde(default)]
    sip: Option<Vec<SipMessage>>,
}

const CALL_FILE: &str = "call.json";

pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '@') { '_' } else { c }).collect()
}

pub fn call_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join(sanitize(id))
}

/// Writes the call's final state next to its recordings.
pub fn save_call(data_dir: &Path, c: &Call, sip: Vec<SipMessage>) -> anyhow::Result<()> {
    let (levels_in, levels_out) = c.levels();
    let stored = StoredCall { call: c.snapshot(), levels_in: Some(levels_in), levels_out: Some(levels_out), sip: Some(sip) };
    let dir = call_dir(data_dir, &c.id);
    std::fs::create_dir_all(&dir)?;
    // Write then rename, so a crash never leaves a half-written file.
    let tmp = dir.join(format!("{CALL_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&stored)?)?;
    std::fs::rename(tmp, dir.join(CALL_FILE))?;
    Ok(())
}

/// Reads every `<data dir>/*/call.json`. A missing data dir is not an error; unreadable files
/// are skipped and reported.
pub fn load_calls(data_dir: &Path) -> (Vec<Arc<Call>>, Vec<anyhow::Error>) {
    let (mut calls, mut errs) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(data_dir) else { return (calls, errs) };
    for entry in entries.flatten() {
        let path = entry.path().join(CALL_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errs.push(anyhow::anyhow!("{}: {e}", path.display()));
                continue;
            }
        };
        match serde_json::from_slice::<StoredCall>(&bytes) {
            Ok(stored) => calls.push(restore(stored)),
            Err(e) => errs.push(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
    (calls, errs)
}

fn restore(s: StoredCall) -> Arc<Call> {
    let snap = s.call;
    let (c, _) = Call::new(&snap.id, &snap.from, &snap.to, snap.offer, snap.started_at);
    {
        let mut g = c.lock();
        g.outbound = snap.outbound;
        g.state = CallState::Ended;
        g.mode = snap.mode;
        g.answered_at = snap.answered_at;
        g.ended_at = snap.ended_at;
        g.end_reason = snap.end_reason;
        g.codec = snap.codec;
        g.local_rtp = snap.local_rtp;
        g.remote_rtp = snap.remote_rtp;
        g.events = snap.events;
        g.levels_in = s.levels_in.unwrap_or_default();
        g.levels_out = s.levels_out.unwrap_or_default();
        g.wav_in = snap.wav_in;
        g.wav_out = snap.wav_out;
        g.saved_sip = Some(s.sip.unwrap_or_default());
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Mode;
    use crate::trace::{SdpSummary, SipTrace};
    use chrono::TimeZone;

    #[test]
    fn saved_calls_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let start = chrono::Utc.with_ymd_and_hms(2026, 10, 16, 8, 0, 0).unwrap();
        let offer = SdpSummary { codecs: vec!["PCMU/8000".into()], ..Default::default() };
        let (c, _) = Call::new("abc@host/1", "2002", "1001", offer, start);
        {
            let mut g = c.lock();
            g.codec = "PCMU".into();
            g.state = CallState::Ended;
            g.mode = Some(Mode::Echo);
            g.levels_in = vec![Level { second: 1, dbfs: Some(-21.0), packets: 50 }];
            g.levels_out = vec![Level { second: 1, dbfs: Some(-120.0), packets: 50 }];
        }
        c.event("answered");
        let sip = vec![SipMessage { direction: "in".into(), first_line: "INVITE sip:1001@x SIP/2.0".into(), call_id: c.id.clone(), ..Default::default() }];
        save_call(dir.path(), &c, sip.clone()).unwrap();

        let (loaded, errs) = load_calls(dir.path());
        assert!(errs.is_empty());
        assert_eq!(loaded.len(), 1);
        let snap = loaded[0].snapshot();
        assert_eq!((snap.id.as_str(), snap.state, snap.codec.as_str(), snap.mode), (c.id.as_str(), CallState::Ended, "PCMU", Some(Mode::Echo)));
        assert!(snap.audio.inbound_audio_heard);
        assert_eq!((snap.audio.packets_out, snap.events.len()), (50, 1));
        let msgs = loaded[0].sip(&SipTrace::default());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].first_line, sip[0].first_line);
        assert!(dir.path().join("abc_host_1").join("call.json").exists(), "call.json in the sanitized dir");
    }

    #[test]
    fn load_calls_skips_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bad")).unwrap();
        std::fs::write(dir.path().join("bad").join("call.json"), "{").unwrap();
        let (calls, errs) = load_calls(dir.path());
        assert_eq!((calls.len(), errs.len()), (0, 1));
        let (calls, errs) = load_calls(&dir.path().join("missing"));
        assert!(calls.is_empty() && errs.is_empty());
    }
}
