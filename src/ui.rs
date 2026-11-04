//! Terminal UI (ratatui). Draws the core's state and sends commands back as JSON.
//! Speaks only about tasks. Never mentions the worker.

use crate::theme::{self, THEMES, Theme};
use crossterm::{
    event::{
        self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Sparkline, Wrap},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io, path::PathBuf, time::Duration};
use tokio::sync::mpsc::{self, UnboundedSender};

pub const VOICES: [&str; 8] = [
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
}

#[derive(Clone, Copy)]
enum Action {
    Voice,
    LiveModel,
    TaskModel,
    ToggleMute,
    MuteOnBlur,
    Save,
    CancelSelected,
    CancelAll,
    NextTheme,
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
}

#[derive(Clone, Default)]
struct Status {
    state: String,
    mic: bool,
    voice: String,
    live_model: String,
    tasks_ready: bool,
}

struct Picker {
    kind: PickKind,
    title: &'static str,
    items: Vec<(String, String)>,
    sel: usize,
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

    fn next_theme(&mut self) {
        let name = THEMES[(self.theme + 1) % THEMES.len()].name;
        self.set_theme(name);
    }

    /// Applies one event from the core.
    pub fn apply(&mut self, ev: Value) {
        match ev["t"].as_str().unwrap_or_default() {
            "say" => {
                let who = ev["who"].as_str().unwrap_or("sys").to_string();
                let text = ev["text"].as_str().unwrap_or_default();
                let cont = ev["cont"].as_bool().unwrap_or(false);
                if cont {
                    if let Some(last) = self.says.last_mut() {
                        if last.open && last.who == who {
                            last.text.push_str(text);
                            return;
                        }
                    }
                }
                let open = who != "sys" && who != "you";
                self.says.push(Say {
                    who,
                    text: text.to_string(),
                    open,
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
        match kind {
            PickKind::Voice => {
                let sel = VOICES
                    .iter()
                    .position(|v| *v == self.status.voice)
                    .unwrap_or(0);
                self.picker = Some(Picker {
                    kind,
                    title: " voice · enter apply · space preview ",
                    items: VOICES
                        .iter()
                        .map(|v| (v.to_string(), v.to_string()))
                        .collect(),
                    sel,
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
                    title: " voice model · enter apply ",
                    items: self
                        .live_models
                        .iter()
                        .map(|m| (m.clone(), m.clone()))
                        .collect(),
                    sel,
                });
            }
            PickKind::Task => {
                if self.task_models.is_empty() {
                    self.note = "task models not ready".into();
                    return;
                }
                self.picker = Some(Picker {
                    kind,
                    title: " task model · enter apply ",
                    items: self.task_models.clone(),
                    sel: 0,
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
            ("Theme: next", "/theme", Action::NextTheme),
            ("Cancel selected task", "ctrl-c", Action::CancelSelected),
            ("Cancel all tasks", "/cancel-all", Action::CancelAll),
            ("Quit", "ctrl-q", Action::Quit),
        ]
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::Voice => self.open_picker(PickKind::Voice),
            Action::LiveModel => self.open_picker(PickKind::Live),
            Action::TaskModel => self.open_picker(PickKind::Task),
            Action::ToggleMute => self.send(json!({"c": "mute", "on": self.status.mic})),
            Action::MuteOnBlur => self.send(json!({"c": "mute_on_blur", "on": false})),
            Action::Save => self.send(json!({"c": "save"})),
            Action::CancelSelected => self.cancel_selected(),
            Action::CancelAll => self.send(json!({"c": "cancel_all"})),
            Action::NextTheme => self.next_theme(),
            Action::Quit => self.send(json!({"c": "quit"})),
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
            "/cancel-all" => return self.send(json!({"c": "cancel_all"})),
            "/quit" => return self.send(json!({"c": "quit"})),
            "/theme" => return self.next_theme(),
            "/help" => {
                return self.palette = Some(Palette {
                    query: String::new(),
                    sel: 0,
                    slash: false,
                });
            }
            _ => {}
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
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => p.sel = (p.sel + 1).min(p.items.len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(p) = self.picker.take() {
                    self.apply_picker(p);
                }
            }
            KeyCode::Char(' ') if p.kind == PickKind::Voice => {
                if let Some((v, _)) = p.items.get(p.sel).cloned() {
                    self.send(json!({"c": "preview", "voice": v}));
                }
            }
            _ => {}
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
                KeyCode::Char('k') => {
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

fn panel(title: &str, color: Color, t: &Theme) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::styled(
            format!(" {title} "),
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
}

fn pane(
    f: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    border: Color,
    t: &Theme,
) {
    let block = panel(title, border, t);
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

fn draw_levels(f: &mut Frame, area: Rect, mic: &[u64], out: &[u64], t: &Theme) {
    let block = panel("levels", t.border, t);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [mic_row, out_row] = Layout::vertical([Constraint::Length(2), Constraint::Length(2)]).areas(inner);
    let width = inner.width.saturating_sub(7) as usize;
    let tail = |h: &[u64]| -> Vec<u64> { h.iter().rev().take(width).rev().copied().collect() };
    let [mic_label, mic_spark] = Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).areas(mic_row);
    f.render_widget(Paragraph::new("mic    ").style(Style::new().fg(t.dim)), mic_label);
    f.render_widget(Sparkline::default().data(&tail(mic)).max(100).style(Style::new().fg(t.good)), mic_spark);
    let [out_label, out_spark] = Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).areas(out_row);
    f.render_widget(Paragraph::new("speak  ").style(Style::new().fg(t.dim)), out_label);
    f.render_widget(Sparkline::default().data(&tail(out)).max(100).style(Style::new().fg(t.special)), out_spark);
}

impl App {
    pub fn draw(&self, f: &mut Frame) {
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
        f.render_widget(
            Paragraph::new(header).style(Style::new().bg(t.header_bg)),
            top,
        );

        // Panes
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main);
        let conv_border = if self.focus == Focus::Conversation {
            t.accent
        } else {
            t.border
        };
        let says: Vec<Line> = self
            .says
            .iter()
            .map(|s| {
                let (label, lc) = match s.who.as_str() {
                    "you" => ("you ", t.accent),
                    "asst" => ("asst", t.good),
                    _ => ("sys ", t.muted),
                };
                let tc = if s.who == "sys" { t.muted } else { t.fg };
                Line::from(vec![
                    Span::styled(
                        format!("{label}  "),
                        Style::new().fg(lc).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(s.text.clone(), Style::new().fg(tc)),
                ])
            })
            .collect();
        pane(f, left, "conversation", says, conv_border, &t);

        let (task_list, log, levels) = if self.log_open {
            let [a, b, c] = Layout::vertical([Constraint::Min(3), Constraint::Percentage(35), Constraint::Length(5)]).areas(right);
            (a, b, c)
        } else {
            let [a, c] = Layout::vertical([Constraint::Min(3), Constraint::Length(5)]).areas(right);
            (a, Rect::default(), c)
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
            let mark = if i == self.sel_task && self.focus == Focus::Tasks {
                "▸"
            } else {
                " "
            };
            task_lines.push(Line::from(vec![
                Span::styled(format!("{mark} {} ", task.id), Style::new().fg(t.fg)),
                Span::styled(
                    format!("{word:<10}"),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(task.title.clone(), Style::new().fg(t.fg)),
            ]));
        }
        pane(f, task_list, "tasks", task_lines, task_border, &t);
        draw_levels(f, levels, &self.mic_hist, &self.out_hist, &t);
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
                    &format!("task {} log", task.id),
                    lines,
                    t.border,
                    &t,
                );
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
        f.render_widget(
            Paragraph::new(prompt).block(panel("message · enter sends", input_border, &t)),
            input,
        );
        if self.focus == Focus::Input {
            f.set_cursor_position(Position::new(
                input.x + 3 + self.input.chars().count() as u16,
                input.y + 1,
            ));
        }

        // Hint line
        let hint_text = if !self.note.is_empty() {
            self.note.clone()
        } else {
            match self.focus {
                Focus::Tasks => {
                    "↑↓ select · enter log · ctrl-c cancel · tab next · ctrl-k commands".into()
                }
                Focus::Conversation => "tab next · ctrl-t mute · ctrl-k commands".into(),
                Focus::Input => "enter send · ctrl-t mute · tab focus · ctrl-k commands".into(),
            }
        };
        f.render_widget(
            Paragraph::new(hint_text).style(Style::new().fg(t.muted)),
            hint,
        );

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
            let h = (items.len() as u16 + 4).min(f.area().height);
            let area = centered(f.area(), (f.area().width / 2).max(44), h);
            f.render_widget(Clear, area);
            let mut lines: Vec<Line> = vec![Line::styled(
                if p.slash {
                    format!("/{q}")
                } else {
                    format!("› {q}")
                },
                Style::new().fg(t.fg),
            )];
            for (i, (label, keys, _)) in items.iter().enumerate() {
                let (fg, bg) = if i == p.sel {
                    (t.on_accent, t.accent)
                } else {
                    (t.dim, t.header_bg)
                };
                lines.push(Line::styled(
                    format!("  {label}   {keys}"),
                    Style::new().fg(fg).bg(bg),
                ));
            }
            lines.push(Line::styled(
                "  ↑↓ move · enter run · esc close",
                Style::new().fg(t.muted),
            ));
            f.render_widget(
                Paragraph::new(lines).block(panel("commands", t.accent, &t)),
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
                .block(panel(p.title, t.accent, &t))
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
    let _ = execute!(io::stdout(), EnableFocusChange);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
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
                Event::FocusGained => app.on_focus(true),
                Event::FocusLost => app.on_focus(false),
                _ => {}
            },
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
