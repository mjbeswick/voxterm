//! ACP hub: one worker process, many sessions. Every event names the session it belongs to.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{mpsc, oneshot},
};

#[derive(Debug)]
pub enum Event {
    Models(Vec<(String, String)>),
    Text { sid: String, text: String },
    Tool { sid: String, line: String },
    Permission { sid: String, rpc_id: Value, title: String, kind: String, options: Vec<(String, String)> },
    Done { sid: String, stop: String, text: String },
    Error { sid: String, msg: String },
    Info(String),
}

#[derive(Debug)]
pub enum Cmd {
    NewSession(oneshot::Sender<Result<String, String>>),
    Prompt { sid: String, text: String },
    Cancel { sid: String },
    Answer { rpc_id: Value, option_id: String },
    SetModel(String),
}

enum Pending {
    Init,
    New(oneshot::Sender<Result<String, String>>),
    Prompt(String),
    Config,
}

struct Conn {
    stdin: ChildStdin,
    next: i64,
    pending: HashMap<i64, Pending>,
}

impl Conn {
    async fn write(&mut self, v: Value) -> Result<()> {
        self.stdin.write_all(format!("{v}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn call(&mut self, method: &str, params: Value, p: Pending) -> Result<()> {
        let id = self.next;
        self.next += 1;
        self.pending.insert(id, p);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
    }
}

/// Starts the worker process and runs the handshake in the background.
pub async fn start(cmd: &[String], cwd: &Path) -> Result<(mpsc::UnboundedSender<Cmd>, mpsc::UnboundedReceiver<Event>)> {
    let (prog, args) = cmd.split_first().ok_or_else(|| anyhow!("empty worker command"))?;
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("cannot start {prog}: {e}"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no worker stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no worker stdout"))?;
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Event>();
    let cwd_s = cwd.display().to_string();

    tokio::spawn(async move {
        let _child = child; // kill_on_drop when this task ends
        let mut lines = BufReader::new(stdout).lines();
        let mut conn = Conn { stdin, next: 1, pending: HashMap::new() };
        let mut texts: HashMap<String, String> = HashMap::new();
        let mut models_sent = false;
        let mut model: Option<String> = None;

        if conn
            .call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}}), Pending::Init)
            .await
            .is_err()
        {
            let _ = ev_tx.send(Event::Error { sid: String::new(), msg: "handshake failed".into() });
            return;
        }

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let line = match line { Ok(Some(l)) => l, _ => break };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(method) = v.get("method").and_then(Value::as_str) {
                        if let Some(id) = v.get("id") {
                            if method == "session/request_permission" {
                                let p = &v["params"];
                                let options = p["options"].as_array().into_iter().flatten()
                                    .map(|o| (o["optionId"].as_str().unwrap_or_default().to_string(), o["kind"].as_str().unwrap_or_default().to_string()))
                                    .collect();
                                let _ = ev_tx.send(Event::Permission {
                                    sid: p["sessionId"].as_str().unwrap_or_default().to_string(),
                                    rpc_id: id.clone(),
                                    title: p["toolCall"]["title"].as_str().unwrap_or("action").to_string(),
                                    kind: p["toolCall"]["kind"].as_str().unwrap_or("other").to_string(),
                                    options,
                                });
                            } else {
                                // No fs or terminal capabilities advertised. Refuse everything else.
                                let _ = conn.write(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported by voxterm"}})).await;
                            }
                        } else if method == "session/update" {
                            let sid = v["params"]["sessionId"].as_str().unwrap_or_default().to_string();
                            let u = &v["params"]["update"];
                            match u["sessionUpdate"].as_str() {
                                Some("agent_message_chunk") => {
                                    if let Some(t) = u["content"]["text"].as_str() {
                                        texts.entry(sid.clone()).or_default().push_str(t);
                                        let _ = ev_tx.send(Event::Text { sid, text: t.to_string() });
                                    }
                                }
                                Some("tool_call") | Some("tool_call_update") => {
                                    if let Some(line) = tool_line(u) {
                                        let _ = ev_tx.send(Event::Tool { sid, line });
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else if let Some(id) = v.get("id").and_then(Value::as_i64) {
                        let err = v["error"]["message"].as_str().map(String::from);
                        match conn.pending.remove(&id) {
                            Some(Pending::New(reply)) => match err {
                                Some(e) => { let _ = reply.send(Err(e)); }
                                None => {
                                    let sid = v["result"]["sessionId"].as_str().unwrap_or_default().to_string();
                                    if !models_sent {
                                        models_sent = true;
                                        let _ = ev_tx.send(Event::Models(model_options(&v["result"])));
                                    }
                                    if let Some(m) = &model {
                                        let _ = conn.call("session/set_config_option", json!({"sessionId": sid, "configId": "model", "value": m}), Pending::Config).await;
                                    }
                                    let _ = reply.send(Ok(sid));
                                }
                            },
                            Some(Pending::Prompt(sid)) => match err {
                                Some(e) => { let _ = ev_tx.send(Event::Error { sid, msg: e }); }
                                None => {
                                    let stop = v["result"]["stopReason"].as_str().unwrap_or("unknown").to_string();
                                    let text = texts.remove(&sid).unwrap_or_default();
                                    let _ = ev_tx.send(Event::Done { sid, stop, text });
                                }
                            },
                            Some(Pending::Config) => match err {
                                Some(e) => { let _ = ev_tx.send(Event::Info(format!("model change failed: {e}"))); }
                                None => { let _ = ev_tx.send(Event::Info("model set".into())); }
                            },
                            Some(Pending::Init) | None => {}
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let r = match cmd {
                        Cmd::NewSession(reply) => {
                            conn.call("session/new", json!({"cwd": cwd_s, "mcpServers": []}), Pending::New(reply)).await
                        }
                        Cmd::Prompt { sid, text } => {
                            texts.insert(sid.clone(), String::new());
                            conn.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}), Pending::Prompt(sid)).await
                        }
                        Cmd::Cancel { sid } => conn.write(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})).await,
                        Cmd::Answer { rpc_id, option_id } => {
                            conn.write(json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"outcome": {"outcome": "selected", "optionId": option_id}}})).await
                        }
                        Cmd::SetModel(value) => {
                            model = Some(value);
                            Ok(())
                        }
                    };
                    if r.is_err() { break }
                }
            }
        }
        let _ = ev_tx.send(Event::Error { sid: String::new(), msg: "worker process exited".into() });
    });

    Ok((cmd_tx, ev_rx))
}

/// Model choices from a session/new result.
fn model_options(result: &Value) -> Vec<(String, String)> {
    result["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["id"].as_str() == Some("model"))
        .map(|o| {
            o["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    (
                        m["value"].as_str().unwrap_or_default().to_string(),
                        m["name"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One-line status for a tool_call or tool_call_update. Quiet on intermediate updates.
fn tool_line(u: &Value) -> Option<String> {
    let title = u["title"].as_str().unwrap_or("tool");
    match (u["sessionUpdate"].as_str(), u["status"].as_str()) {
        (Some("tool_call"), _) => Some(title.to_string()),
        (_, Some(s @ ("completed" | "failed"))) => Some(format!("{title} {s}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_line_reports_start_and_finish_only() {
        let start = json!({"sessionUpdate": "tool_call", "title": "write"});
        let progress = json!({"sessionUpdate": "tool_call_update", "title": "write", "status": "in_progress"});
        let done = json!({"sessionUpdate": "tool_call_update", "title": "write", "status": "completed"});
        assert_eq!(tool_line(&start).as_deref(), Some("write"));
        assert_eq!(tool_line(&progress), None);
        assert_eq!(tool_line(&done).as_deref(), Some("write completed"));
    }
}
