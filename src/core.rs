//! Core: the voice session, the task manager, and a JSON-lines protocol to the UI.
//! The assistant does the work itself. Coding workers run as tasks in the background.
//! The UI never sees "agent" wording. Tasks are the only unit it shows.

use crate::acp::{self, Cmd as WCmd, Event as WEvent};
use crate::audio::AudioCmd;
use crate::live::{self, LiveCmd, LiveEvent, Settings};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};

pub const VOICES: [&str; 8] = [
    "Kore", "Puck", "Charon", "Aoede", "Fenrir", "Leda", "Orus", "Zephyr",
];

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub voice: String,
    pub live_model: String,
    pub agent_cmd: String,
    pub mute_on_blur: bool,
    pub echo_gate: Option<bool>,
    pub echo_tail_ms: u64,
    pub mic_min_rms: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            voice: "Kore".into(),
            live_model: "gemini-3.1-flash-live-preview".into(),
            agent_cmd: "opencode acp".into(),
            mute_on_blur: true,
            echo_gate: None,
            echo_tail_ms: 600,
            mic_min_rms: 0.0,
        }
    }
}

impl Config {
    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".config/voxterm/config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

enum In {
    Ui(Value),
    Live(LiveEvent),
    Worker(WEvent),
    Session {
        id: u32,
        text: String,
        sid: Result<String, String>,
    },
    LiveModels(Vec<String>),
    Preview(Vec<i16>),
}

struct Perm {
    rpc_id: Value,
    title: String,
    kind: String,
    options: Vec<(String, String)>,
}

struct Task {
    id: u32,
    title: String,
    status: &'static str,
    sid: Option<String>,
    log: Vec<String>,
    perm: Option<Perm>,
    started: Instant,
    last_update: Instant,
    /// Set once the first progress note has been spoken.
    noted: bool,
}

pub struct Core {
    cfg: Config,
    key: String,
    live_tx: UnboundedSender<LiveCmd>,
    worker: Option<UnboundedSender<WCmd>>,
    audio: std::sync::mpsc::Sender<AudioCmd>,
    out: UnboundedSender<Value>,
    in_tx: UnboundedSender<In>,
    tasks: Vec<Task>,
    next_id: u32,
    by_sid: HashMap<String, u32>,
    transcript: Vec<(String, String)>,
    open: bool,
    live_status: String,
    manual_mute: bool,
    focused: bool,
    last_audio: Option<Instant>,
    last_status: String,
    last_tasks: String,
    live_models: Vec<String>,
    worker_models: Vec<(String, String)>,
}

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    let mut t: Vec<char> = s.chars().rev().take(n).collect();
    t.reverse();
    t.into_iter().collect()
}

/// Runs the core until the UI sends quit. Reads commands from stdin and writes events to stdout.
/// Runs the core. `out` receives every event as JSON. `commands` carries UI commands as JSON.
/// The core stops when it gets a quit command, or when the command channel closes.
pub async fn run(
    cfg: Config,
    key: String,
    cwd: std::path::PathBuf,
    out: UnboundedSender<Value>,
    mut commands: UnboundedReceiver<Value>,
) -> Result<()> {
    let out_tx = out;
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<In>();

    // Commands from the UI.
    {
        let tx = in_tx.clone();
        tokio::spawn(async move {
            while let Some(v) = commands.recv().await {
                let _ = tx.send(In::Ui(v));
            }
            let _ = tx.send(In::Ui(json!({"c": "quit"})));
        });
    }

    // Live voice session.
    let (live_tx, live_rx) = mpsc::unbounded_channel::<LiveCmd>();
    let (live_ev_tx, mut live_ev_rx) = mpsc::unbounded_channel::<LiveEvent>();
    tokio::spawn(live::run(
        key.clone(),
        settings_of(&cfg),
        live_rx,
        live_ev_tx,
    ));
    forward(&mut live_ev_rx, in_tx.clone(), In::Live);

    // Audio thread. Errors become status lines.
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<String>();
    {
        let tx = in_tx.clone();
        tokio::spawn(async move {
            while let Some(e) = err_rx.recv().await {
                let _ = tx.send(In::Ui(json!({"c": "_note", "text": e})));
            }
        });
    }
    let audio = crate::audio::spawn(
        live_tx.clone(),
        err_tx.clone(),
        cfg.echo_gate,
        Duration::from_millis(cfg.echo_tail_ms),
        cfg.mic_min_rms,
    );

    // Task worker. A failure here only disables tasks.
    let worker = {
        let cmd: Vec<String> = cfg.agent_cmd.split_whitespace().map(String::from).collect();
        match acp::start(&cmd, &cwd).await {
            Ok((tx, mut rx)) => {
                forward(&mut rx, in_tx.clone(), In::Worker);
                Some(tx)
            }
            Err(e) => {
                let _ = err_tx.send(format!("tasks unavailable: {e}"));
                None
            }
        }
    };

    // Model list for the picker.
    {
        let tx = in_tx.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Ok(models) = live::list_models(&key).await {
                let _ = tx.send(In::LiveModels(models));
            }
        });
    }

    let mut core = Core {
        cfg,
        key,
        live_tx,
        worker,
        audio,
        out: out_tx,
        in_tx: in_tx.clone(),
        tasks: vec![],
        next_id: 1,
        by_sid: HashMap::new(),
        transcript: vec![],
        open: false,
        live_status: "live: connecting".into(),
        manual_mute: false,
        focused: true,
        last_audio: None,
        last_status: String::new(),
        last_tasks: String::new(),
        live_models: vec![],
        worker_models: vec![],
    };
    core.apply_mic();
    core.emit_status();
    core.emit_tasks();

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => {
                if core.on_in(ev) { break }
            }
            _ = tick.tick() => {
                core.emit_status();
                core.emit_levels();
                core.progress_updates();
            }
        }
    }
    core.emit(json!({"t": "bye"}));
    Ok(())
}

fn forward<T: Send + 'static>(
    rx: &mut UnboundedReceiver<T>,
    to: UnboundedSender<In>,
    wrap: fn(T) -> In,
) {
    let mut rx = std::mem::replace(rx, mpsc::unbounded_channel().1);
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            if to.send(wrap(e)).is_err() {
                break;
            }
        }
    });
}

fn settings_of(c: &Config) -> Settings {
    Settings {
        model: c.live_model.clone(),
        voice: c.voice.clone(),
    }
}

impl Core {
    fn emit(&self, v: Value) {
        let _ = self.out.send(v);
    }

    fn mic_wanted(&self) -> bool {
        !self.manual_mute && (self.focused || !self.cfg.mute_on_blur)
    }

    fn apply_mic(&self) {
        let _ = self.audio.send(AudioCmd::Mic(self.mic_wanted()));
    }

    fn reconfigure(&self) {
        let _ = self
            .live_tx
            .send(LiveCmd::Reconfigure(settings_of(&self.cfg)));
    }

    /// Adds transcript text. `cont` appends to the open entry of the same speaker.
    fn say(&mut self, who: &str, text: &str, cont: bool) {
        let cont = cont && self.open && self.transcript.last().is_some_and(|(w, _)| w == who);
        if cont {
            if let Some(last) = self.transcript.last_mut() {
                last.1.push_str(text);
            }
        } else {
            self.transcript.push((who.to_string(), text.to_string()));
        }
        self.open = cont || who != "sys" && who != "you";
        self.emit(json!({"t": "say", "who": who, "text": text, "cont": cont}));
    }

    fn close(&mut self) {
        self.open = false;
        self.emit(json!({"t": "close"}));
    }

    /// Speaks a short progress note for tasks that run long: once after 45 seconds, then every minute.
    fn progress_updates(&mut self) {
        let now = Instant::now();
        let mut notes = vec![];
        for t in self.tasks.iter_mut() {
            if t.status != "running" {
                continue;
            }
            let due = if t.noted {
                now.duration_since(t.last_update) >= Duration::from_secs(60)
            } else {
                now.duration_since(t.started) >= Duration::from_secs(45)
            };
            if due {
                t.noted = true;
                t.last_update = now;
                let secs = now.duration_since(t.started).as_secs();
                let latest = t
                    .log
                    .iter()
                    .rev()
                    .find(|l| !l.starts_with('['))
                    .map(|l| l.trim_start_matches("· ").to_string())
                    .unwrap_or_default();
                notes.push(format!(
                    "[task {} still running after {secs}s: {}. Latest output: {}]",
                    t.id,
                    t.title,
                    short(&latest, 200)
                ));
            }
        }
        for n in notes {
            self.notice(n);
        }
    }

    /// Drops finished, failed, and cancelled tasks from the list.
    fn clear_finished(&mut self) {
        self.tasks
            .retain(|t| matches!(t.status, "starting" | "running" | "needs_approval"));
        self.emit_tasks();
    }

    fn emit_levels(&self) {
        let (mic, out) = crate::audio::LEVELS
            .lock()
            .map(|l| *l)
            .unwrap_or((0.0, 0.0));
        self.emit(json!({"t": "levels", "mic": mic, "out": out}));
    }

    fn emit_status(&mut self) {
        let live_up =
            self.live_status.starts_with("live: ") && !self.live_status.contains("retrying");
        let speaking = self
            .last_audio
            .is_some_and(|t| t.elapsed() < Duration::from_millis(800));
        let state = if self.live_status.contains("retrying") {
            "retrying"
        } else if speaking {
            "speaking"
        } else if !self.mic_wanted() {
            "muted"
        } else if live_up {
            "listening"
        } else {
            "connecting"
        };
        let v = json!({
            "t": "status",
            "state": state,
            "live": self.live_status,
            "mic": self.mic_wanted(),
            "voice": self.cfg.voice,
            "live_model": self.cfg.live_model,
            "tasks_ready": self.worker.is_some(),
        });
        let s = v.to_string();
        if s != self.last_status {
            self.last_status = s;
            self.emit(v);
        }
    }

    fn emit_tasks(&mut self) {
        let list: Vec<Value> = self
            .tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                    "approval": t.perm.as_ref().map(|p| p.title.clone()),
                    "kind": t.perm.as_ref().map(|p| p.kind.clone()),
                    "log": t.log.iter().rev().take(200).rev().cloned().collect::<Vec<_>>(),
                })
            })
            .collect();
        let v = json!({"t": "tasks", "tasks": list});
        let s = v.to_string();
        if s != self.last_tasks {
            self.last_tasks = s;
            self.emit(v);
        }
    }

    fn notice(&self, text: String) {
        let _ = self.live_tx.send(LiveCmd::Text(text));
    }

    fn task_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id as u64 == id)
    }

    /// Starts a task. Its session is created first, then the prompt is sent.
    fn start_task(&mut self, title: String, text: String) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.push(Task {
            id,
            title: title.clone(),
            status: "starting",
            sid: None,
            log: vec![],
            perm: None,
            started: Instant::now(),
            last_update: Instant::now(),
            noted: false,
        });
        self.emit_tasks();
        match self.worker.clone() {
            None => self.fail_task(id, "tasks are not available".into()),
            Some(worker) => {
                let in_tx = self.in_tx.clone();
                tokio::spawn(async move {
                    let (otx, orx) = oneshot::channel();
                    let sid = if worker.send(WCmd::NewSession(otx)).is_err() {
                        Err("worker gone".to_string())
                    } else {
                        orx.await.unwrap_or_else(|_| Err("worker gone".to_string()))
                    };
                    let _ = in_tx.send(In::Session { id, text, sid });
                });
            }
        }
        id
    }

    fn fail_task(&mut self, id: u32, msg: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.status = "failed";
            t.log.push(format!("error: {msg}"));
        }
        self.say("sys", &format!("That change failed: {msg}"), false);
        self.notice(format!("[task {id} failed] {msg}"));
        self.emit_tasks();
    }

    fn resolve_perm(&mut self, id: u64, allow: bool) -> Result<(), String> {
        let worker = self.worker.clone();
        let task = self.task_mut(id).ok_or("no such task")?;
        let perm = task.perm.take().ok_or("nothing waiting for approval")?;
        let want = if allow { "allow" } else { "reject" };
        let option = perm
            .options
            .iter()
            .find(|(_, k)| k.starts_with(want))
            .map(|(o, _)| o.clone());
        task.status = "running";
        task.log.push(format!(
            "{}: {}",
            if allow { "approved" } else { "denied" },
            perm.title
        ));
        let cmd = match option {
            Some(option_id) => WCmd::Answer {
                rpc_id: perm.rpc_id,
                option_id,
            },
            None => {
                task.status = "running";
                return worker
                    .and_then(|w| w.send(WCmd::Cancel { sid: String::new() }).ok())
                    .map(|_| ())
                    .ok_or_else(|| "no way to answer".to_string());
            }
        };
        if let Some(w) = worker {
            let _ = w.send(cmd);
        }
        self.emit_tasks();
        Ok(())
    }

    /// Tool calls from the voice model.
    fn tool(&mut self, id: String, name: String, args: Value) {
        let result = match name.as_str() {
            "start_task" => match args["task"].as_str() {
                Some(text) if !text.trim().is_empty() => {
                    let title = args["title"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| short(text, 48));
                    let n = self.start_task(title, text.to_string());
                    json!({"task_id": n, "status": "started"})
                }
                _ => json!({"error": "missing task description"}),
            },
            "list_tasks" => json!(
                self.tasks
                    .iter()
                    .map(|t| json!({"task_id": t.id, "title": t.title, "status": t.status}))
                    .collect::<Vec<_>>()
            ),
            "task_status" => match args["task_id"]
                .as_u64()
                .and_then(|n| self.tasks.iter().find(|t| t.id as u64 == n))
            {
                Some(t) => json!({
                    "task_id": t.id,
                    "status": t.status,
                    "recent": t.log.iter().rev().take(3).rev().cloned().collect::<Vec<_>>(),
                    "waiting_for_approval": t.perm.as_ref().map(|p| p.title.clone()),
                }),
                None => json!({"error": "no such task"}),
            },
            "cancel_task" => {
                let sid = args["task_id"]
                    .as_u64()
                    .and_then(|n| self.tasks.iter().find(|t| t.id as u64 == n))
                    .and_then(|t| t.sid.clone());
                match (sid, &self.worker) {
                    (Some(sid), Some(w)) => {
                        let _ = w.send(WCmd::Cancel { sid });
                        json!({"status": "cancel requested"})
                    }
                    _ => json!({"error": "task is not running"}),
                }
            }
            "approve_change" => {
                let allow = args["allow"].as_bool().unwrap_or(false);
                let tid = args["task_id"].as_u64().unwrap_or(0);
                let kind = self
                    .tasks
                    .iter()
                    .find(|t| t.id as u64 == tid)
                    .and_then(|t| t.perm.as_ref())
                    .map(|p| p.kind.clone());
                match kind {
                    None => json!({"error": "nothing waiting for approval on that task"}),
                    Some(k) if allow && k != "edit" => {
                        json!({"error": "this change is waiting for approval on screen. Tell the user to press y in the tasks panel. Do not say there is nothing pending."})
                    }
                    Some(_) => match self.resolve_perm(tid, allow) {
                        Ok(()) => json!({"status": if allow { "approved" } else { "denied" }}),
                        Err(e) => json!({"error": e}),
                    },
                }
            }
            "set_voice" => match args["voice"].as_str() {
                Some(v) => match self.set_voice(v) {
                    Ok(m) => json!({"status": m}),
                    Err(e) => json!({"error": e}),
                },
                None => json!({"error": "missing voice"}),
            },
            "preview_voice" => match args["voice"].as_str() {
                Some(v) => match self.preview(v) {
                    Ok(m) => json!({"status": m}),
                    Err(e) => json!({"error": e}),
                },
                None => json!({"error": "missing voice"}),
            },
            "set_live_model" => match args["model"].as_str() {
                Some(m) => match self.set_live_model(m) {
                    Ok(s) => json!({"status": s}),
                    Err(e) => json!({"error": e}),
                },
                None => json!({"error": "missing model"}),
            },
            "set_mute" => json!({"status": self.set_mute(args["muted"].as_bool().unwrap_or(true))}),
            "set_mute_on_blur" => {
                json!({"status": self.set_mute_on_blur(args["on"].as_bool().unwrap_or(true))})
            }
            "get_settings" => self.settings_summary(),
            "cancel_all_tasks" => {
                json!({"status": format!("cancel requested for {} task(s)", self.cancel_active())})
            }
            "save_transcript" => {
                self.save_transcript();
                json!({"status": "transcript saved"})
            }
            "new_conversation" => {
                self.reconfigure();
                self.say("sys", "new conversation", false);
                json!({"status": "started a new conversation"})
            }
            _ => json!({"error": "unknown function"}),
        };
        let _ = self.live_tx.send(LiveCmd::ToolResult { id, name, result });
    }

    fn on_live(&mut self, e: LiveEvent) {
        match e {
            LiveEvent::Status(s) => {
                self.live_status = s;
                self.emit_status();
            }
            LiveEvent::Audio(s) => {
                self.last_audio = Some(Instant::now());
                let _ = self.audio.send(AudioCmd::Play(s));
            }
            LiveEvent::User(t) => self.say("you", &t, true),
            LiveEvent::Assistant(t) => self.say("asst", &t, true),
            LiveEvent::Interrupted => {
                let _ = self.audio.send(AudioCmd::Flush);
                self.close();
            }
            LiveEvent::TurnComplete => self.close(),
            LiveEvent::Call { id, name, args } => self.tool(id, name, args),
        }
    }

    fn on_worker(&mut self, e: WEvent) {
        match e {
            WEvent::Models(m) => {
                self.worker_models = m;
                self.emit_models();
            }
            WEvent::Text { sid, text } => {
                if let Some(t) = self.task_for(&sid) {
                    match t.log.last_mut() {
                        Some(l) if l.starts_with("· ") => l.push_str(&text),
                        _ => t.log.push(format!("· {text}")),
                    }
                }
                self.emit_tasks();
            }
            WEvent::Tool { sid, line } => {
                if let Some(t) = self.task_for(&sid) {
                    t.log.push(format!("[{line}]"));
                }
                self.emit_tasks();
            }
            WEvent::Permission {
                sid,
                rpc_id,
                title,
                kind,
                options,
            } => {
                if let Some(id) = self.by_sid.get(&sid).copied() {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.status = "needs_approval";
                        t.log.push(format!("? {title} ({kind})"));
                        t.perm = Some(Perm {
                            rpc_id,
                            title: title.clone(),
                            kind: kind.clone(),
                            options,
                        });
                    }
                    self.notice(format!(
                        "[task {id} needs approval: {title} ({kind}). If it is a file edit and the user says yes aloud, call approve_change. Otherwise tell the user to press y on screen.]"
                    ));
                }
                self.emit_tasks();
            }
            WEvent::Done { sid, stop, text } => {
                if let Some(id) = self.by_sid.get(&sid).copied() {
                    let cancelled = stop == "cancelled";
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.status = if cancelled { "cancelled" } else { "done" };
                        t.perm = None;
                        t.log.push(format!("finished ({stop})"));
                    }
                    if cancelled {
                        self.notice(format!("[task {id} cancelled]"));
                    } else {
                        let tail = tail_chars(&text, 1500);
                        let msg = if tail.trim().is_empty() {
                            format!("[task {id} finished] no text output")
                        } else {
                            format!("[task {id} finished] {tail}")
                        };
                        self.notice(msg);
                    }
                }
                self.emit_tasks();
            }
            WEvent::Error { sid, msg } => {
                if sid.is_empty() {
                    self.say("sys", &format!("tasks: {msg}"), false);
                } else if let Some(id) = self.by_sid.get(&sid).copied() {
                    self.fail_task(id, msg);
                }
            }
            WEvent::Info(s) => self.say("sys", &s, false),
        }
    }

    fn task_for(&mut self, sid: &str) -> Option<&mut Task> {
        let id = *self.by_sid.get(sid)?;
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn emit_models(&self) {
        self.emit(json!({
            "t": "models",
            "live": self.live_models,
            "task": self.worker_models.iter().map(|(v, n)| json!([v, n])).collect::<Vec<_>>(),
        }));
    }

    /// Returns true when the core should stop.
    fn on_in(&mut self, ev: In) -> bool {
        match ev {
            In::Live(e) => self.on_live(e),
            In::Worker(e) => self.on_worker(e),
            In::Session { id, text, sid } => match sid {
                Ok(sid) => {
                    self.by_sid.insert(sid.clone(), id);
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                        t.sid = Some(sid.clone());
                        t.status = "running";
                    }
                    if let Some(w) = &self.worker {
                        let _ = w.send(WCmd::Prompt { sid, text });
                    }
                    self.emit_tasks();
                }
                Err(e) => self.fail_task(id, e),
            },
            In::LiveModels(m) => {
                self.live_models = m;
                self.emit_models();
            }
            In::Preview(s) => {
                let _ = self.audio.send(AudioCmd::Play(s));
            }
            In::Ui(v) => return self.on_command(v),
        }
        false
    }

    /// Shared by the UI commands and the voice tools. Each returns a short result for the model.
    fn set_voice(&mut self, name: &str) -> Result<String, String> {
        let name = VOICES
            .iter()
            .find(|v| v.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("unknown voice. Choose from: {}", VOICES.join(", ")))?;
        self.cfg.voice = name.to_string();
        self.cfg.save();
        self.reconfigure();
        self.say(
            "sys",
            &format!("voice: {name}. Live session reconnects; context resets"),
            false,
        );
        self.emit_status();
        Ok(format!("voice set to {name}"))
    }

    fn set_live_model(&mut self, name: &str) -> Result<String, String> {
        if !self.live_models.is_empty() && !self.live_models.iter().any(|m| m == name) {
            return Err(format!(
                "unknown model. Choose from: {}",
                self.live_models.join(", ")
            ));
        }
        self.cfg.live_model = name.to_string();
        self.cfg.save();
        self.reconfigure();
        self.say(
            "sys",
            &format!("live model: {name}. Live session reconnects; context resets"),
            false,
        );
        self.emit_status();
        Ok(format!("live model set to {name}"))
    }

    fn set_mute(&mut self, on: bool) -> String {
        self.manual_mute = on;
        self.apply_mic();
        self.emit_status();
        if on {
            "microphone muted".into()
        } else {
            "microphone on".into()
        }
    }

    fn set_mute_on_blur(&mut self, on: bool) -> String {
        self.cfg.mute_on_blur = on;
        self.cfg.save();
        self.apply_mic();
        self.emit_status();
        format!("mute on blur {}", if on { "on" } else { "off" })
    }

    fn preview(&mut self, voice: &str) -> Result<String, String> {
        let name = VOICES
            .iter()
            .find(|v| v.eq_ignore_ascii_case(voice))
            .ok_or_else(|| format!("unknown voice. Choose from: {}", VOICES.join(", ")))?;
        let _ = self.audio.send(AudioCmd::Flush);
        let s = Settings {
            model: self.cfg.live_model.clone(),
            voice: name.to_string(),
        };
        let text = format!("Hi, I'm {name}. This is how I sound.");
        let key = self.key.clone();
        let tx = self.in_tx.clone();
        tokio::spawn(async move {
            if let Ok(samples) = live::once(&key, &s, &text).await {
                let _ = tx.send(In::Preview(samples));
            }
        });
        Ok(format!("playing a preview of {name}"))
    }

    fn cancel_active(&mut self) -> usize {
        let sids: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, "running" | "needs_approval" | "starting"))
            .filter_map(|t| t.sid.clone())
            .collect();
        let n = sids.len();
        if let Some(w) = &self.worker {
            for sid in sids {
                let _ = w.send(WCmd::Cancel { sid });
            }
        }
        n
    }

    fn settings_summary(&self) -> Value {
        json!({
            "voice": self.cfg.voice,
            "live_model": self.cfg.live_model,
            "muted": self.manual_mute,
            "mute_on_blur": self.cfg.mute_on_blur,
            "voices": VOICES,
        })
    }

    fn on_command(&mut self, v: Value) -> bool {
        let c = v["c"].as_str().unwrap_or_default();
        match c {
            "say" => {
                if let Some(text) = v["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    self.say("you", text, false);
                    let _ = self.live_tx.send(LiveCmd::Text(text.to_string()));
                }
            }
            "mute" => {
                let on = v["on"].as_bool().unwrap_or(false);
                self.set_mute(on);
            }
            "focus" => {
                self.focused = v["on"].as_bool().unwrap_or(true);
                self.apply_mic();
                self.emit_status();
            }
            "mute_on_blur" => {
                let on = v["on"].as_bool().unwrap_or(true);
                self.set_mute_on_blur(on);
            }
            "voice" => {
                if let Some(name) = v["name"].as_str() {
                    let _ = self.set_voice(name);
                }
            }
            "live_model" => {
                if let Some(name) = v["name"].as_str() {
                    let _ = self.set_live_model(name);
                }
            }
            "task_model" => {
                if let (Some(value), Some(w)) = (v["value"].as_str(), &self.worker) {
                    let _ = w.send(WCmd::SetModel(value.to_string()));
                    self.say("sys", &format!("task model: {value}"), false);
                }
            }
            "preview" => {
                if let Some(voice) = v["voice"].as_str() {
                    let _ = self.preview(voice);
                }
            }
            "cancel" => {
                if let Some(n) = v["task"].as_u64() {
                    let sid = self
                        .tasks
                        .iter()
                        .find(|t| t.id as u64 == n)
                        .and_then(|t| t.sid.clone());
                    if let (Some(sid), Some(w)) = (sid, &self.worker) {
                        let _ = w.send(WCmd::Cancel { sid });
                    }
                }
            }
            "clear_finished" => self.clear_finished(),
            "cancel_all" => {
                self.cancel_active();
            }
            "answer" => {
                let id = v["task"].as_u64().unwrap_or(0);
                let allow = v["allow"].as_bool().unwrap_or(false);
                if let Err(e) = self.resolve_perm(id, allow) {
                    self.say("sys", &e, false);
                }
            }
            "save" => self.save_transcript(),
            "_note" => {
                if let Some(t) = v["text"].as_str() {
                    self.say("sys", t, false);
                }
            }
            "quit" => return true,
            _ => {}
        }
        false
    }

    fn save_transcript(&mut self) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join(format!("voxterm-{stamp}.md"));
        let mut out = String::from("# voxterm transcript\n\n## Conversation\n\n");
        for (who, text) in &self.transcript {
            out.push_str(&format!("- **{who}:** {text}\n"));
        }
        out.push_str("\n## Tasks\n\n");
        for t in &self.tasks {
            out.push_str(&format!(
                "### {} (task {}, {})\n\n```\n{}\n```\n\n",
                t.title,
                t.id,
                t.status,
                t.log.join("\n")
            ));
        }
        match std::fs::write(&path, out) {
            Ok(()) => self.say("sys", &format!("saved {}", path.display()), false),
            Err(e) => self.say("sys", &format!("save failed: {e}"), false),
        }
    }
}

/// Used by the self-test. Runs one task through the worker and waits for its result.
pub async fn selftest_task(cwd: &std::path::Path, cmd: &[String], text: &str) -> Result<String> {
    let (tx, mut rx) = acp::start(cmd, cwd).await?;
    let (otx, orx) = oneshot::channel();
    tx.send(WCmd::NewSession(otx))?;
    let sid = orx.await?.map_err(|e| anyhow::anyhow!(e))?;
    tx.send(WCmd::Prompt {
        sid,
        text: text.to_string(),
    })?;
    while let Some(e) = rx.recv().await {
        match e {
            WEvent::Done { .. } => return Ok("done".into()),
            WEvent::Error { msg, .. } => return Err(anyhow::anyhow!(msg)),
            _ => {}
        }
    }
    Err(anyhow::anyhow!("worker closed"))
}
