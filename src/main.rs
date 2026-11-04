//! voxterm: full-screen voice assistant. Gemini Live talks. A coding agent (ACP) changes things.

mod acp;
mod aec;
mod audio;
mod live;

use acp::{AgentCmd, AgentEvent};
use anyhow::{Result, anyhow};
use audio::AudioCmd;
use crossterm::{
    event::{
        DisableFocusChange, EnableFocusChange, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use futures_util::StreamExt;
use live::{LiveCmd, LiveEvent, Settings};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, UnboundedSender};

const VOICES: [&str; 8] = [
    "Kore", "Puck", "Charon", "Aoede", "Fenrir", "Leda", "Orus", "Zephyr",
];

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    voice: String,
    live_model: String,
    agent_cmd: String,
    mute_on_blur: bool,
    echo_gate: Option<bool>,
    echo_tail_ms: u64,
    mic_min_rms: f32,
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
    fn path() -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/voxterm/config.json")
    }

    fn load() -> Self {
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

fn settings_of(c: &Config) -> Settings {
    Settings {
        model: c.live_model.clone(),
        voice: c.voice.clone(),
    }
}

/// Opt-in event log: set VOXTERM_LOG to a file path.
pub fn dbg(msg: impl AsRef<str>) {
    if let Ok(path) = std::env::var("VOXTERM_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "{} {}", ms % 100_000_000, msg.as_ref());
        }
    }
}

/// Everything the UI loop reacts to, funnelled into one channel.
enum Ui {
    Live(LiveEvent),
    Agent(AgentEvent),
    Status(String),
    Models(Vec<String>),
    Preview(Vec<i16>),
}

fn forward<T: Send + 'static>(
    mut rx: mpsc::UnboundedReceiver<T>,
    ui: UnboundedSender<Ui>,
    wrap: fn(T) -> Ui,
) {
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            if ui.send(wrap(e)).is_err() {
                break;
            }
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum PickKind {
    Voice,
    LiveModel,
    AgentModel,
}

struct Picker {
    kind: PickKind,
    title: &'static str,
    items: Vec<(String, String)>,
    sel: usize,
}

struct Perm {
    rpc_id: Value,
    title: String,
    kind: String,
    options: Vec<(String, String)>,
}

struct Entry {
    who: &'static str,
    text: String,
    open: bool,
}

struct App {
    cfg: Config,
    key: String,
    live_tx: UnboundedSender<LiveCmd>,
    agent_tx: Option<UnboundedSender<AgentCmd>>,
    audio_tx: std::sync::mpsc::Sender<AudioCmd>,
    ui_tx: UnboundedSender<Ui>,
    transcript: Vec<Entry>,
    agent_log: Vec<String>,
    agent_last: String,
    agent_busy: bool,
    agent_models: Vec<(String, String)>,
    live_models: Vec<String>,
    live_status: String,
    input: String,
    manual_mute: bool,
    focused: bool,
    perm: Option<Perm>,
    picker: Option<Picker>,
    quit: bool,
    last_audio: Option<Instant>,
}

impl App {
    fn mic_wanted(&self) -> bool {
        !self.manual_mute && (self.focused || !self.cfg.mute_on_blur)
    }

    fn apply_mic(&self) {
        dbg(format!(
            "mic wanted={} focused={} manual_mute={}",
            self.mic_wanted(),
            self.focused,
            self.manual_mute
        ));
        let _ = self.audio_tx.send(AudioCmd::Mic(self.mic_wanted()));
    }

    fn sys(&mut self, text: impl Into<String>) {
        self.transcript.push(Entry {
            who: "sys",
            text: text.into(),
            open: false,
        });
    }

    /// Streams text into the open entry of the same speaker. Closed on turn end.
    fn push_text(&mut self, who: &'static str, text: &str) {
        match self.transcript.last_mut() {
            Some(e) if e.who == who && e.open => e.text.push_str(text),
            _ => self.transcript.push(Entry {
                who,
                text: text.to_string(),
                open: true,
            }),
        }
    }

    fn close_open(&mut self) {
        for e in self.transcript.iter_mut() {
            e.open = false;
        }
    }

    fn reconfigure(&self) {
        let _ = self
            .live_tx
            .send(LiveCmd::Reconfigure(settings_of(&self.cfg)));
    }

    fn on_ui(&mut self, ev: Ui) {
        dbg(format!(
            "ui {}",
            match &ev {
                Ui::Live(e) => format!("{e:?}").chars().take(60).collect::<String>(),
                Ui::Agent(e) => format!("{e:?}").chars().take(60).collect::<String>(),
                Ui::Status(s) => format!("status {s}"),
                Ui::Models(m) => format!("models {}", m.len()),
                Ui::Preview(_) => "preview".into(),
            }
        ));
        match ev {
            Ui::Live(e) => self.on_live(e),
            Ui::Agent(e) => self.on_agent(e),
            Ui::Status(s) => self.sys(s),
            Ui::Models(m) => self.live_models = m,
            Ui::Preview(s) => {
                let _ = self.audio_tx.send(AudioCmd::Play(s));
            }
        }
    }

    fn on_live(&mut self, e: LiveEvent) {
        match e {
            LiveEvent::Status(s) => self.live_status = s,
            LiveEvent::Audio(s) => {
                self.last_audio = Some(Instant::now());
                let _ = self.audio_tx.send(AudioCmd::Play(s));
            }
            LiveEvent::User(t) => self.push_text("you", &t),
            LiveEvent::Assistant(t) => self.push_text("asst", &t),
            LiveEvent::Interrupted => {
                dbg("interrupted: flush playback");
                let _ = self.audio_tx.send(AudioCmd::Flush);
                self.close_open();
            }
            LiveEvent::TurnComplete => self.close_open(),
            LiveEvent::Call { id, name, args } => self.on_call(id, name, args),
        }
    }

    fn on_call(&mut self, id: String, name: String, args: Value) {
        let result = match name.as_str() {
            "ask_coding_agent" => match (&self.agent_tx, self.agent_busy, args["task"].as_str()) {
                (None, _, _) => json!({"error": "coding agent is not running"}),
                (_, true, _) => json!({"error": "agent is still busy; cancel_agent first"}),
                (Some(tx), false, Some(task)) => {
                    let _ = tx.send(AgentCmd::Prompt(task.to_string()));
                    self.agent_busy = true;
                    self.agent_log.push(format!("> {task}"));
                    json!({"status": "started; the result will follow"})
                }
                _ => json!({"error": "missing task"}),
            },
            "agent_status" => json!({"busy": self.agent_busy, "last": self.agent_last}),
            "cancel_agent" => {
                if let Some(tx) = &self.agent_tx {
                    let _ = tx.send(AgentCmd::Cancel);
                }
                json!({"status": "cancel sent"})
            }
            "answer_permission" => {
                let allow = args["allow"].as_bool().unwrap_or(false);
                let kind = self.perm.as_ref().map(|p| p.kind.clone());
                match kind {
                    None => json!({"error": "no pending permission"}),
                    Some(k) if allow && k != "edit" => {
                        json!({"error": "voice may only approve file edits; ask the user to confirm on screen"})
                    }
                    Some(_) => {
                        self.resolve_perm(allow);
                        json!({"status": if allow { "approved" } else { "denied" }})
                    }
                }
            }
            _ => json!({"error": "unknown function"}),
        };
        let _ = self.live_tx.send(LiveCmd::ToolResult { id, name, result });
    }

    fn resolve_perm(&mut self, allow: bool) {
        let Some(p) = self.perm.take() else { return };
        let want = if allow { "allow" } else { "reject" };
        let option = p
            .options
            .iter()
            .find(|(_, k)| k.starts_with(want))
            .map(|(id, _)| id.clone());
        if let Some(tx) = &self.agent_tx {
            let _ = match option {
                Some(option_id) => tx.send(AgentCmd::Answer {
                    rpc_id: p.rpc_id,
                    option_id,
                }),
                None => tx.send(AgentCmd::Cancel),
            };
        }
        self.agent_log.push(format!(
            "permission {}: {}",
            if allow { "allowed" } else { "denied" },
            p.title
        ));
    }

    fn on_agent(&mut self, e: AgentEvent) {
        match e {
            AgentEvent::Ready { models } => {
                self.agent_models = models;
                self.agent_log.push("agent ready".into());
            }
            AgentEvent::Text(t) => match self.agent_log.last_mut() {
                Some(l) if l.starts_with("· ") => l.push_str(&t),
                _ => self.agent_log.push(format!("· {t}")),
            },
            AgentEvent::Tool(s) => self.agent_log.push(format!("[{s}]")),
            AgentEvent::Info(s) => self.agent_log.push(s),
            AgentEvent::Permission {
                rpc_id,
                title,
                kind,
                options,
            } => {
                self.agent_log
                    .push(format!("? permission: {title} ({kind})"));
                self.perm = Some(Perm {
                    rpc_id,
                    title,
                    kind,
                    options,
                });
            }
            AgentEvent::Done { stop, text } => {
                self.agent_busy = false;
                self.agent_log.push(format!("done ({stop})"));
                let tail = tail_chars(&text, 1500);
                let msg = if tail.trim().is_empty() {
                    "[coding agent finished, no text output]".to_string()
                } else {
                    format!("[coding agent finished] {tail}")
                };
                let _ = self.live_tx.send(LiveCmd::Text(msg));
            }
            AgentEvent::Error(e) => {
                self.agent_busy = false;
                self.agent_log.push(format!("error: {e}"));
                let _ = self
                    .live_tx
                    .send(LiveCmd::Text(format!("[coding agent error] {e}")));
            }
        }
        self.agent_last = self.agent_log.last().cloned().unwrap_or_default();
    }

    fn on_term(&mut self, ev: Event) {
        match ev {
            Event::FocusLost => {
                self.focused = false;
                self.apply_mic();
            }
            Event::FocusGained => {
                self.focused = true;
                self.apply_mic();
            }
            Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.picker.is_some() {
            return self.on_picker_key(k.code);
        }
        if self.perm.is_some() {
            match k.code {
                KeyCode::Char('y') => self.resolve_perm(true),
                KeyCode::Char('n') | KeyCode::Esc => self.resolve_perm(false),
                _ => {}
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.agent_busy {
                    if let Some(tx) = &self.agent_tx {
                        let _ = tx.send(AgentCmd::Cancel);
                    }
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('q') if ctrl => self.quit = true,
            KeyCode::F(2) => {
                self.manual_mute = !self.manual_mute;
                self.apply_mic();
            }
            KeyCode::F(3) => self.open_picker(PickKind::Voice),
            KeyCode::F(4) => self.open_picker(PickKind::LiveModel),
            KeyCode::F(5) => self.open_picker(PickKind::AgentModel),
            KeyCode::F(7) => self.save_transcript(),
            KeyCode::F(6) => {
                self.cfg.mute_on_blur = !self.cfg.mute_on_blur;
                self.cfg.save();
                self.apply_mic();
                self.sys(format!("mute on blur: {}", self.cfg.mute_on_blur));
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                if !text.trim().is_empty() {
                    self.transcript.push(Entry {
                        who: "you",
                        text: text.clone(),
                        open: false,
                    });
                    let _ = self.live_tx.send(LiveCmd::Text(text));
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => self.input.clear(),
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            _ => {}
        }
    }

    /// Writes conversation and agent log to ./voxterm-<unix-time>.md in the working directory.
    fn save_transcript(&mut self) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join(format!("voxterm-{stamp}.md"));
        let mut out = String::from("# voxterm transcript\n\n## Conversation\n\n");
        for e in &self.transcript {
            out.push_str(&format!("- **{}:** {}\n", e.who, e.text));
        }
        out.push_str("\n## Agent\n\n```\n");
        out.push_str(&self.agent_log.join("\n"));
        out.push_str("\n```\n");
        match std::fs::write(&path, out) {
            Ok(()) => self.sys(format!("saved {}", path.display())),
            Err(e) => self.sys(format!("save failed: {e}")),
        }
    }

    fn open_picker(&mut self, kind: PickKind) {
        let (title, items, current) = match kind {
            PickKind::Voice => (
                " voice · enter select · p preview ",
                VOICES
                    .iter()
                    .map(|v| (v.to_string(), v.to_string()))
                    .collect(),
                self.cfg.voice.clone(),
            ),
            PickKind::LiveModel => (
                " live model ",
                self.live_models
                    .iter()
                    .map(|m| (m.clone(), m.clone()))
                    .collect(),
                self.cfg.live_model.clone(),
            ),
            PickKind::AgentModel => (" agent model ", self.agent_models.clone(), String::new()),
        };
        if items.is_empty() {
            self.sys("nothing to pick yet; models may still be loading");
            return;
        }
        let sel = items.iter().position(|(v, _)| *v == current).unwrap_or(0);
        self.picker = Some(Picker {
            kind,
            title,
            items,
            sel,
        });
    }

    fn on_picker_key(&mut self, code: KeyCode) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        let last = p.items.len().saturating_sub(1);
        match code {
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => p.sel = (p.sel + 1).min(last),
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                if let Some(p) = self.picker.take() {
                    self.apply_pick(p);
                }
            }
            KeyCode::Char('p') if p.kind == PickKind::Voice => {
                let voice = p.items[p.sel].0.clone();
                self.preview(voice);
            }
            _ => {}
        }
    }

    fn apply_pick(&mut self, p: Picker) {
        let (val, _) = p.items[p.sel].clone();
        dbg(format!("pick {} {}", p.title, val));
        match p.kind {
            PickKind::Voice => {
                self.cfg.voice = val.clone();
                self.cfg.save();
                self.reconfigure();
                self.sys(format!(
                    "voice: {val}. live session reconnects; context resets"
                ));
            }
            PickKind::LiveModel => {
                self.cfg.live_model = val.clone();
                self.cfg.save();
                self.reconfigure();
                self.sys(format!(
                    "live model: {val}. live session reconnects; context resets"
                ));
            }
            PickKind::AgentModel => {
                if let Some(tx) = &self.agent_tx {
                    let _ = tx.send(AgentCmd::SetModel(val.clone()));
                }
                self.sys(format!("agent model: {val}"));
            }
        }
    }

    fn preview(&mut self, voice: String) {
        let _ = self.audio_tx.send(AudioCmd::Flush);
        let s = Settings {
            model: self.cfg.live_model.clone(),
            voice: voice.clone(),
        };
        let (key, ui) = (self.key.clone(), self.ui_tx.clone());
        let text = format!("Hi, I'm {voice}. This is how I sound.");
        tokio::spawn(async move {
            let msg = match live::once(&key, &s, &text).await {
                Ok(samples) => {
                    dbg(format!("preview {} samples", samples.len()));
                    Ui::Preview(samples)
                }
                Err(e) => Ui::Status(format!("preview failed: {e}")),
            };
            let _ = ui.send(msg);
        });
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    let mut t: Vec<char> = s.chars().rev().take(n).collect();
    t.reverse();
    t.into_iter().collect()
}

/// Wrapped row count for one line, used to keep the newest line in view.
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

fn pane(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>, accent: Color) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().dark_gray())
        .title(Line::styled(
            format!(" {title} "),
            Style::new().fg(accent).bold(),
        ));
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

/// Header state: label and colour. Red = retrying, magenta = speaking, yellow = muted,
/// green = listening, cyan = connecting.
fn header_state(app: &App) -> (&'static str, Color) {
    let live_up = app.live_status.starts_with("live: ") && !app.live_status.contains("retrying");
    let speaking = app
        .last_audio
        .is_some_and(|t| t.elapsed() < Duration::from_millis(800));
    if app.live_status.contains("retrying") {
        ("RETRYING", Color::Red)
    } else if speaking {
        ("SPEAKING", Color::Magenta)
    } else if !app.mic_wanted() {
        ("MUTED", Color::Yellow)
    } else if live_up {
        ("LISTENING", Color::Green)
    } else {
        ("CONNECTING", Color::Cyan)
    }
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    app.transcript
        .iter()
        .map(|e| {
            let (label, label_style, text_style) = match e.who {
                "you" => ("you ", Style::new().cyan().bold(), Style::new()),
                "asst" => ("asst", Style::new().green().bold(), Style::new()),
                _ => ("sys ", Style::new().dark_gray(), Style::new().dark_gray()),
            };
            Line::from(vec![
                Span::styled(format!("{label}  "), label_style),
                Span::styled(e.text.clone(), text_style),
            ])
        })
        .collect()
}

fn agent_lines(app: &App) -> Vec<Line<'static>> {
    app.agent_log
        .iter()
        .map(|l| {
            let style = if l.starts_with('>') {
                Style::new().cyan()
            } else if l.starts_with('[') {
                Style::new().dark_gray()
            } else if l.starts_with("done") || l.starts_with("agent ready") {
                Style::new().green()
            } else if l.starts_with("error") {
                Style::new().red().bold()
            } else if l.starts_with('?') || l.starts_with("permission") {
                Style::new().yellow()
            } else {
                Style::new()
            };
            Line::styled(l.clone(), style)
        })
        .collect()
}

fn hint_line() -> Line<'static> {
    let keys = [
        ("F2", "mic"),
        ("F3", "voice"),
        ("F4", "live"),
        ("F5", "agent"),
        ("F6", "blur"),
        ("F7", "save"),
        ("ctrl-c", "cancel"),
        ("ctrl-q", "quit"),
    ];
    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().white().on_dark_gray().bold(),
        ));
        spans.push(Span::styled(
            format!(" {label}  "),
            Style::new().dark_gray(),
        ));
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, app: &App) {
    let [top, main, input, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let (state, color) = header_state(app);
    let agent = if app.agent_tx.is_none() {
        "off"
    } else if app.agent_busy {
        "busy"
    } else {
        "idle"
    };
    let header = Line::from(vec![
        Span::styled(" voxterm ", Style::new().bold()),
        Span::styled(format!(" {state} "), Style::new().black().bg(color).bold()),
        Span::styled(
            format!(
                "  {} · {} · agent {}  ",
                app.cfg.voice, app.cfg.live_model, agent
            ),
            Style::new().dark_gray(),
        ),
        Span::styled(app.live_status.clone(), Style::new().dark_gray()),
    ]);
    f.render_widget(Paragraph::new(header), top);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);
    pane(f, left, "conversation", transcript_lines(app), Color::Cyan);
    pane(f, right, "agent", agent_lines(app), Color::Yellow);

    let prompt = if app.input.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::new().cyan().bold()),
            Span::styled("type to talk", Style::new().dark_gray()),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", Style::new().cyan().bold()),
            Span::raw(app.input.clone()),
        ])
    };
    f.render_widget(
        Paragraph::new(prompt).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().cyan())
                .title(" message · enter sends "),
        ),
        input,
    );
    f.set_cursor_position(Position::new(
        input.x + 3 + app.input.chars().count() as u16,
        input.y + 1,
    ));
    f.render_widget(Paragraph::new(hint_line()), hint);

    if let Some(p) = &app.perm {
        let area = centered(f.area(), 64, 8);
        f.render_widget(Clear, area);
        let text = vec![
            Line::styled(p.title.clone(), Style::new().bold()),
            Line::styled(format!("kind: {}", p.kind), Style::new().dark_gray()),
            Line::from(""),
            Line::from(vec![
                Span::styled(" y ", Style::new().black().on_green().bold()),
                Span::raw(" allow    "),
                Span::styled(" n ", Style::new().white().on_red().bold()),
                Span::raw(" deny    "),
                Span::styled("esc", Style::new().dark_gray()),
                Span::styled(" deny", Style::new().dark_gray()),
            ]),
        ];
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().yellow())
                    .title(Line::styled(
                        " agent asks permission ",
                        Style::new().yellow().bold(),
                    )),
            ),
            area,
        );
    }
    if let Some(p) = &app.picker {
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
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().cyan())
                    .title(Line::styled(p.title, Style::new().cyan().bold())),
            )
            .highlight_style(Style::new().bg(Color::DarkGray).bold())
            .highlight_symbol("▸ ");
        f.render_stateful_widget(list, area, &mut state);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| anyhow!("GEMINI_API_KEY is not set"))?;
    if std::env::args().any(|a| a == "--selftest") {
        return selftest(&key).await;
    }

    let cfg = Config::load();
    let cwd = std::env::current_dir()?;
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Ui>();

    let (live_tx, live_rx) = mpsc::unbounded_channel::<LiveCmd>();
    let (live_ev_tx, live_ev_rx) = mpsc::unbounded_channel::<LiveEvent>();
    forward(live_ev_rx, ui_tx.clone(), Ui::Live);
    tokio::spawn(live::run(
        key.clone(),
        settings_of(&cfg),
        live_rx,
        live_ev_tx,
    ));

    let (err_tx, err_rx) = mpsc::unbounded_channel::<String>();
    forward(err_rx, ui_tx.clone(), Ui::Status);
    let audio_tx = audio::spawn(
        live_tx.clone(),
        err_tx.clone(),
        cfg.echo_gate,
        Duration::from_millis(cfg.echo_tail_ms),
        cfg.mic_min_rms,
    );

    let (agent_ev_tx, agent_ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
    forward(agent_ev_rx, ui_tx.clone(), Ui::Agent);
    let cmd: Vec<String> = cfg.agent_cmd.split_whitespace().map(String::from).collect();
    let agent_tx = match acp::start(&cmd, &cwd, agent_ev_tx).await {
        Ok(tx) => Some(tx),
        Err(e) => {
            let _ = err_tx.send(format!("agent: {e}"));
            None
        }
    };

    let (ui2, key2) = (ui_tx.clone(), key.clone());
    tokio::spawn(async move {
        if let Ok(models) = live::list_models(&key2).await {
            let _ = ui2.send(Ui::Models(models));
        }
    });

    let mut app = App {
        cfg,
        key,
        live_tx,
        agent_tx,
        audio_tx,
        ui_tx,
        transcript: vec![],
        agent_log: vec![],
        agent_last: String::new(),
        agent_busy: false,
        agent_models: vec![],
        live_models: vec![],
        live_status: "live: connecting".into(),
        last_audio: None,
        input: String::new(),
        manual_mute: false,
        focused: true,
        perm: None,
        picker: None,
        quit: false,
    };
    app.apply_mic();

    let mut term = ratatui::init();
    let _ = execute!(io::stdout(), EnableFocusChange);
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let result: Result<()> = loop {
        if let Err(e) = term.draw(|f| draw(f, &app)) {
            break Err(anyhow!(e));
        }
        tokio::select! {
            Some(Ok(ev)) = keys.next() => app.on_term(ev),
            Some(ev) = ui_rx.recv() => app.on_ui(ev),
            _ = tick.tick() => {}
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(io::stdout(), DisableFocusChange);
    ratatui::restore();
    result
}

/// Headless check: ACP prompt writes a file, Live returns audio. Run with `voxterm --selftest`.
async fn selftest(key: &str) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("voxterm-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let agent = acp::start(&["opencode".to_string(), "acp".to_string()], &dir, tx).await?;
    let run = async {
        let mut sent = false;
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::Ready { .. } if !sent => {
                    agent.send(AgentCmd::Prompt(
                        "Create a file named hello.txt containing exactly: hi".into(),
                    ))?;
                    sent = true;
                }
                AgentEvent::Done { .. } => return Ok::<(), anyhow::Error>(()),
                AgentEvent::Error(e) => return Err(anyhow!(e)),
                _ => {}
            }
        }
        Err(anyhow!("agent closed early"))
    };
    tokio::time::timeout(Duration::from_secs(180), run).await??;
    let file = std::fs::read_to_string(dir.join("hello.txt"))?;
    println!("ACP prompt: PASS, hello.txt = {:?}", file.trim());

    let s = settings_of(&Config::default());
    let samples = live::once(key, &s, "Say hello in one short sentence.").await?;
    println!(
        "Live audio: {} samples at 24 kHz, {}",
        samples.len(),
        if samples.is_empty() { "FAIL" } else { "PASS" }
    );
    Ok(())
}

#[cfg(test)]
mod ui_snapshot {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn sample_app() -> App {
        let (live_tx, _) = mpsc::unbounded_channel();
        let (ui_tx, _) = mpsc::unbounded_channel();
        let (audio_tx, _) = std::sync::mpsc::channel();
        let entry = |who: &'static str, text: &str| Entry {
            who,
            text: text.to_string(),
            open: false,
        };
        App {
            cfg: Config::default(),
            key: String::new(),
            live_tx,
            agent_tx: None,
            audio_tx,
            ui_tx,
            transcript: vec![
                entry("you", "add retry to the fetch function"),
                entry(
                    "asst",
                    "Starting that now. I'll tell you when the agent finishes.",
                ),
                entry(
                    "sys",
                    "voice: Puck. live session reconnects; context resets",
                ),
            ],
            agent_log: vec![
                "> add retry to the fetch function".into(),
                "[read src/fetch.rs completed]".into(),
                "· Adding exponential backoff with three tries.".into(),
                "? permission: src/fetch.rs (edit)".into(),
                "done (end_turn)".into(),
            ],
            agent_last: String::new(),
            agent_busy: false,
            agent_models: vec![],
            live_models: vec![],
            live_status: "live: gemini-3.1-flash-live-preview / Kore".into(),
            input: "and add a test".into(),
            manual_mute: false,
            focused: true,
            perm: None,
            picker: None,
            quit: false,
            last_audio: None,
        }
    }

    #[test]
    fn render_snapshot() {
        let app = sample_app();
        let mut term = Terminal::new(TestBackend::new(110, 28)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        println!("{text}");
        assert!(text.contains("LISTENING"));
        assert!(text.contains("conversation"));
    }
}
