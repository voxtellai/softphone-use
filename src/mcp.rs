//! MCP over stdio as a thin client of a running `softphone serve`, so the phone keeps its
//! registration and calls across MCP restarts. JSON-RPC 2.0, one message per line.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

const MODE_DESCRIPTION: &str = "silent: send silence. tone: steady 440 Hz. echo: send back what is received. human: the person at the softphone talks through this computer's microphone and speaker. ai: an xAI realtime voice agent talks; its transcript is logged as call events.";

fn tools() -> Value {
    let call_id = json!({"type": "string", "description": "Call id (the SIP Call-ID) from wait_for_call or list_calls."});
    let mode = json!({"type": "string", "enum": ["silent", "tone", "echo", "human", "ai"], "description": MODE_DESCRIPTION});
    let instructions =
        json!({"type": "string", "description": "For mode ai: how the agent should behave, e.g. \"You are billing; accept the transfer after the briefing.\""});
    let voice = json!({"type": "string", "description": "For mode ai: xAI voice, e.g. ara or eve."});
    let tool = |name: &str, description: &str, props: Value, required: &[&str]| json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": props, "required": required}});
    json!([
        tool("status", "Registration state of the softphone device and the number of active calls.", json!({}), &[]),
        tool("list_calls", "All calls, newest first, with state, codecs, RTP addresses and an audio summary.", json!({}), &[]),
        tool(
            "wait_for_call",
            "Block until a call is ringing and return it (caller, offered SDP). Answer it with `answer`.",
            json!({"timeout_seconds": {"type": "number", "description": "How long to wait. Default 60."}}),
            &[]
        ),
        tool(
            "answer",
            "Answer a ringing call in the given mode, or switch the mode of an answered call.",
            json!({"call_id": call_id, "mode": mode, "instructions": instructions, "voice": voice}),
            &["call_id"]
        ),
        tool(
            "dial",
            "Place a call as the device to an extension or number. The PBX routes it with its dial plan. Follow it with get_call.",
            json!({"to": {"type": "string", "description": "Extension or number, e.g. 001 or 14075551234."}, "mode": mode, "instructions": instructions, "voice": voice}),
            &["to"]
        ),
        tool("hangup", "Reject a ringing call or hang up an answered one.", json!({"call_id": call_id}), &["call_id"]),
        tool(
            "get_call",
            "One call: state, offered and negotiated codec, RTP addresses, events, and an audio summary that flags no RTP, digital silence or no inbound sound.",
            json!({"call_id": call_id}),
            &["call_id"]
        ),
        tool(
            "get_levels",
            "Per-second audio level (dBFS) and packet count, inbound and outbound. -120 means exact zeros; packets 0 means no RTP that second.",
            json!({"call_id": call_id}),
            &["call_id"]
        ),
        tool("get_sip", "Every SIP message of a call as sent and received, including SDP bodies and re-INVITEs.", json!({"call_id": call_id}), &["call_id"]),
        tool(
            "recent_sip",
            "The last SIP messages across all dialogs, including REGISTER. Use it to debug registration.",
            json!({"count": {"type": "number", "description": "How many messages. Default 30."}}),
            &[]
        ),
    ])
}

/// The API request a tool call makes: method, path and body.
fn route(name: &str, a: &Value) -> anyhow::Result<(&'static str, String, Option<Value>)> {
    let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
    let call = |action: &str| {
        let id: String = url_escape(&s("call_id"));
        if action.is_empty() { format!("/api/calls/{id}") } else { format!("/api/calls/{id}/{action}") }
    };
    let mode_body = |extra: Value| {
        let mut b = json!({"mode": a["mode"].as_str().unwrap_or("silent"), "instructions": s("instructions"), "voice": s("voice")});
        if let (Some(b), Some(e)) = (b.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        b
    };
    Ok(match name {
        "status" => ("GET", "/api/status".into(), None),
        "list_calls" => ("GET", "/api/calls".into(), None),
        "wait_for_call" => ("POST", format!("/api/wait?timeout={}", a["timeout_seconds"].as_u64().unwrap_or(60)), None),
        "answer" => ("POST", call("answer"), Some(mode_body(json!({})))),
        "dial" => ("POST", "/api/dial".into(), Some(mode_body(json!({"to": s("to")})))),
        "hangup" => ("POST", call("hangup"), None),
        "get_call" => ("GET", call(""), None),
        "get_levels" => ("GET", call("levels"), None),
        "get_sip" => ("GET", call("sip"), None),
        "recent_sip" => ("GET", format!("/api/sip?n={}", a["count"].as_u64().unwrap_or(30)), None),
        _ => anyhow::bail!("unknown tool {name}"),
    })
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub async fn run(base: String) -> anyhow::Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let out = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    while let Some(line) = rx.recv().await {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (http, base, out) = (http.clone(), base.clone(), out.clone());
        // Tool calls such as wait_for_call block; answer each request on its own task.
        tokio::spawn(async move {
            if let Some(res) = handle(&http, &base, req).await {
                let mut o = out.lock().unwrap();
                let _ = writeln!(o, "{res}");
                let _ = o.flush();
            }
        });
    }
    Ok(())
}

async fn handle(http: &reqwest::Client, base: &str, req: Value) -> Option<Value> {
    let id = req.get("id")?.clone();
    let method = req["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "softphone-use", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or_default();
            let args = &req["params"]["arguments"];
            match call_tool(http, base, name, args).await {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}),
            }
        }
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

async fn call_tool(http: &reqwest::Client, base: &str, name: &str, args: &Value) -> anyhow::Result<String> {
    let (method, path, body) = route(name, args)?;
    let mut req = http.request(method.parse()?, format!("{base}{path}"));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.map_err(|e| anyhow::anyhow!("softphone daemon not reachable at {base} (start it with `softphone serve`): {e}"))?;
    let status = res.status();
    let text = res.text().await?;
    if status.as_u16() >= 400 {
        anyhow::bail!("{method} {path}: {}", text.trim());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_tools_to_the_api() {
        let (m, p, b) = route("answer", &json!({"call_id": "a@b/c", "mode": "echo"})).unwrap();
        assert_eq!((m, p.as_str()), ("POST", "/api/calls/a@b%2Fc/answer"));
        assert_eq!(b.unwrap()["mode"], "echo");
        let (_, p, b) = route("dial", &json!({"to": "001"})).unwrap();
        assert_eq!(p, "/api/dial");
        let b = b.unwrap();
        assert_eq!((b["to"].as_str(), b["mode"].as_str()), (Some("001"), Some("silent")));
        assert_eq!(route("recent_sip", &json!({})).unwrap().1, "/api/sip?n=30");
        assert!(route("robot", &json!({})).is_err());
        assert_eq!(tools().as_array().unwrap().len(), 10);
    }
}
