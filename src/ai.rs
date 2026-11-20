//! `Mode::Ai`: the call talks to an xAI realtime voice agent.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::audio::{downsample3, i16s_from_le, i16s_to_le, upsample3};
use crate::call::Call;
use crate::phone::Phone;

const XAI_REALTIME_URL: &str = "wss://api.x.ai/v1/realtime";

/// Suits the destination of a warm transfer.
pub const DEFAULT_INSTRUCTIONS: &str = "You are a person answering a business phone line. Greet the caller briefly
when the call connects. Keep every reply short and natural, like a real person on the phone.
If someone briefs you about a caller and asks whether you can take the call, say yes.";

pub const VOICES: [&str; 5] = ["ara", "eve", "rex", "sal", "leo"];

impl Phone {
    /// Connects the call to an xAI realtime session, unless one is running.
    pub fn start_ai(self: &Arc<Self>, c: &Arc<Call>) {
        let mut g = c.lock();
        if g.ai_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let instructions = if g.ai_instructions.is_empty() { DEFAULT_INSTRUCTIONS.to_string() } else { g.ai_instructions.clone() };
        let voice = if g.ai_voice.is_empty() { self.cfg.xai_voice.clone() } else { g.ai_voice.clone() };
        let (me, call) = (self.clone(), c.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = me.run_ai(&call, &instructions, &voice).await {
                call.event(format!("ai session ended: {e}"));
            }
        });
        g.ai_task = Some(task.abort_handle());
    }

    pub fn stop_ai(&self, c: &Call) {
        if let Some(task) = c.lock().ai_task.take() {
            task.abort();
            c.ai_out.clear();
        }
    }

    async fn run_ai(&self, c: &Arc<Call>, instructions: &str, voice: &str) -> anyhow::Result<()> {
        if self.cfg.xai_key.is_empty() {
            anyhow::bail!("XAI_API_KEY is not set");
        }
        let mut req = format!("{XAI_REALTIME_URL}?model={}", self.cfg.xai_model).into_client_request()?;
        req.headers_mut().insert("Authorization", format!("Bearer {}", self.cfg.xai_key).parse()?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| anyhow::anyhow!("connect: {e}"))?;
        let (mut sink, mut stream) = ws.split();

        sink.send(Message::text(session_update(instructions, voice).to_string())).await?;
        // Speak first: the far end of a warm transfer waits for the destination.
        sink.send(Message::text(json!({"type": "response.create"}).to_string())).await?;
        c.event(format!("ai session started ({}, voice {voice})", self.cfg.xai_model));

        let (sub, mut frames) = c.inbound.subscribe();
        let writer = tokio::spawn(async move {
            let (mut prev, mut buf) = (0i16, Vec::new());
            while let Some(frame) = frames.recv().await {
                let up = upsample3(&frame, prev);
                if let Some(last) = frame.last() {
                    prev = *last;
                }
                i16s_to_le(&up, &mut buf);
                let msg = json!({"type": "input_audio_buffer.append", "audio": B64.encode(&buf)});
                if sink.send(Message::text(msg.to_string())).await.is_err() {
                    return;
                }
            }
        });
        let _cleanup = Cleanup(c.clone(), sub, writer.abort_handle());

        let mut rest: Vec<i16> = Vec::new();
        // xAI sends a transcript for an utterance more than once as it firms up; keep one line per item.
        let mut heard: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut last_heard: Option<(usize, String)> = None;
        while let Some(msg) = stream.next().await {
            let Message::Text(text) = msg? else { continue };
            let ev: Value = serde_json::from_str(&text)?;
            let typ = ev["type"].as_str().unwrap_or_default();
            match typ {
                "response.output_audio.delta" | "response.audio.delta" => {
                    let Ok(raw) = B64.decode(ev["delta"].as_str().unwrap_or_default()) else { continue };
                    rest.extend(i16s_from_le(&raw));
                    let (out, r) = downsample3(&rest);
                    rest = r;
                    c.ai_out.push(&out);
                }
                // The other side started talking: stop speaking over them.
                "input_audio_buffer.speech_started" => c.ai_out.clear(),
                "response.output_audio_transcript.done" | "response.audio_transcript.done" => {
                    if let Some(t) = transcript(&ev) {
                        c.event(format!("ai said: {t}"));
                    }
                }
                "error" => c.event(format!("ai error: {}", ev["error"])),
                _ if typ.starts_with("conversation.item.input_audio_transcription.") && typ.ends_with(".completed") => {
                    if let Some(t) = transcript(&ev) {
                        let id = ev["item_id"].as_str().filter(|id| !id.is_empty()).map(str::to_string);
                        let again = id.as_ref().and_then(|id| heard.get(id).copied());
                        let events = c.lock().events.len();
                        let (replace, text) = place_heard(t, again, last_heard.as_ref(), events);
                        let line = format!("heard: {text}");
                        let index = match replace {
                            Some(i) => {
                                c.replace_event(i, line);
                                i
                            }
                            None => c.event_at(line),
                        };
                        if let Some(id) = id {
                            heard.insert(id, index);
                        }
                        last_heard = Some((index, text));
                    }
                }
                _ => {}
            }
        }
        anyhow::bail!("connection closed")
    }
}

struct Cleanup(Arc<Call>, u64, tokio::task::AbortHandle);

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.inbound.unsubscribe(self.1);
        self.2.abort();
    }
}

/// Where a caller transcript goes: over the line for the same item, over the line just added
/// when one text extends the other (keeping the longer), or on a new line (`None`).
fn place_heard(t: String, again: Option<usize>, last: Option<&(usize, String)>, events: usize) -> (Option<usize>, String) {
    if again.is_some() {
        return (again, t);
    }
    match last {
        Some((i, prev)) if events == i + 1 && (t.starts_with(prev.as_str()) || prev.starts_with(t.as_str())) => {
            (Some(*i), if prev.len() > t.len() { prev.clone() } else { t })
        }
        _ => (None, t),
    }
}

fn transcript(ev: &Value) -> Option<String> {
    let t = ev["transcript"].as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub fn session_update(instructions: &str, voice: &str) -> Value {
    let pcm24k = json!({"type": "audio/pcm", "rate": 24000});
    json!({
        "type": "session.update",
        "session": {
            "voice": voice,
            "instructions": instructions,
            "turn_detection": {"type": "server_vad", "silence_duration_ms": 700},
            "audio": {
                "input": {"format": pcm24k, "transport": "json", "transcription": {"model": "grok-transcribe"}},
                "output": {"format": pcm24k, "transport": "json"},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::place_heard;

    #[test]
    fn merges_repeated_and_growing_transcripts() {
        let last = (4, "Uh, you are".to_string());
        assert_eq!(place_heard("Uh, you are calling me.".into(), None, Some(&last), 5), (Some(4), "Uh, you are calling me.".into()));
        let last = (4, "Uh, you are calling me.".to_string());
        assert_eq!(place_heard("Uh, you are calling me.".into(), None, Some(&last), 5), (Some(4), "Uh, you are calling me.".into()));
        assert_eq!(place_heard("Uh, you are".into(), None, Some(&last), 5), (Some(4), "Uh, you are calling me.".into()));
        // The agent spoke in between: a new line, even if the words repeat.
        assert_eq!(place_heard("Hello?".into(), None, Some(&(2, "Hello?".to_string())), 4), (None, "Hello?".into()));
        assert_eq!(place_heard("Something else".into(), Some(7), None, 9), (Some(7), "Something else".into()));
    }
    #[test]
    fn session_update_uses_pcm_24k() {
        let s = super::session_update("be brief", "ara").to_string();
        for want in [r#""type":"session.update""#, r#""rate":24000"#, r#""voice":"ara""#, r#""instructions":"be brief""#, "grok-transcribe"] {
            assert!(s.contains(want), "session.update lacks {want}: {s}");
        }
    }
}
