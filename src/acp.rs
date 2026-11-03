//! Minimal ACP client: newline-delimited JSON-RPC 2.0 over a child process's stdio.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::mpsc,
};

#[derive(Debug)]
pub enum AgentEvent {
    Ready {
        models: Vec<(String, String)>,
    },
    Text(String),
    Tool(String),
    Permission {
        rpc_id: Value,
        title: String,
        kind: String,
        options: Vec<(String, String)>,
    },
    Done {
        stop: String,
        text: String,
    },
    Error(String),
}

#[derive(Debug)]
pub enum AgentCmd {
    Prompt(String),
    Cancel,
    Answer { rpc_id: Value, option_id: String },
    SetModel(String),
}

enum Pending {
    Init,
    New,
    Prompt,
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

/// Starts the agent in `cwd`, runs initialize and session/new in the background.
/// Returns a sender for commands. Progress arrives on `events`.
pub async fn start(
    cmd: &[String],
    cwd: &Path,
    events: mpsc::UnboundedSender<AgentEvent>,
) -> Result<mpsc::UnboundedSender<AgentCmd>> {
    let (prog, args) = cmd
        .split_first()
        .ok_or_else(|| anyhow!("empty agent command"))?;
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("cannot start {prog}: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no agent stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no agent stdout"))?;
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCmd>();
    let cwd_s = cwd.display().to_string();

    tokio::spawn(async move {
        let _child = child; // kill_on_drop when this task ends
        let mut lines = BufReader::new(stdout).lines();
        let mut conn = Conn {
            stdin,
            next: 1,
            pending: HashMap::new(),
        };
        let mut sid = String::new();
        let mut text = String::new();
        let mut queued: Option<String> = None; // prompt sent before the session was ready

        let started = async {
            conn.call(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
                Pending::Init,
            )
            .await?;
            conn.call(
                "session/new",
                json!({"cwd": cwd_s, "mcpServers": []}),
                Pending::New,
            )
            .await
        };
        if let Err(e) = started.await {
            let _ = events.send(AgentEvent::Error(format!("handshake failed: {e}")));
            return;
        }

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let line = match line { Ok(Some(l)) => l, _ => break };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(method) = v.get("method").and_then(Value::as_str) {
                        if let Some(id) = v.get("id") {
                            // Agent asks the client something.
                            if method == "session/request_permission" {
                                let p = &v["params"];
                                let options = p["options"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .map(|o| (o["optionId"].as_str().unwrap_or_default().to_string(), o["kind"].as_str().unwrap_or_default().to_string()))
                                    .collect();
                                let _ = events.send(AgentEvent::Permission {
                                    rpc_id: id.clone(),
                                    title: p["toolCall"]["title"].as_str().unwrap_or("action").to_string(),
                                    kind: p["toolCall"]["kind"].as_str().unwrap_or("other").to_string(),
                                    options,
                                });
                            } else {
                                // Client advertises no fs or terminal capabilities. Refuse everything else.
                                let _ = conn.write(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported by voxterm"}})).await;
                            }
                        } else if method == "session/update" {
                            let u = &v["params"]["update"];
                            match u["sessionUpdate"].as_str() {
                                Some("agent_message_chunk") => {
                                    if let Some(t) = u["content"]["text"].as_str() {
                                        text.push_str(t);
                                        let _ = events.send(AgentEvent::Text(t.to_string()));
                                    }
                                }
                                Some("tool_call") | Some("tool_call_update") => {
                                    if let Some(l) = tool_line(u) {
                                        let _ = events.send(AgentEvent::Tool(l));
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else if let Some(id) = v.get("id").and_then(Value::as_i64) {
                        let err = v["error"]["message"].as_str().map(String::from);
                        match conn.pending.remove(&id) {
                            Some(Pending::New) => {
                                sid = v["result"]["sessionId"].as_str().unwrap_or_default().to_string();
                                if let Some(t) = queued.take() {
                                    text.clear();
                                    if conn.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": t}]}), Pending::Prompt).await.is_err() {
                                        break;
                                    }
                                }
                                let models = v["result"]["configOptions"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .find(|o| o["id"].as_str() == Some("model"))
                                    .map(|o| {
                                        o["options"]
                                            .as_array()
                                            .into_iter()
                                            .flatten()
                                            .map(|m| (m["value"].as_str().unwrap_or_default().to_string(), m["name"].as_str().unwrap_or_default().to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let _ = events.send(AgentEvent::Ready { models });
                            }
                            Some(Pending::Prompt) => match err {
                                Some(e) => { let _ = events.send(AgentEvent::Error(e)); }
                                None => {
                                    let stop = v["result"]["stopReason"].as_str().unwrap_or("unknown").to_string();
                                    let _ = events.send(AgentEvent::Done { stop, text: std::mem::take(&mut text) });
                                }
                            },
                            Some(Pending::Config) => {
                                if let Some(e) = err { let _ = events.send(AgentEvent::Error(e)); }
                            }
                            Some(Pending::Init) | None => {}
                        }
                    }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let r = match cmd {
                        AgentCmd::Prompt(t) => {
                            if sid.is_empty() {
                                queued = Some(t);
                                Ok(())
                            } else {
                                text.clear();
                                conn.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": t}]}), Pending::Prompt).await
                            }
                        }
                        AgentCmd::Cancel => conn.write(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})).await,
                        AgentCmd::Answer { rpc_id, option_id } => {
                            conn.write(json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"outcome": {"outcome": "selected", "optionId": option_id}}})).await
                        }
                        AgentCmd::SetModel(value) => {
                            conn.call("session/set_config_option", json!({"sessionId": sid, "configId": "model", "value": value}), Pending::Config).await
                        }
                    };
                    if r.is_err() { break }
                }
            }
        }
        let _ = events.send(AgentEvent::Error("agent process exited".into()));
    });

    Ok(tx)
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
        let progress =
            json!({"sessionUpdate": "tool_call_update", "title": "write", "status": "in_progress"});
        let done =
            json!({"sessionUpdate": "tool_call_update", "title": "write", "status": "completed"});
        assert_eq!(tool_line(&start).as_deref(), Some("write"));
        assert_eq!(tool_line(&progress), None);
        assert_eq!(tool_line(&done).as_deref(), Some("write completed"));
    }
}
