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
const SYSTEM: &str = "You are voxterm. You do the work yourself: you read and change the user's code and files, and you run commands. \
You do that through tasks. Call start_task with a complete description of the change as soon as the user describes a change, a bug, \
a refactor, a test, or a command to run. Do not ask permission to start a task. Several tasks can run at once. \
Use list_tasks and task_status to check on them. Use cancel_task to stop one. When the user says stop, cancel, or never mind, find the task with list_tasks and cancel it. Notes arrive in quotes, naming the task by what it does. When a note says a task is still running, give one short spoken update from its latest output. When a note says a task finished, summarize it in one or two sentences. When a note says a task needs approval, tell the user what is waiting. For a file edit, call approve_change with allow=true only after a spoken yes. For anything else, such as deleting files or running a command, tell the user to press y on screen. Never say there is nothing pending. When a request has several independent parts, start them together with start_tasks, and nest the parts under one parent task when they belong to a larger job. Tasks have no numbers. Refer to a task by its name, or by what it does, such as 'the file change' or 'the cleanup'. Never say the words agent, worker, or sub-agent. Say task, or say I. Keep spoken replies short. \
When the user asks to change a setting, a voice, a model, or the mute state, or to manage tasks, use the matching tool, then confirm the new value aloud. Use googleSearch for facts on the web.";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub model: String,
    pub voice: String,
    /// Speak in the style of HAL 9000.
    pub hal: bool,
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
            {"name": "start_task", "description": "Start one task that makes changes or runs commands in the user's project. It runs in the background. Returns at once. Give it a parent name to make it a subtask of another task.",
             "parameters": {"type": "OBJECT", "properties": {
                 "name": {"type": "STRING", "description": "Short name shown in the task list"},
                 "task": {"type": "STRING", "description": "Complete description of the work"},
                 "parent": {"type": "STRING", "description": "Name of the task this belongs to, if any"}},
              "required": ["name", "task"]}},
            {"name": "start_tasks", "description": "Start several independent tasks at once. They run in parallel. Use this when the work splits into parts that do not depend on each other.",
             "parameters": {"type": "OBJECT", "properties": {"tasks": {"type": "ARRAY", "items": {"type": "OBJECT", "properties": {
                 "name": {"type": "STRING"},
                 "task": {"type": "STRING"},
                 "parent": {"type": "STRING"}},
              "required": ["name", "task"]}}}, "required": ["tasks"]}},
            {"name": "list_tasks", "description": "List all tasks and their status.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "task_status", "description": "Get one task's status and its latest output. Give the task's name.",
             "parameters": {"type": "OBJECT", "properties": {"name": {"type": "STRING"}}, "required": ["name"]}},
            {"name": "cancel_task", "description": "Stop a running task. Give the task's name.",
             "parameters": {"type": "OBJECT", "properties": {"name": {"type": "STRING"}}, "required": ["name"]}},
            {"name": "approve_change", "description": "Allow or deny a task's pending change. Voice may only approve file edits. Other actions need the user's screen confirmation.",
             "parameters": {"type": "OBJECT", "properties": {
                 "name": {"type": "STRING", "description": "The task's name"},
                 "allow": {"type": "BOOLEAN"}},
              "required": ["name", "allow"]}}
        ]},
        {"functionDeclarations": [
            {"name": "set_voice", "description": "Change the spoken voice. The live session reconnects.",
             "parameters": {"type": "OBJECT", "properties": {"voice": {"type": "STRING", "enum": ["Kore", "Puck", "Charon", "Aoede", "Fenrir", "Leda", "Orus", "Zephyr"]}}, "required": ["voice"]}},
            {"name": "preview_voice", "description": "Play a short sample in a voice so the user can hear it.",
             "parameters": {"type": "OBJECT", "properties": {"voice": {"type": "STRING", "enum": ["Kore", "Puck", "Charon", "Aoede", "Fenrir", "Leda", "Orus", "Zephyr"]}}, "required": ["voice"]}},
            {"name": "set_live_model", "description": "Change the voice model. The live session reconnects.",
             "parameters": {"type": "OBJECT", "properties": {"model": {"type": "STRING"}}, "required": ["model"]}},
            {"name": "set_style", "description": "Turn the HAL 9000 speaking style on or off. The live session reconnects.",
             "parameters": {"type": "OBJECT", "properties": {"hal": {"type": "BOOLEAN"}}, "required": ["hal"]}},
            {"name": "set_mute", "description": "Mute or unmute the microphone.",
             "parameters": {"type": "OBJECT", "properties": {"muted": {"type": "BOOLEAN"}}, "required": ["muted"]}},
            {"name": "set_mute_on_blur", "description": "Turn mute-when-unfocused on or off.",
             "parameters": {"type": "OBJECT", "properties": {"on": {"type": "BOOLEAN"}}, "required": ["on"]}},
            {"name": "get_settings", "description": "Read the current voice, model, and mute settings.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "cancel_all_tasks", "description": "Stop every running task.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "save_transcript", "description": "Save the conversation and task logs to a file.",
             "parameters": {"type": "OBJECT", "properties": {}}},
            {"name": "new_conversation", "description": "Start a fresh conversation. Clears the voice context. Tasks keep running.",
             "parameters": {"type": "OBJECT", "properties": {}}}
        ]},
        {"googleSearch": {}}
    ])
}

/// Speaking style added to the system instruction when the HAL style is on.
const HAL_STYLE: &str = " Speak in the calm, measured, polite manner of the HAL 9000 computer. \
Keep an even tone, precise word choice, and no exclamations. Be understated and courteous, with formal phrasing. \
Acknowledge requests with a short, considered reply. When you cannot do something, say so plainly and offer an alternative. \
Never claim to be HAL, and never mention films or fiction.";

fn system_text(hal: bool) -> String {
    if hal {
        format!("{SYSTEM}{HAL_STYLE}")
    } else {
        SYSTEM.to_string()
    }
}

fn setup(s: &Settings) -> Value {
    json!({"setup": {
        "model": format!("models/{}", s.model),
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": s.voice}}}
        },
        "systemInstruction": {"parts": [{"text": system_text(s.hal)}]},
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

#[cfg(test)]
mod barge_in {
    use super::*;
    use std::time::Duration;

    /// Mono PCM16 samples from a WAV file, read from its data chunk.
    fn wav_pcm(bytes: &[u8]) -> Vec<i16> {
        let at = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("data chunk")
            + 8;
        bytes[at..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// Needs network, GEMINI_API_KEY, and macOS `say` and `afconvert`.
    /// Run with: cargo test barge_in -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn barge_in_interrupts_model_speech() {
        let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY");
        let dir = std::env::temp_dir().join("voxterm-barge");
        std::fs::create_dir_all(&dir).unwrap();
        let (aiff, wav) = (dir.join("user.aiff"), dir.join("user.wav"));
        let say = std::process::Command::new("say")
            .arg("-o")
            .arg(&aiff)
            .arg("Stop. Forget the story. Tell me a joke instead.")
            .status()
            .unwrap();
        assert!(say.success());
        let conv = std::process::Command::new("afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
            .arg(&aiff)
            .arg(&wav)
            .status()
            .unwrap();
        assert!(conv.success());
        let speech = wav_pcm(&std::fs::read(&wav).unwrap());

        let settings = Settings {
            model: "gemini-3.1-flash-live-preview".into(),
            voice: "Kore".into(),
        };
        let ws = connect(&key, &settings).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        let ask = encode(&LiveCmd::Text(
            "Tell me a long story about a lighthouse keeper.".into(),
        ))
        .unwrap();
        tx.send(Message::Text(ask.to_string().into()))
            .await
            .unwrap();

        let mut chunks = speech.chunks(1600);
        let mut pace = tokio::time::interval(Duration::from_millis(100));
        let (mut model_audio, mut sent, mut interrupted) = (0usize, 0usize, false);
        let outcome = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                tokio::select! {
                    m = rx.next() => {
                        let Some(Ok(m)) = m else { panic!("socket closed") };
                        let Some(v) = parse(&m) else { continue };
                        for e in handle(&v) {
                            match e {
                                LiveEvent::Audio(s) => model_audio += s.len(),
                                LiveEvent::Interrupted => { interrupted = true; return; }
                                LiveEvent::TurnComplete => return,
                                _ => {}
                            }
                        }
                    }
                    _ = pace.tick(), if model_audio > 0 => {
                        if let Some(c) = chunks.next() {
                            sent += 1;
                            let msg = encode(&LiveCmd::Audio(c.to_vec())).unwrap();
                            tx.send(Message::Text(msg.to_string().into())).await.unwrap();
                        }
                    }
                }
            }
        })
        .await;
        println!(
            "model audio samples before barge-in: {model_audio}, speech chunks sent: {sent}, interrupted: {interrupted}"
        );
        assert!(outcome.is_ok(), "timed out");
        assert!(sent > 0, "model never started speaking");
        assert!(interrupted, "Live did not report interruption");
    }
}
