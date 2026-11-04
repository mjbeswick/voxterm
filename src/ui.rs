//! Terminal UI (ratatui). Draws the core's state and sends commands back as JSON.
//! Speaks only about tasks. Never mentions the worker.

use crate::theme::{self, THEMES, Theme};
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, UnboundedSender};

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const VOICES: [&str; 8] = [
    "Kore", "Puck", "Charon", "Aoede", "Fenrir", "Leda", "Orus", "Zephyr",
];

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Conversation,
    Tasks,
    Input,
}

#[derive(Clone, Copy, PartialEq)]
enum PickKind {
    Voice,
    Live,
    Task,
    Theme,
    Style,
}

#[derive(Clone, Copy)]
enum Action {
    FocusTasks,
    ToggleLog,
    OpenCommands,
    Voice,
    LiveModel,
    TaskModel,
    ToggleMute,
    MuteOnBlur,
    Save,
    CancelSelected,
    CancelAll,
    ClearFinished,
    NextTheme,
    ToggleStyle,
    CopyConversation,
    ShowMemory,
    Quit,
}

struct Say {
    who: String,
    text: String,
    open: bool,
}

#[derive(Deserialize, Clone)]
struct Task {
    id: u64,
    title: String,
    status: String,
    approval: Option<String>,
    log: Vec<String>,
    /// Name of the parent task, for subtasks.
    #[serde(default)]
    parent: Option<String>,
}

#[derive(Clone, Default)]
struct Status {
    state: String,
    mic: bool,
    voice: String,
    live_model: String,
    tasks_ready: bool,
    style: String,
}

struct Picker {
    kind: PickKind,
    title: &'static str,
    items: Vec<(String, String)>,
    sel: usize,
    /// Theme index when the picker opened. Esc restores it.
    origin_theme: usize,
}

struct Palette {
    query: String,
    sel: usize,
    slash: bool,
}

pub struct App {
    says: Vec<Say>,
    tasks: Vec<Task>,
    status: Status,
    live_models: Vec<String>,
    task_models: Vec<(String, String)>,
    input: String,
    focus: Focus,
    sel_task: usize,
    log_open: bool,
    picker: Option<Picker>,
    palette: Option<Palette>,
    note: String,
    quit_armed: bool,
    theme: usize,
    cmd: UnboundedSender<Value>,
    quit: bool,
    mic_hist: Vec<u64>,
    out_hist: Vec<u64>,
    tick: usize,
    /// A voice the picker has moved to, waiting for the selection to settle.
    pending_voice: Option<(String, Instant)>,
    clear_btn: Option<Rect>,
    /// Header state chip. Clicking it toggles the mic.
    mic_btn: Option<Rect>,
    /// Clickable buttons on the bottom row.
    buttons: Vec<(Rect, Action)>,
    /// Whether the tasks column is shown. Ctrl-A and the tasks button toggle it.
    tasks_visible: bool,
}

fn ui_state_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/voxterm/ui.json")
}

fn load_theme() -> usize {
    std::fs::read_to_string(ui_state_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["theme"].as_str().and_then(theme::by_name))
        .unwrap_or(0)
}

impl App {
    pub fn new(cmd: UnboundedSender<Value>) -> Self {
        Self {
            says: vec![],
            tasks: vec![],
            status: Status {
                state: "connecting".into(),
                mic: true,
                ..Default::default()
            },
            live_models: vec![],
            task_models: vec![],
            input: String::new(),
            focus: Focus::Input,
            sel_task: 0,
            log_open: false,
            picker: None,
            palette: None,
            note: String::new(),
            quit_armed: false,
            theme: load_theme(),
            cmd,
            quit: false,
            mic_hist: vec![],
            out_hist: vec![],
            tick: 0,
            pending_voice: None,
            clear_btn: None,
            mic_btn: None,
            buttons: Vec::new(),
            tasks_visible: true,
        }
    }

    fn send(&self, v: Value) {
        let _ = self.cmd.send(v);
    }

    fn t(&self) -> Theme {
        THEMES[self.theme]
    }

    fn set_theme(&mut self, name: &str) {
        match theme::by_name(name) {
            Some(i) => {
                self.theme = i;
                self.note = format!("theme: {name}");
                let p = ui_state_path();
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(p, json!({"theme": name}).to_string());
            }
            None => {
                let names: Vec<&str> = THEMES.iter().map(|t| t.name).collect();
                self.note = format!("unknown theme: {name}. Choose from: {}", names.join(", "));
            }
        }
    }

    /// Applies one event from the core.
    pub fn apply(&mut self, ev: Value) {
        match ev["t"].as_str().unwrap_or_default() {
            "say" => {
                let who = ev["who"].as_str().unwrap_or("sys").to_string();
                let text = ev["text"].as_str().unwrap_or_default();
                let stream = ev["stream"].as_bool().unwrap_or(false);
                let appended = ev["cont"].as_bool().unwrap_or(false);
                if appended {
                    // Same rule as the core: append to the open entry of this speaker.
                    if let Some(entry) = self.says.iter_mut().rev().find(|s| s.open && s.who == who)
                    {
                        entry.text.push_str(text);
                        return;
                    }
                }
                self.says.push(Say {
                    who,
                    text: text.to_string(),
                    open: stream,
                });
            }
            "close" => {
                for s in self.says.iter_mut() {
                    s.open = false;
                }
            }
            "status" => {
                self.status = Status {
                    state: ev["state"].as_str().unwrap_or("connecting").into(),
                    mic: ev["mic"].as_bool().unwrap_or(true),
                    voice: ev["voice"].as_str().unwrap_or_default().into(),
                    live_model: ev["live_model"].as_str().unwrap_or_default().into(),
                    tasks_ready: ev["tasks_ready"].as_bool().unwrap_or(false),
                    style: ev["style"].as_str().unwrap_or("plain").to_string(),
                };
            }
            "tasks" => {
                self.tasks = serde_json::from_value(ev["tasks"].clone()).unwrap_or_default();
                if self.sel_task >= self.tasks.len() {
                    self.sel_task = self.tasks.len().saturating_sub(1);
                }
            }
            "levels" => {
                // RMS of ~0.25 or more fills the meter. Typical speech sits well below that.
                let scale = |v: &Value| (v.as_f64().unwrap_or(0.0) * 400.0).min(100.0) as u64;
                push_hist(&mut self.mic_hist, scale(&ev["mic"]));
                push_hist(&mut self.out_hist, scale(&ev["out"]));
            }
            "bye" => self.quit = true,
            "models" => {
                if let Some(a) = ev["live"].as_array() {
                    self.live_models = a
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                }
                if let Some(a) = ev["task"].as_array() {
                    self.task_models = a
                        .iter()
                        .filter_map(|p| {
                            Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string()))
                        })
                        .collect();
                }
            }
            _ => {}
        }
    }

    fn pending_approval(&self) -> Option<&Task> {
        self.tasks.iter().find(|t| t.approval.is_some())
    }

    fn running(&self) -> bool {
        self.tasks
            .iter()
            .any(|t| matches!(t.status.as_str(), "running" | "needs_approval" | "starting"))
    }

    fn open_picker(&mut self, kind: PickKind) {
        if kind == PickKind::Style {
            let current = self.status.style.clone();
            self.picker = Some(Picker {
                kind,
                title: "voice style",
                items: crate::live::STYLES
                    .iter()
                    .map(|(n, _)| (n.to_string(), n.to_string()))
                    .collect(),
                sel: crate::live::STYLES
                    .iter()
                    .position(|(n, _)| *n == current)
                    .unwrap_or(0),
                origin_theme: self.theme,
            });
            return;
        }
        if kind == PickKind::Theme {
            self.picker = Some(Picker {
                kind,
                title: "theme",
                items: THEMES
                    .iter()
                    .map(|t| (t.name.to_string(), t.name.to_string()))
                    .collect(),
                sel: self.theme,
                origin_theme: self.theme,
            });
            return;
        }
        match kind {
            PickKind::Theme | PickKind::Style => {}
            PickKind::Voice => {
                let sel = VOICES
                    .iter()
                    .position(|v| *v == self.status.voice)
                    .unwrap_or(0);
                self.picker = Some(Picker {
                    kind,
                    title: "voice",
                    items: VOICES
                        .iter()
                        .map(|v| (v.to_string(), v.to_string()))
                        .collect(),
                    sel,
                    origin_theme: self.theme,
                });
            }
            PickKind::Live => {
                if self.live_models.is_empty() {
                    self.note = "models still loading".into();
                    return;
                }
                let sel = self
                    .live_models
                    .iter()
                    .position(|m| *m == self.status.live_model)
                    .unwrap_or(0);
                self.picker = Some(Picker {
                    kind,
                    title: "voice model",
                    items: self
                        .live_models
                        .iter()
                        .map(|m| (m.clone(), m.clone()))
                        .collect(),
                    sel,
                    origin_theme: self.theme,
                });
            }
            PickKind::Task => {
                if self.task_models.is_empty() {
                    self.note = "task models not ready".into();
                    return;
                }
                self.picker = Some(Picker {
                    kind,
                    title: "task model",
                    items: self.task_models.clone(),
                    sel: 0,
                    origin_theme: self.theme,
                });
            }
        }
    }

    fn apply_picker(&mut self, p: Picker) {
        let Some((value, _)) = p.items.get(p.sel).cloned() else {
            return;
        };
        match p.kind {
            PickKind::Voice => self.send(json!({"c": "voice", "name": value})),
            PickKind::Live => self.send(json!({"c": "live_model", "name": value})),
            PickKind::Task => self.send(json!({"c": "task_model", "value": value})),
            PickKind::Theme => self.set_theme(&value),
            PickKind::Style => self.send(json!({"c": "style", "name": value})),
        }
    }

    fn palette_items(&self) -> Vec<(&'static str, &'static str, Action)> {
        vec![
            ("Voice: pick and preview", "/voice", Action::Voice),
            ("Voice model: pick", "/model", Action::LiveModel),
            ("Task model: pick", "/task-model", Action::TaskModel),
            (
                if self.status.mic {
                    "Mute microphone"
                } else {
                    "Unmute microphone"
                },
                "ctrl-t",
                Action::ToggleMute,
            ),
            ("Mute on blur: toggle", "/blur", Action::MuteOnBlur),
            ("Save transcript", "/save", Action::Save),
            ("Copy conversation", "/copy", Action::CopyConversation),
            ("Show memory", "/memory", Action::ShowMemory),
            ("Theme: pick", "/theme", Action::NextTheme),
            ("Voice style: pick", "/style", Action::ToggleStyle),
            ("Cancel selected task", "ctrl-c", Action::CancelSelected),
            ("Cancel all tasks", "/cancel-all", Action::CancelAll),
            ("Clear finished tasks", "/clear", Action::ClearFinished),
            ("Quit", "ctrl-q", Action::Quit),
        ]
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::FocusTasks => self.toggle_tasks(),
            Action::ToggleLog => self.log_open = !self.log_open,
            Action::OpenCommands => {
                self.palette = Some(Palette {
                    query: String::new(),
                    sel: 0,
                    slash: false,
                })
            }
            Action::Voice => self.open_picker(PickKind::Voice),
            Action::LiveModel => self.open_picker(PickKind::Live),
            Action::TaskModel => self.open_picker(PickKind::Task),
            Action::ToggleMute => self.send(json!({"c": "mute", "on": self.status.mic})),
            Action::MuteOnBlur => self.send(json!({"c": "mute_on_blur", "on": false})),
            Action::Save => self.send(json!({"c": "save"})),
            Action::CancelSelected => self.cancel_selected(),
            Action::CancelAll => self.send(json!({"c": "cancel_all"})),
            Action::ClearFinished => self.clear_finished(),
            Action::NextTheme => self.open_picker(PickKind::Theme),
            Action::ToggleStyle => self.open_picker(PickKind::Style),
            Action::CopyConversation => self.copy_conversation(),
            Action::ShowMemory => self.send(json!({"c": "memory"})),
            Action::Quit => self.send(json!({"c": "quit"})),
        }
    }

    fn has_finished(&self) -> bool {
        self.tasks
            .iter()
            .any(|t| matches!(t.status.as_str(), "done" | "failed" | "cancelled"))
    }

    fn clear_finished(&mut self) {
        if self.has_finished() {
            self.send(json!({"c": "clear_finished"}));
            self.note = "cleared finished tasks".into();
        } else {
            self.note = "no finished tasks to clear".into();
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let inside = |r: Rect| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            };
            if self.clear_btn.is_some_and(inside) {
                self.clear_finished();
            } else if self.mic_btn.is_some_and(inside) {
                self.send(json!({"c": "mute", "on": self.status.mic}));
            } else if let Some(action) = self
                .buttons
                .iter()
                .find(|(r, _)| inside(*r))
                .map(|(_, a)| *a)
            {
                self.run_action(action);
            }
        }
    }

    /// Copies the conversation to the clipboard. Mouse capture stops the terminal's own selection,
    /// so this is the way to take text out. On macOS it uses pbcopy.
    fn copy_conversation(&mut self) {
        let text = self
            .says
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.trim().is_empty() {
            self.note = "nothing to copy yet".into();
            return;
        }
        let result = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait().map(|_| ())
            });
        self.note = match result {
            Ok(()) => "conversation copied to the clipboard".into(),
            Err(e) => format!("copy failed: {e}"),
        };
    }

    fn toggle_tasks(&mut self) {
        self.tasks_visible = !self.tasks_visible;
        if self.tasks_visible {
            self.focus = Focus::Tasks;
        } else if self.focus == Focus::Tasks {
            self.focus = Focus::Input;
        }
    }

    fn cancel_selected(&mut self) {
        match self.tasks.get(self.sel_task) {
            Some(t) if matches!(t.status.as_str(), "running" | "needs_approval" | "starting") => {
                let id = t.id;
                self.send(json!({"c": "cancel", "task": id}));
                self.note = format!("cancel requested for task {id}");
            }
            _ => self.note = "no running task selected".into(),
        }
    }

    fn run_slash(&mut self, text: &str) {
        let cmd = text.trim().to_lowercase();
        match cmd.as_str() {
            "/voice" => return self.open_picker(PickKind::Voice),
            "/model" => return self.open_picker(PickKind::Live),
            "/task-model" => return self.open_picker(PickKind::Task),
            "/mute" => return self.send(json!({"c": "mute", "on": true})),
            "/unmute" => return self.send(json!({"c": "mute", "on": false})),
            "/save" => return self.send(json!({"c": "save"})),
            "/copy" => return self.copy_conversation(),
            "/cancel-all" => return self.send(json!({"c": "cancel_all"})),
            "/clear" => return self.clear_finished(),
            "/quit" => return self.send(json!({"c": "quit"})),
            "/theme" => return self.open_picker(PickKind::Theme),
            "/style" => return self.open_picker(PickKind::Style),
            "/memory" => return self.send(json!({"c": "memory"})),
            "/help" => {
                return self.palette = Some(Palette {
                    query: String::new(),
                    sel: 0,
                    slash: false,
                });
            }
            _ => {}
        }
        if let Some(name) = cmd.strip_prefix("/style ") {
            return self.send(json!({"c": "style", "name": name.trim()}));
        }
        if let Some(name) = cmd.strip_prefix("/theme ") {
            return self.set_theme(name.trim());
        }
        self.note = format!("unknown command: {cmd}");
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input).trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            return self.run_slash(&text);
        }
        self.send(json!({"c": "say", "text": text}));
    }

    fn ctrl_c(&mut self) {
        let running = self.running();
        if self.focus == Focus::Tasks && running {
            return self.cancel_selected();
        }
        if running && !self.quit_armed {
            self.quit_armed = true;
            self.note = "tasks are running. Press ctrl-c again to quit.".into();
            return;
        }
        self.send(json!({"c": "quit"}));
    }

    fn on_palette_key(&mut self, key: KeyEvent) {
        let Some(p) = self.palette.as_ref() else {
            return;
        };
        let slash = p.slash;
        let query = if slash {
            self.input.trim_start_matches('/').to_string()
        } else {
            p.query.clone()
        };
        let items: Vec<(&'static str, &'static str, Action)> = self
            .palette_items()
            .into_iter()
            .filter(|(label, _, _)| label.to_lowercase().contains(&query.to_lowercase()))
            .collect();
        let sel = p.sel;
        match key.code {
            KeyCode::Esc => {
                if slash {
                    self.input.clear();
                }
                self.palette = None;
            }
            KeyCode::Up => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = p.sel.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = (p.sel + 1).min(items.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if slash && self.input.contains(' ') {
                    let typed = std::mem::take(&mut self.input);
                    self.palette = None;
                    return self.run_slash(&typed);
                }
                self.input.clear();
                self.palette = None;
                if let Some((_, _, a)) = items.get(sel) {
                    self.run_action(*a);
                }
            }
            KeyCode::Backspace => {
                if slash {
                    self.input.pop();
                    if self.input.is_empty() {
                        self.palette = None;
                    }
                } else if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                    p.sel = 0;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if slash {
                    self.input.push(c);
                } else if let Some(p) = self.palette.as_mut() {
                    p.query.push(c);
                    p.sel = 0;
                }
            }
            _ => {}
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                if let Some(p) = self.picker.take() {
                    // Theme shows what you hover, so Esc puts back the one you started with.
                    if p.kind == PickKind::Theme {
                        self.theme = p.origin_theme;
                    }
                }
                self.pending_voice = None;
            }
            KeyCode::Up | KeyCode::Down => {
                let last = p.items.len().saturating_sub(1);
                p.sel = if key.code == KeyCode::Up {
                    p.sel.saturating_sub(1)
                } else {
                    (p.sel + 1).min(last)
                };
                let kind = p.kind;
                let name = p.items.get(p.sel).map(|(v, _)| v.clone());
                if let Some(name) = name {
                    match kind {
                        // The theme applies as you move.
                        PickKind::Theme => {
                            if let Some(i) = theme::by_name(&name) {
                                self.theme = i;
                            }
                        }
                        // The voice is previewed once the selection has been still for a moment.
                        PickKind::Voice => self.pending_voice = Some((name, Instant::now())),
                        _ => {}
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(p) = self.picker.take() {
                    self.pending_voice = None;
                    self.apply_picker(p);
                }
            }
            _ => {}
        }
    }

    /// Previews a voice once the picker selection has been still for 700 ms.
    /// Previewing does not reconnect the session, so the conversation is kept.
    pub fn on_tick(&mut self) {
        if let Some((name, at)) = self.pending_voice.clone() {
            if at.elapsed() >= Duration::from_millis(700) {
                self.pending_voice = None;
                self.send(json!({"c": "preview", "voice": name}));
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.palette.is_some() {
            return self.on_palette_key(key);
        }
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Approval: y and n work only while the message line is empty.
        if let Some(t) = self.pending_approval() {
            let id = t.id;
            if self.input.is_empty() {
                match key.code {
                    KeyCode::Char('y') => {
                        return self.send(json!({"c": "answer", "task": id, "allow": true}));
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        return self.send(json!({"c": "answer", "task": id, "allow": false}));
                    }
                    _ => {}
                }
            }
        }

        if ctrl {
            match key.code {
                KeyCode::Char('q') => return self.send(json!({"c": "quit"})),
                KeyCode::Char('c') => return self.ctrl_c(),
                KeyCode::Char('o') => return self.focus = Focus::Conversation,
                KeyCode::Char('a') => return self.toggle_tasks(),
                KeyCode::Char('l') => {
                    self.log_open = !self.log_open;
                    return;
                }
                KeyCode::Char('p') | KeyCode::Char('k') => {
                    return self.palette = Some(Palette {
                        query: String::new(),
                        sel: 0,
                        slash: false,
                    });
                }
                KeyCode::Char('t') => {
                    return self.send(json!({"c": "mute", "on": self.status.mic}));
                }
                KeyCode::Char('u') => return self.input.clear(),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Conversation => Focus::Tasks,
                    Focus::Tasks => Focus::Input,
                    Focus::Input => Focus::Conversation,
                };
                return;
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Conversation => Focus::Input,
                    Focus::Tasks => Focus::Conversation,
                    Focus::Input => Focus::Tasks,
                };
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Tasks => match key.code {
                KeyCode::Up => self.sel_task = self.sel_task.saturating_sub(1),
                KeyCode::Down => {
                    self.sel_task = (self.sel_task + 1).min(self.tasks.len().saturating_sub(1))
                }
                KeyCode::Enter => self.log_open = !self.log_open,
                KeyCode::Char('c') => self.clear_finished(),
                KeyCode::Esc => self.focus = Focus::Input,
                _ => {}
            },
            Focus::Conversation => {
                if key.code == KeyCode::Esc {
                    self.focus = Focus::Input;
                }
            }
            Focus::Input => match key.code {
                KeyCode::Enter => self.submit(),
                KeyCode::Esc => self.input.clear(),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char('/') if self.input.is_empty() => {
                    self.input.push('/');
                    self.palette = Some(Palette {
                        query: String::new(),
                        sel: 0,
                        slash: true,
                    });
                }
                KeyCode::Char(c) if !ctrl => self.input.push(c),
                _ => {}
            },
        }
    }

    pub fn on_focus(&self, gained: bool) {
        self.send(json!({"c": "focus", "on": gained}));
    }
}

/// Eight-step block bar for a 0..=100 level.
/// Braille meter: the newest `width` readings, one braille cell each, rising from the bottom.
fn braille_meter(hist: &[u64], width: usize) -> String {
    // Eight fill levels per cell, built from the bottom row of dots of a braille cell.
    const FILL: [char; 9] = ['⠀', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];
    let start = hist.len().saturating_sub(width);
    let mut out: String = std::iter::repeat('⠀')
        .take(width.saturating_sub(hist.len() - start))
        .collect();
    for &v in &hist[start..] {
        let level = (v.min(100) as usize * 8).div_ceil(100);
        out.push(FILL[level.min(8)]);
    }
    out
}

fn push_hist(h: &mut Vec<u64>, v: u64) {
    h.push(v);
    if h.len() > 240 {
        h.remove(0);
    }
}

// ---- drawing ----

fn rows_of(width: usize, cols: u16) -> u16 {
    let w = (cols as usize).max(1);
    width.max(1).div_ceil(w) as u16
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Panel title with an optional hotkey letter, highlighted in red as btop does.
fn title_line(name: &str, hot: Option<usize>, t: &Theme) -> Line<'static> {
    title_with_note(name, hot, None, t)
}

/// A key label with its letter highlighted, like btop's hotkey marks. "^T" shows the T in red.
fn key_spans(key: &str, bg: Option<Color>, t: &Theme) -> Vec<Span<'static>> {
    let mut base = Style::new().fg(t.dim);
    let mut hot = Style::new().fg(t.bad).add_modifier(Modifier::BOLD);
    if let Some(c) = bg {
        base = base.bg(c);
        hot = hot.bg(c);
    }
    if let Some(rest) = key.strip_prefix('^') {
        return vec![
            Span::styled("^".to_string(), base),
            Span::styled(rest.to_string(), hot.add_modifier(Modifier::UNDERLINED)),
        ];
    }
    vec![Span::styled(key.to_string(), base)]
}

/// Markdown for the conversation: headings, bullets, fenced code, **bold**, *italic*, and `code`.
fn markdown_lines(text: &str, t: &Theme) -> Vec<Line<'static>> {
    let base = Style::new().fg(t.good);
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(Line::styled(
                format!("  {raw}"),
                Style::new().fg(t.special).bg(t.header_bg),
            ));
            continue;
        }
        if let Some(h) = trimmed.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            out.push(Line::styled(
                h.to_string(),
                Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
            ));
            continue;
        }
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled("  • ".to_string(), Style::new().fg(t.accent))];
            spans.extend(inline_spans(item, base, t));
            out.push(Line::from(spans));
            continue;
        }
        out.push(Line::from(inline_spans(raw, base, t)));
    }
    out
}

/// Inline markdown: **bold**, *italic*, and `code`, with markers removed.
fn inline_spans(text: &str, base: Style, t: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut code = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };
    let style_now = |bold: bool, italic: bool, code: bool| {
        let mut st = base;
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if italic {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if code {
            st = st.fg(t.special).bg(t.header_bg);
        }
        st
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            flush(&mut buf, &mut spans, style_now(bold, italic, code));
            code = !code;
        } else if !code && c == '*' && chars.get(i + 1) == Some(&'*') {
            flush(&mut buf, &mut spans, style_now(bold, italic, code));
            bold = !bold;
            i += 1;
        } else if !code && c == '*' {
            flush(&mut buf, &mut spans, style_now(bold, italic, code));
            italic = !italic;
        } else {
            buf.push(c);
        }
        i += 1;
    }
    flush(&mut buf, &mut spans, style_now(bold, italic, code));
    spans
}

/// A word with its shortcut letter highlighted in place, like "mu**t**e".
fn word_with_letter(word: &str, letter: Option<char>, bg: Color, t: &Theme) -> Vec<Span<'static>> {
    let base = Style::new().fg(t.fg).bg(bg).add_modifier(Modifier::BOLD);
    let hot = Style::new()
        .fg(t.bad)
        .bg(bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let at = letter.and_then(|c| word.to_lowercase().find(c.to_ascii_lowercase()));
    match at {
        Some(i) => vec![
            Span::styled(word[..i].to_string(), base),
            Span::styled(word[i..i + 1].to_string(), hot),
            Span::styled(word[i + 1..].to_string(), base),
        ],
        None => vec![Span::styled(word.to_string(), base)],
    }
}

/// The shared panel title format: name in the accent colour, hotkey letter in red, optional note in grey.
fn title_with_note(name: &str, hot: Option<usize>, note: Option<&str>, t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, ch) in name.chars().enumerate() {
        let style = if Some(i) == hot {
            Style::new().fg(t.bad).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::raw(" "));
    if let Some(n) = note {
        spans.push(Span::styled(format!("· {n} "), Style::new().fg(t.muted)));
    }
    Line::from(spans)
}

fn panel_line(title: Line<'static>, color: Color, t: &Theme) -> Block<'static> {
    let _ = t;
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(title)
}

fn pane(
    f: &mut Frame,
    area: Rect,
    title: Line<'static>,
    lines: Vec<Line<'static>>,
    border: Color,
    t: &Theme,
) {
    let block = panel_line(title, border, t);
    let inner = block.inner(area);
    let rows: u16 = lines.iter().map(|l| rows_of(l.width(), inner.width)).sum();
    let scroll = rows.saturating_sub(inner.height);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(block),
        area,
    );
}

fn state_style(state: &str, t: &Theme) -> (&'static str, Color) {
    match state {
        "listening" => ("LISTENING", t.good),
        "speaking" => ("SPEAKING", t.special),
        "muted" => ("MUTED", t.warn),
        "retrying" => ("RETRYING", t.bad),
        _ => ("CONNECTING", t.accent),
    }
}

fn status_word(s: &str) -> (&str, Color) {
    let t = THEMES[0];
    match s {
        "running" | "starting" => ("running", t.accent),
        "needs_approval" => ("approval", t.warn),
        "done" => ("done", t.good),
        "failed" => ("failed", t.bad),
        "cancelled" => ("cancelled", t.muted),
        other => (other, t.fg),
    }
}

impl App {
    pub fn draw(&mut self, f: &mut Frame) {
        let t = self.t();
        let [top, main, approval, input, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(if self.pending_approval().is_some() {
                1
            } else {
                0
            }),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        // Header
        let (label, color) = state_style(&self.status.state, &t);
        let chip_width = label.chars().count() as u16 + 2;
        self.mic_btn = Some(Rect::new(top.x + 9, top.y, chip_width, 1));
        let header = Line::from(vec![
            Span::styled(
                " voxterm ",
                Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(t.on_accent)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {} · {}  {}",
                    self.status.voice,
                    self.status.live_model,
                    if self.status.mic { "" } else { "· muted" }
                ),
                Style::new().fg(t.muted),
            ),
            Span::styled(
                format!(
                    "  {}",
                    if self.status.tasks_ready {
                        ""
                    } else {
                        "tasks off"
                    }
                ),
                Style::new().fg(t.muted),
            ),
        ]);
        // Level meter at the right of the header: braille columns, speaker while talking, mic otherwise.
        let (hist, meter_color) = if self.status.state == "speaking" {
            (&self.out_hist, t.special)
        } else {
            (&self.mic_hist, t.good)
        };
        let meter_width = top.width.saturating_sub(40).clamp(0, 30);
        let [title_area, meter_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(meter_width)]).areas(top);
        f.render_widget(
            Paragraph::new(header).style(Style::new().bg(t.header_bg)),
            title_area,
        );
        if meter_width > 0 {
            f.render_widget(
                Paragraph::new(braille_meter(hist, meter_width as usize))
                    .style(Style::new().fg(meter_color).bg(t.header_bg)),
                meter_area,
            );
        }

        // Panes
        let (left, right) = if self.tasks_visible {
            let [l, r] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(main);
            (l, r)
        } else {
            (main, Rect::default())
        };
        let conv_border = if self.focus == Focus::Conversation {
            t.accent
        } else {
            t.border
        };
        // No speaker labels. Your text and the assistant's text differ by colour.
        // Core notices (approvals, failures) show inline in a dim italic style.
        // The assistant's text renders as markdown. Your text and notices stay plain.
        let says: Vec<Line> = self
            .says
            .iter()
            .flat_map(|s| match s.who.as_str() {
                "asst" => markdown_lines(&s.text, &t),
                "you" => s
                    .text
                    .split('\n')
                    .map(|l| Line::styled(l.to_string(), Style::new().fg(t.accent)))
                    .collect(),
                _ => s
                    .text
                    .split('\n')
                    .map(|l| {
                        Line::styled(
                            l.to_string(),
                            Style::new().fg(t.warn).add_modifier(Modifier::ITALIC),
                        )
                    })
                    .collect(),
            })
            .collect();
        pane(
            f,
            left,
            title_line("conversation", Some(1), &t),
            says,
            conv_border,
            &t,
        );

        if !self.tasks_visible {
            self.clear_btn = None;
        }
        if self.tasks_visible {
            let (task_list, log) = if self.log_open {
                let [a, b] =
                    Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .areas(right);
                (a, b)
            } else {
                (right, Rect::default())
            };
            let task_border = if self.focus == Focus::Tasks {
                t.warn
            } else {
                t.border
            };
            let mut task_lines: Vec<Line> = vec![];
            if self.tasks.is_empty() {
                task_lines.push(Line::styled(
                    " No tasks yet. Ask for a change.",
                    Style::new().fg(t.muted),
                ));
            }
            for (i, task) in self.tasks.iter().enumerate() {
                let (word, color) = status_word(&task.status);
                let running = matches!(task.status.as_str(), "running" | "starting");
                let mark = if running {
                    SPINNER[self.tick % SPINNER.len()]
                } else if i == self.sel_task && self.focus == Focus::Tasks {
                    "▸"
                } else {
                    " "
                };
                let indent = if task.parent.is_some() { "  └ " } else { "" };
                task_lines.push(Line::from(vec![
                    Span::styled(
                        format!("{mark} {indent}{} ", task.id),
                        Style::new().fg(t.fg),
                    ),
                    Span::styled(
                        format!("{word:<10}"),
                        Style::new().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(task.title.clone(), Style::new().fg(t.fg)),
                ]));
            }
            {
                let label = " [ clear finished ] ";
                let finished = self.has_finished();
                let btn_style = if finished {
                    Style::new()
                        .fg(t.on_accent)
                        .bg(t.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(t.muted)
                };
                let block = panel_line(title_line("tasks", Some(1), &t), task_border, &t)
                    .title_bottom(Line::styled(label, btn_style));
                let inner = block.inner(task_list);
                let rows: u16 = task_lines
                    .iter()
                    .map(|l| rows_of(l.width(), inner.width))
                    .sum();
                let scroll = rows.saturating_sub(inner.height);
                f.render_widget(
                    Paragraph::new(task_lines)
                        .wrap(Wrap { trim: false })
                        .scroll((scroll, 0))
                        .block(block),
                    task_list,
                );
                self.clear_btn = if finished {
                    Some(Rect::new(
                        task_list.x + 2,
                        task_list.y + task_list.height.saturating_sub(1),
                        label.chars().count() as u16,
                        1,
                    ))
                } else {
                    None
                };
            }
            if self.log_open {
                if let Some(task) = self.tasks.get(self.sel_task) {
                    let lines: Vec<Line> = task
                        .log
                        .iter()
                        .rev()
                        .take(200)
                        .rev()
                        .map(|l| Line::styled(l.clone(), Style::new().fg(t.dim)))
                        .collect();
                    pane(
                        f,
                        log,
                        title_with_note("log", Some(0), Some(&task.title), &t),
                        lines,
                        t.border,
                        &t,
                    );
                }
            }
        }

        // Approval banner
        if let Some(task) = self.pending_approval() {
            let text = format!(
                " task {} needs approval: {} · y allow · n deny · esc deny ",
                task.id,
                task.approval.clone().unwrap_or_default()
            );
            f.render_widget(
                Paragraph::new(text).style(Style::new().fg(t.on_accent).bg(t.warn)),
                approval,
            );
        }

        // Message line
        let prompt = if self.input.is_empty() {
            Line::from(vec![
                Span::styled("› ", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled("type to talk", Style::new().fg(t.muted)),
            ])
        } else {
            Line::from(vec![
                Span::styled("› ", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled(self.input.clone(), Style::new().fg(t.fg)),
            ])
        };
        let input_border = if self.focus == Focus::Input {
            t.accent
        } else {
            t.border
        };
        let input_block = panel_line(
            title_with_note("message", None, Some("enter sends"), &t),
            input_border,
            &t,
        );
        let inner = input_block.inner(input);
        f.render_widget(input_block, input);
        f.render_widget(Paragraph::new(prompt), inner);
        if self.focus == Focus::Input {
            f.set_cursor_position(Position::new(
                input.x + 3 + self.input.chars().count() as u16,
                input.y + 1,
            ));
        }

        // Bottom row: clickable buttons. Each shows the key that does the same thing.
        // Each word carries its own shortcut letter, which is highlighted inside the word.
        let buttons_def: Vec<(&str, &str, Action)> = vec![
            ("mute", "^T", Action::ToggleMute),
            ("tasks", "^A", Action::FocusTasks),
            ("log", "^L", Action::ToggleLog),
            ("palette", "^P", Action::OpenCommands),
            ("copy", "/copy", Action::CopyConversation),
            ("save", "/save", Action::Save),
            ("quit", "^Q", Action::Quit),
        ];
        self.buttons.clear();
        let mut x = hint.x;
        let mut spans: Vec<Span> = Vec::new();
        for (label, key, action) in buttons_def {
            let w = (label.chars().count() + 2) as u16;
            if x + w > hint.x + hint.width {
                break;
            }
            self.buttons.push((Rect::new(x, hint.y, w, 1), action));
            let letter = key.strip_prefix('^').and_then(|k| k.chars().next());
            spans.push(Span::styled(" ", Style::new().bg(t.header_bg)));
            spans.extend(word_with_letter(label, letter, t.header_bg, &t));
            spans.push(Span::styled(" ", Style::new().bg(t.header_bg)));
            spans.push(Span::raw(" "));
            x += w + 1;
        }
        let note = if self.note.is_empty() {
            match self.focus {
                Focus::Tasks => "↑↓ select · enter log · ctrl-c cancel task".to_string(),
                Focus::Conversation => "ctrl-o chat · tab moves focus".to_string(),
                Focus::Input => "enter send · tab moves focus".to_string(),
            }
        } else {
            self.note.clone()
        };
        spans.push(Span::styled(note, Style::new().fg(t.muted)));
        f.render_widget(Paragraph::new(Line::from(spans)), hint);

        // Overlays
        if let Some(p) = &self.palette {
            let q = if p.slash {
                self.input.trim_start_matches('/').to_string()
            } else {
                p.query.clone()
            };
            let items: Vec<(&str, &str, Action)> = self
                .palette_items()
                .into_iter()
                .filter(|(label, _, _)| label.to_lowercase().contains(&q.to_lowercase()))
                .collect();
            let h = (items.len() as u16 + 6).min(f.area().height);
            let area = centered(f.area(), (f.area().width * 3 / 5).max(48), h);
            f.render_widget(Clear, area);
            let inner_w = area.width.saturating_sub(2) as usize;

            // Filter line: a search prompt, then what is typed.
            let mut lines: Vec<Line> = vec![Line::from(vec![
                Span::styled(
                    if p.slash { "/" } else { "› " },
                    Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if p.slash { q.clone() } else { p.query.clone() },
                    Style::new().fg(t.fg),
                ),
                Span::styled("▏", Style::new().fg(t.accent)),
            ])];
            lines.push(Line::styled("─".repeat(inner_w), Style::new().fg(t.border)));

            // One row per command: label on the left, its key right-aligned.
            for (i, (label, keys, _)) in items.iter().enumerate() {
                let selected = i == p.sel;
                let bg = if selected { t.accent } else { t.header_bg };
                let fg = if selected { t.on_accent } else { t.fg };
                let key_len = keys.chars().count();
                let pad = inner_w.saturating_sub(label.chars().count() + key_len + 3);
                let mut row = vec![
                    Span::styled(format!("  {label}"), Style::new().fg(fg).bg(bg)),
                    Span::styled(" ".repeat(pad), Style::new().bg(bg)),
                ];
                row.extend(key_spans(keys, Some(bg), &t));
                row.push(Span::styled(" ", Style::new().bg(bg)));
                lines.push(Line::from(row));
            }

            // Footer: the controls, as key chips.
            lines.push(Line::from(vec![
                Span::styled(
                    " ↑↓ ",
                    Style::new()
                        .fg(t.fg)
                        .bg(t.header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" move   ", Style::new().fg(t.muted)),
                Span::styled(
                    " enter ",
                    Style::new()
                        .fg(t.fg)
                        .bg(t.header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" run   ", Style::new().fg(t.muted)),
                Span::styled(
                    " esc ",
                    Style::new()
                        .fg(t.fg)
                        .bg(t.header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" close", Style::new().fg(t.muted)),
            ]));
            f.render_widget(
                Paragraph::new(lines).block(panel_line(
                    title_with_note("commands", None, Some("type to filter"), &t),
                    t.accent,
                    &t,
                )),
                area,
            );
        }
        if let Some(p) = &self.picker {
            let h = (p.items.len() as u16 + 2).min(f.area().height);
            let area = centered(f.area(), 48, h);
            f.render_widget(Clear, area);
            let items: Vec<ListItem> = p
                .items
                .iter()
                .map(|(_, label)| ListItem::new(label.clone()))
                .collect();
            let mut state = ListState::default().with_selected(Some(p.sel));
            let list = List::new(items)
                .block(
                    panel_line(title_line(p.title, None, &t), t.accent, &t).title_bottom(
                        Line::styled(" enter apply · esc cancel ", Style::new().fg(t.muted)),
                    ),
                )
                .highlight_style(Style::new().fg(t.on_accent).bg(t.accent))
                .highlight_symbol("▸ ");
            f.render_stateful_widget(list, area, &mut state);
        }
    }
}

/// Runs the terminal UI with the core in the same process.
pub async fn run(cfg: crate::core::Config, key: String, cwd: PathBuf) -> anyhow::Result<()> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Value>();
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<Event>();

    // Terminal input runs on its own thread, since crossterm reads block.
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if term_tx.send(ev).is_err() {
                break;
            }
        }
    });

    tokio::spawn(crate::core::run(cfg, key, cwd, out_tx, cmd_rx));

    let mut app = App::new(cmd_tx);
    let mut term = ratatui::init();
    let _ = execute!(io::stdout(), EnableFocusChange, EnableMouseCapture);
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let result: anyhow::Result<()> = loop {
        if let Err(e) = term.draw(|f| app.draw(f)) {
            break Err(anyhow::anyhow!(e));
        }
        tokio::select! {
            ev = out_rx.recv() => match ev {
                Some(ev) => app.apply(ev),
                None => app.quit = true,
            },
            Some(ev) = term_rx.recv() => match ev {
                Event::Key(k) => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                Event::FocusGained => app.on_focus(true),
                Event::FocusLost => app.on_focus(false),
                _ => {}
            },
            _ = tick.tick() => {
                app.tick = app.tick.wrapping_add(1);
                app.on_tick();
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(io::stdout(), DisableFocusChange, DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod markdown_tests {
    use super::*;

    #[test]
    fn markers_are_removed_and_text_kept() {
        let t = THEMES[0];
        let spans = inline_spans("a **b** *c* `d`", Style::new(), &t);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "a b c d");
    }

    #[test]
    fn fenced_code_and_bullets_render() {
        let t = THEMES[0];
        let lines = markdown_lines("# Title\n- item\n```\nlet x = 1;\n```", &t);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text[0], "Title");
        assert!(text[1].contains("• item"));
        assert!(text[2].contains("let x = 1;"));
        assert_eq!(text.len(), 3);
    }
}
