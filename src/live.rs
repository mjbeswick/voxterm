//! Gemini Live client: one bidirectional WebSocket per session. Audio is PCM16 LE.
//! Input 16 kHz, output 24 kHz. The API key goes in a header, never in the URL.

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const WS_URL: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const SYSTEM: &str = "You are voxterm, a voice assistant. Keep spoken replies short. \
For anything that changes files, runs commands, or touches code, call ask_coding_agent with a complete task. \
Use googleSearch for facts on the web. When a [coding agent finished] message arrives, summarize it in one or two sentences.";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub model: String,
    pub voice: String,
}

#[derive(Debug)]
pub enum LiveCmd {
    Audio(Vec<i16>),
    Text(String),
    ToolResult {
        id: String,
        name: String,
        result: Value,
    },
    Reconfigure(Settings),
}

#[derive(Debug)]
pub enum LiveEvent {
    Status(String),
    Audio(Vec<i16>),
    User(String),
    Assistant(String),
    Call {
        id: String,
        name: String,
        args: Value,
    },
    Interrupted,
    TurnComplete,
}

fn tools() -> Value {
    json!([
        {"functionDeclarations": [
            {"name": "ask_coding_agent", "description": "Give a coding task to the coding agent. It runs in the background. Use for any change to files, code, or commands. Returns immediately; the result arrives later.",
             "parameters": {"type": "OBJECT", "properties": {"task": {"type": "STRING", "description": "Complete task description"}}, "required": ["task"]}},
            {"name": "agent_status", "description": "Check whether the coding agent is still working.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "cancel_agent", "description": "Stop the coding agent's current task.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "answer_permission", "description": "Approve or deny the agent's pending action. Voice may only approve file edits; other actions need the user's screen confirmation.",
             "parameters": {"type": "OBJECT", "properties": {"allow": {"type": "BOOLEAN"}}, "required": ["allow"]}}
        ]},
        {"googleSearch": {}}
    ])
}

fn setup(s: &Settings) -> Value {
    json!({"setup": {
        "model": format!("models/{}", s.model),
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": s.voice}}}
        },
        "systemInstruction": {"parts": [{"text": SYSTEM}]},
        "tools": tools(),
        "inputAudioTranscription": {},
        "outputAudioTranscription": {}
    }})
}

fn encode(cmd: &LiveCmd) -> Option<Value> {
    Some(match cmd {
        LiveCmd::Audio(s) => {
            let bytes: Vec<u8> = s.iter().flat_map(|x| x.to_le_bytes()).collect();
            json!({"realtimeInput": {"audio": {"mimeType": "audio/pcm;rate=16000", "data": B64.encode(bytes)}}})
        }
        LiveCmd::Text(t) => {
            json!({"clientContent": {"turns": [{"role": "user", "parts": [{"text": t}]}], "turnComplete": true}})
        }
        LiveCmd::ToolResult { id, name, result } => {
            json!({"toolResponse": {"functionResponses": [{"id": id, "name": name, "response": {"result": result}}]}})
        }
        LiveCmd::Reconfigure(_) => return None,
    })
}

fn parse(m: &Message) -> Option<Value> {
    match m {
        Message::Text(t) => serde_json::from_str(t.as_str()).ok(),
        Message::Binary(b) => serde_json::from_slice(b).ok(),
        _ => None,
    }
}

/// Turns one server message into app events.
pub fn handle(v: &Value) -> Vec<LiveEvent> {
    let mut out = vec![];
    if let Some(sc) = v.get("serverContent") {
        for part in sc["modelTurn"]["parts"].as_array().into_iter().flatten() {
            if let Some(data) = part["inlineData"]["data"].as_str() {
                if let Ok(bytes) = B64.decode(data) {
                    out.push(LiveEvent::Audio(
                        bytes
                            .chunks_exact(2)
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect(),
                    ));
                }
            }
        }
        if let Some(t) = sc["outputTranscription"]["text"].as_str() {
            out.push(LiveEvent::Assistant(t.to_string()));
        }
        if let Some(t) = sc["inputTranscription"]["text"].as_str() {
            out.push(LiveEvent::User(t.to_string()));
        }
        if sc["interrupted"].as_bool() == Some(true) {
            out.push(LiveEvent::Interrupted);
        }
        if sc["turnComplete"].as_bool() == Some(true) {
            out.push(LiveEvent::TurnComplete);
        }
    }
    for c in v["toolCall"]["functionCalls"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push(LiveEvent::Call {
            id: c["id"].as_str().unwrap_or_default().to_string(),
            name: c["name"].as_str().unwrap_or_default().to_string(),
            args: c["args"].clone(),
        });
    }
    out
}

async fn connect(key: &str, s: &Settings) -> Result<Ws> {
    let mut req = WS_URL.into_client_request()?;
    req.headers_mut().insert("x-goog-api-key", key.parse()?);
    let (mut ws, _) = connect_async(req).await?;
    ws.send(Message::Text(setup(s).to_string().into())).await?;
    loop {
        match ws.next().await {
            Some(Ok(m)) => {
                if parse(&m).is_some_and(|v| v.get("setupComplete").is_some()) {
                    return Ok(ws);
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => return Err(anyhow!("closed during setup")),
        }
    }
}

/// Keeps a session alive. Reconnects on failure. Returns when the command channel closes.
pub async fn run(
    key: String,
    mut settings: Settings,
    mut cmds: mpsc::UnboundedReceiver<LiveCmd>,
    events: mpsc::UnboundedSender<LiveEvent>,
) {
    loop {
        let _ = events.send(LiveEvent::Status("live: connecting".into()));
        match session(&key, &settings, &mut cmds, &events).await {
            Ok(Some(next)) => settings = next,
            Ok(None) => return,
            Err(e) => {
                let _ = events.send(LiveEvent::Status(format!("live: {e}; retrying")));
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn session(
    key: &str,
    s: &Settings,
    cmds: &mut mpsc::UnboundedReceiver<LiveCmd>,
    events: &mpsc::UnboundedSender<LiveEvent>,
) -> Result<Option<Settings>> {
    let ws = connect(key, s).await?;
    let _ = events.send(LiveEvent::Status(format!(
        "live: {} / {}",
        s.model, s.voice
    )));
    let (mut tx, mut rx) = ws.split();
    loop {
        tokio::select! {
            m = rx.next() => match m {
                Some(Ok(m)) => {
                    if let Some(v) = parse(&m) {
                        for e in handle(&v) {
                            let _ = events.send(e);
                        }
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Err(anyhow!("connection closed")),
            },
            c = cmds.recv() => match c {
                None => return Ok(None),
                Some(LiveCmd::Reconfigure(next)) => return Ok(Some(next)),
                Some(cmd) => {
                    if let Some(v) = encode(&cmd) {
                        tx.send(Message::Text(v.to_string().into())).await?;
                    }
                }
            }
        }
    }
}

/// One-shot: sends `text`, returns all audio up to turnComplete. Used for voice previews and selftest.
pub async fn once(key: &str, s: &Settings, text: &str) -> Result<Vec<i16>> {
    let mut ws = connect(key, s).await?;
    let msg = encode(&LiveCmd::Text(text.to_string())).ok_or_else(|| anyhow!("encode"))?;
    ws.send(Message::Text(msg.to_string().into())).await?;
    let mut audio = vec![];
    loop {
        let next = timeout(Duration::from_secs(60), ws.next()).await?;
        let m = next.ok_or_else(|| anyhow!("closed before turn complete"))??;
        if let Some(v) = parse(&m) {
            for e in handle(&v) {
                match e {
                    LiveEvent::Audio(s) => audio.extend(s),
                    LiveEvent::TurnComplete => return Ok(audio),
                    _ => {}
                }
            }
        }
    }
}

/// Live-capable model names from the models endpoint.
pub async fn list_models(key: &str) -> Result<Vec<String>> {
    let v: Value = reqwest::Client::new()
        .get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=200")
        .header("x-goog-api-key", key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["supportedGenerationMethods"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some("bidiGenerateContent")))
        })
        .filter_map(|m| {
            m["name"]
                .as_str()
                .map(|n| n.trim_start_matches("models/").to_string())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_reads_audio_transcript_and_tool_call() {
        let audio = B64.encode([1u8, 0, 255, 127]);
        let v = json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": audio}}]}, "turnComplete": true}});
        let ev = handle(&v);
        assert!(matches!(&ev[0], LiveEvent::Audio(s) if s == &vec![1i16, 32767]));
        assert!(matches!(ev[1], LiveEvent::TurnComplete));

        let call = json!({"toolCall": {"functionCalls": [{"id": "fc1", "name": "agent_status", "args": {}}]}});
        assert!(
            matches!(&handle(&call)[0], LiveEvent::Call { name, .. } if name == "agent_status")
        );
    }
}
