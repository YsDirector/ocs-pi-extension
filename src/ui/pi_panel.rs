//! **OCS Pi Extension**: the built-in Pi assistant panel — a native chat view
//! over the local `pi-web` HTTP API.
//!
//! Everything here is plain iced — same header chrome (pin / close / drag),
//! same dock behaviour, same theme and DPI handling as the Properties panel,
//! and it renders natively on both X11 and Wayland. No child windows, no
//! external processes, nothing to align.
//!
//! Data source (all local, plain HTTP + JSON, see `crate::pi`):
//!   GET  {endpoint}/api/sessions                 session list
//!   GET  {endpoint}/api/agent/<id>/events        SSE transcript/tool stream
//!   POST {endpoint}/api/agent/<id> {"type":"prompt",…}  send a message
//!
//! The client lives on a worker thread; `PiPanelState::apply` folds its events
//! into render state. This module owns the state and the widgets.

use std::collections::{HashSet, VecDeque};

use iced::widget::{
    button, column, container, mouse_area, pick_list, row, scrollable, text, text_editor, tooltip,
    Space,
};
use iced::{Background, Border, Color, Element, Font, Length, Theme};

use crate::app::Message;
use crate::pi::{self, Entry, Event, Part, Status};

/// Scrollable widget id of the transcript (for scroll-to-bottom).
pub const TRANSCRIPT_ID: &str = "pi_transcript_scroll";
/// Fixed composer height in px.
const COMPOSER_H: f32 = 72.0;
/// Muted-but-readable grey for the composer hint and the stats line
/// (#B1B1B1) — the theme's secondary text is too dim on the dark panel.
const MUTED_TEXT: Color = Color::from_rgb8(177, 177, 177);
/// Live entry ids start above this base so they never collide with the
/// content-hashed ids of backfilled (`Replace`) entries.
const LIVE_ID_BASE: u64 = 1 << 62;

// ── Sub-messages (routed through `Message::Pi`) ─────────────────────────────

#[derive(Debug, Clone)]
pub enum PiMsg {
    /// 10 Hz drain of the worker channel.
    Poll,
    /// The composer editor processed an action.
    Editor(text_editor::Action),
    /// Send the composer text (Enter or the send button).
    Send,
    /// Switch the followed session.
    SessionPick(String),
    /// Expand/collapse a transcript entry (thinking/tool/…).
    ToggleEntry(u64),
    /// Connect (or drop and reconnect) the worker.
    Reconnect,
    /// Paste from the clipboard: an image becomes the attachment, otherwise
    /// the text falls back into the editor.
    Paste,
    /// Drop the attached image.
    ClearImage,
    /// Switch the model the session uses.
    ModelPick(pi::ModelInfo),
    /// Switch the session's reasoning/thinking level.
    ThinkingPick(String),
    /// Switch the transport backend (`auto` | `web` | `rpc`) — restarts the
    /// worker, because the two backends have different session stores and
    /// transports.
    BackendPick(String),
    /// Completion popup: move the highlight.
    MenuUp,
    MenuDown,
    /// Completion popup: accept the highlighted item.
    MenuAccept,
    /// Completion popup: dismiss.
    MenuClose,
    /// Open the new-session directory browser.
    NewSessionOpen,
    /// Close the new-session browser.
    NewSessionCancel,
    /// Browse into a directory.
    DirOpen(String),
    /// Browse to the parent directory.
    DirUp,
    /// Create a session in the browsed directory.
    CreateSession,
    /// Answer a pending extension UI request (`select` option index or
    /// confirm value; `value` is `None` for confirm).
    UiAnswer { value: Option<String>, confirmed: Option<bool> },
    /// Dismiss a pending extension UI request (cancelled).
    UiCancel,
    /// Edit the free-text answer of an input/editor request.
    UiEdit(text_editor::Action),
    /// Submit the free-text answer.
    UiSubmit,
    /// Manually compact the conversation context.
    Compact,
}

// ── View model ──────────────────────────────────────────────────────────────

/// One rendered transcript entry.
pub enum PiEntryKind {
    User(String),
    /// Assistant text + parsed markdown (and table segments when present).
    Assistant(AssistantBody),
    Thinking(String),
    Tool { id: String, name: String, output: String, done: bool, is_error: bool },
    Notice(String),
}

/// One rendered piece of an assistant message: markdown or a pipe table.
/// (`markdown::Content` isn't `Clone`, so segments are built in place.)
#[derive(Debug)]
pub enum MdSegment {
    Markdown {
        text: String,
        md: iced::widget::markdown::Content,
    },
    /// `[header, body rows…]`, cells trimmed.
    Table(Vec<Vec<String>>),
}

/// Assistant message content: the raw text, its parsed markdown, and — only
/// when the text contains pipe tables — a table-aware segmentation.
pub struct AssistantBody {
    pub text: String,
    pub md: iced::widget::markdown::Content,
    /// `None` when the message has no table (fast path at render time).
    pub table_split: Option<Vec<MdSegment>>,
}

impl AssistantBody {
    pub fn new(text: String) -> Self {
        let md = iced::widget::markdown::Content::parse(&text);
        let table_split = if text.contains('|') {
            let segments = split_tables(&text);
            segments
                .iter()
                .any(|s| matches!(s, MdSegment::Table(_)))
                .then_some(segments)
        } else {
            None
        };
        Self { text, md, table_split }
    }
}

/// Split raw markdown into markdown and table segments. A table starts at a
/// `|`-prefixed line whose next line is a `|---|` separator.
pub fn split_tables(text: &str) -> Vec<MdSegment> {
    let lines: Vec<&str> = text.lines().collect();
    let is_row = |line: &str| line.trim_start().starts_with('|');
    let is_separator = |line: &str| {
        let trimmed = line.trim();
        trimmed.starts_with('|')
            && trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
            && trimmed.contains('-')
    };
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect()
    };
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < lines.len() {
        let starts_table = is_row(lines[i]) && i + 1 < lines.len() && is_separator(lines[i + 1]);
        if !starts_table {
            plain.push_str(lines[i]);
            plain.push('\n');
            i += 1;
            continue;
        }
        if !plain.trim().is_empty() {
            let chunk = std::mem::take(&mut plain);
            let md = iced::widget::markdown::Content::parse(&chunk);
            segments.push(MdSegment::Markdown { text: chunk, md });
        }
        let mut rows = vec![cells(lines[i])];
        i += 2; // header + separator
        while i < lines.len() && is_row(lines[i]) {
            rows.push(cells(lines[i]));
            i += 1;
        }
        segments.push(MdSegment::Table(rows));
    }
    if !plain.trim().is_empty() {
        let md = iced::widget::markdown::Content::parse(&plain);
        segments.push(MdSegment::Markdown { text: plain, md });
    }
    segments
}

/// A transcript entry plus per-entry UI state (collapsed/expanded).
/// (`markdown::Content` isn't `Clone`, so entries are built in place.)
pub struct PiEntry {
    pub id: u64,
    pub kind: PiEntryKind,
    pub expanded: bool,
}

/// Blocks of the assistant message currently streaming in.
#[derive(Debug, Default)]
pub struct StreamingMsg {
    pub blocks: Vec<StreamBlock>,
}

#[derive(Debug)]
pub enum StreamBlock {
    Thinking { idx: usize, text: String },
    /// Live assistant text with its incrementally parsed markdown.
    Text { idx: usize, text: String, md: iced::widget::markdown::Content },
}

/// Completion popup above the composer (`/` commands or `@` files).
#[derive(Debug, Clone, PartialEq)]
pub enum Popup {
    Slash {
        query: String,
        /// Indices into `PiPanelState::commands`.
        matches: Vec<usize>,
        active: usize,
    },
    At {
        query: String,
        /// Matching file paths (relative to the session cwd).
        matches: Vec<String>,
        active: usize,
    },
}

impl Popup {
    fn len(&self) -> usize {
        match self {
            Popup::Slash { matches, .. } => matches.len(),
            Popup::At { matches, .. } => matches.len(),
        }
    }

    fn active(&self) -> usize {
        match self {
            Popup::Slash { active, .. } | Popup::At { active, .. } => *active,
        }
    }

    fn move_active(&mut self, delta: isize) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let current = self.active() as isize;
        let next = (current + delta).rem_euclid(len as isize) as usize;
        match self {
            Popup::Slash { active, .. } | Popup::At { active, .. } => *active = next,
        }
    }
}

/// A pending extension UI request (approval dialog) awaiting an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUi {
    pub id: String,
    pub method: String,
    pub title: String,
    pub message: Option<String>,
    pub options: Vec<String>,
}

/// New-session directory browser state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowseState {
    pub path: String,
    pub parent: Option<String>,
    /// `(name, absolute path)` of sub-directories.
    pub dirs: Vec<(String, String)>,
    pub loading: bool,
}

/// Connection status shown in the status strip.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PiStatus {
    /// Not started yet — no worker running.
    #[default]
    Idle,
    Connecting,
    Ready { session: String, streaming: bool },
    Error(String),
}

/// A badge next to a tool row: (label, palette swatch to take the color from).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Badge {
    Running,
    Done,
    Failed,
}

impl Badge {
    fn label(self) -> &'static str {
        match self {
            Badge::Running => "运行中…",
            Badge::Done => "完成",
            Badge::Failed => "出错",
        }
    }

    fn color(self, theme: &Theme) -> Color {
        let palette = theme.palette();
        match self {
            Badge::Running => palette.warning.base.color,
            Badge::Done => palette.success.base.color,
            Badge::Failed => palette.danger.base.color,
        }
    }
}

/// State for one document tab's Pi panel.
pub struct PiPanelState {
    /// Base URL of the local `pi-web` server (no trailing slash).
    pub endpoint: String,
    pub status: PiStatus,
    /// Session list (`/api/sessions`), newest activity first.
    pub sessions: Vec<pi::SessionInfo>,
    /// The session being followed (default: the newest-active one).
    pub active: Option<String>,
    pub entries: Vec<PiEntry>,
    pub streaming: StreamingMsg,
    /// Composer text.
    pub input: text_editor::Content,
    /// Echo tickets: texts optimistically inserted, waiting for the SSE echo
    /// (`message_end` role=user) to be consumed. FIFO — queued follow-ups
    /// deliver in send order.
    pub pending_echoes: VecDeque<String>,
    /// Server-side follow-up queue length (`queue_update`), if known.
    pub queued_followups: Option<usize>,
    /// Model catalog (`/api/models`) + the model the session currently uses.
    pub models: Vec<pi::ModelInfo>,
    pub current_model: Option<(String, String)>,
    /// Available thinking levels per `"provider:id"` + the current level.
    pub thinking_levels: Vec<(String, Vec<String>)>,
    pub current_thinking: Option<String>,
    /// Slash commands from `get_commands`.
    pub commands: Vec<pi::CommandInfo>,
    /// File index `(cwd, files)` for `@` references.
    pub files: Option<(String, Vec<String>)>,
    /// A `FetchFiles` request is in flight for this cwd.
    files_requested: Option<String>,
    /// Completion popup (`/` or `@`) above the composer.
    pub popup: Option<Popup>,
    /// New-session directory browser.
    pub browse: Option<BrowseState>,
    /// Pending extension UI request (approval dialog).
    pub pending_ui: Option<PendingUi>,
    /// Backend label shown in the status strip ("pi-web …" / "pi rpc …").
    pub backend: String,
    /// Token/cost/context accounting (`get_session_stats`).
    pub stats: Option<pi::SessionStats>,
    /// Free-text answer buffer for `input`/`editor` UI requests.
    pub ui_answer: text_editor::Content,
    /// How to reach pi: "auto" | "web" | "rpc" (`OCS_PI_MODE`).
    pub mode: String,
    /// `pi` executable for RPC mode (`OCS_PI_BIN`).
    pub pi_bin: String,
    /// Project directory for RPC mode (`OCS_PI_CWD`, default `$HOME`).
    pub project_cwd: String,
    /// Screenshot attached to the next message (raw base64 PNG).
    pub pending_image: Option<pi::ImageAttachment>,
    /// Image dimensions for the attachment chip.
    pub pending_image_size: Option<(u32, u32)>,
    /// Canvas selection summary shown above the composer ("圆弧（1）"…).
    pub selection_label: String,
    /// Selection fingerprint the label was computed from (skip per-frame
    /// rescans while the selection is unchanged).
    pub(crate) selection_fingerprint: u64,
    /// Expand flags for *live* blocks (streaming thinking), keyed by block id
    /// — they don't exist as entries yet.
    live_expanded: HashSet<u64>,
    /// Monotonic id source for live entries.
    next_id: u64,
    /// Worker handle (None while stopped).
    pub worker: Option<crate::pi::PiHandle>,
}

impl Default for PiPanelState {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("OCS_PI_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:30141".to_string()),
            status: PiStatus::Idle,
            sessions: Vec::new(),
            active: None,
            entries: Vec::new(),
            streaming: StreamingMsg::default(),
            input: text_editor::Content::new(),
            pending_echoes: VecDeque::new(),
            queued_followups: None,
            models: Vec::new(),
            current_model: None,
            thinking_levels: Vec::new(),
            current_thinking: None,
            commands: Vec::new(),
            files: None,
            files_requested: None,
            popup: None,
            browse: None,
            pending_ui: None,
            backend: String::new(),
            stats: None,
            ui_answer: text_editor::Content::new(),
            mode: crate::pi::initial_backend_mode(),
            pi_bin: std::env::var("OCS_PI_BIN").unwrap_or_else(|_| "pi".to_string()),
            project_cwd: std::env::var("OCS_PI_CWD")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| "/".to_string()),
            pending_image: None,
            pending_image_size: None,
            selection_label: String::new(),
            selection_fingerprint: 0,
            live_expanded: HashSet::new(),
            next_id: 0,
            worker: None,
        }
    }
}

impl PiPanelState {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Start the worker if none is running. `auto` tries pi-web first and
    /// falls back to a local `pi --mode rpc` child process.
    pub fn ensure_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let handle = match self.mode.as_str() {
            "web" => crate::pi::PiHandle::start(&self.endpoint),
            "rpc" => crate::pi::PiHandle::start_rpc(&self.pi_bin, &self.project_cwd),
            _ => crate::pi::PiHandle::start_auto(
                &self.endpoint,
                &self.pi_bin,
                &self.project_cwd,
            ),
        };
        self.worker = Some(handle);
    }

    /// Stop the worker (panel closed).
    pub fn stop_worker(&mut self) {
        if let Some(w) = &self.worker {
            w.stop();
        }
        self.worker = None;
    }

    /// Forget everything derived from the current worker before switching
    /// backends: sessions, transcript, model/thinking catalog, stats and any
    /// pending UI request all belong to the transport that produced them.
    pub fn reset_for_backend_switch(&mut self) {
        self.status = PiStatus::Idle;
        self.backend.clear();
        self.sessions.clear();
        self.active = None;
        self.entries.clear();
        self.streaming = StreamingMsg::default();
        self.pending_echoes.clear();
        self.queued_followups = None;
        self.models.clear();
        self.current_model = None;
        self.thinking_levels.clear();
        self.current_thinking = None;
        self.commands.clear();
        self.files = None;
        self.files_requested = None;
        self.popup = None;
        self.browse = None;
        self.pending_ui = None;
        self.stats = None;
    }

    pub fn send_command(&self, cmd: pi::Command) {
        if let Some(w) = &self.worker {
            let _ = w.tx.send(cmd);
        }
    }

    /// Render the panel (delegates to the free `view`). `theme` feeds the
    /// markdown renderer (it resolves colors eagerly, not via style closures).
    pub fn view<'a>(
        &'a self,
        width: f32,
        auto_collapse: bool,
        selection_label: &'a str,
        theme: &'a Theme,
    ) -> Element<'a, Message> {
        view(self, width, auto_collapse, selection_label, theme)
    }
    /// Whether an agent run is currently streaming.
    pub fn is_streaming(&self) -> bool {
        matches!(&self.status, PiStatus::Ready { streaming: true, .. })
    }

    /// Human-readable status for the status strip.
    pub fn status_label(&self) -> String {
        match &self.status {
            PiStatus::Idle => "未连接".to_string(),
            PiStatus::Connecting => "正在连接 pi-web…".to_string(),
            PiStatus::Ready { session, streaming } => {
                let dot = if *streaming { "● 生成中" } else { "○ 空闲" };
                let short: String = session.chars().take(8).collect();
                format!("{dot} · 会话 {short}")
            }
            PiStatus::Error(e) => format!("连接失败：{e}"),
        }
    }

    /// Fold one worker event into render state. Returns whether transcript
    /// content changed (→ the caller may scroll to the bottom).
    pub fn apply(&mut self, ev: Event) -> bool {
        match ev {
            Event::Status(s) => {
                // A fresh connection/session — pull the token accounting so the
                // stats line is populated right away.
                if matches!(s, Status::Ready { .. }) {
                    self.send_command(pi::Command::FetchStats);
                }
                self.status = match &s {
                    Status::Connecting => PiStatus::Connecting,
                    Status::Ready { session, streaming } => {
                        self.active = Some(session.clone());
                        PiStatus::Ready {
                            session: session.clone(),
                            streaming: *streaming,
                        }
                    }
                    Status::Error(e) => PiStatus::Error(e.clone()),
                };
                false
            }
            Event::Sessions(list) => {
                self.sessions = list;
                // Default: follow the newest-active session until the user
                // picks one explicitly.
                if self.active.is_none() {
                    self.active = self.sessions.first().map(|s| s.id.clone());
                }
                false
            }
            Event::Replace(entries) => {
                self.entries = entries.into_iter().map(stable_entry).collect();
                self.streaming = StreamingMsg::default();
                true
            }
            Event::User(msg_text) => {
                // Consume the matching optimistic bubble instead of doubling.
                if self.pending_echoes.front().map(|f| *f == msg_text).unwrap_or(false) {
                    self.pending_echoes.pop_front();
                    return false;
                }
                // An empty echo with no ticket is just an image-only message
                // we already showed optimistically — don't render an empty
                // bubble.
                if msg_text.is_empty() {
                    return false;
                }
                if matches!(self.entries.last().map(|e| &e.kind), Some(PiEntryKind::User(t)) if *t == msg_text) {
                    return false;
                }
                self.push_kind(PiEntryKind::User(msg_text));
                true
            }
            Event::Models { list, current, thinking_levels, thinking } => {
                self.models = list;
                self.thinking_levels = thinking_levels;
                if current.is_some() {
                    self.current_model = current;
                }
                if thinking.is_some() {
                    self.current_thinking = thinking;
                }
                false
            }
            Event::Commands(list) => {
                self.commands = list;
                // A popup may be open right now with a stale (empty) list.
                self.refresh_popup();
                false
            }
            Event::Files { cwd, files } => {
                self.files = Some((cwd, files));
                self.files_requested = None;
                self.refresh_popup();
                false
            }
            Event::DirListing { path, parent, dirs } => {
                if let Some(browse) = &mut self.browse {
                    browse.path = path;
                    browse.parent = parent;
                    browse.dirs = dirs;
                    browse.loading = false;
                }
                false
            }
            Event::ThinkingSet { level } => {
                self.current_thinking = Some(level);
                false
            }
            Event::Notice(message) => {
                self.push_notice(message);
                true
            }
            Event::Backend(label) => {
                self.backend = label;
                false
            }
            Event::UiRequest { id, method, title, message, options, prefill } => {
                if let Some(prefill) = &prefill {
                    self.ui_answer = text_editor::Content::with_text(prefill);
                } else {
                    self.ui_answer = text_editor::Content::new();
                }
                self.pending_ui = Some(PendingUi { id, method, title, message, options });
                true
            }
            Event::Stats(stats) => {
                self.stats = Some(stats);
                false
            }
            Event::ModelSet { provider, id } => {
                self.current_model = Some((provider, id));
                false
            }
            Event::MsgStart { parts } => {
                self.streaming = StreamingMsg {
                    blocks: parts
                        .iter()
                        .filter_map(|(idx, part)| match part {
                            Part::Thinking(t) => {
                                Some(StreamBlock::Thinking { idx: *idx, text: t.clone() })
                            }
                            Part::Text(t) => Some(StreamBlock::Text {
                                idx: *idx,
                                text: t.clone(),
                                md: iced::widget::markdown::Content::parse(t),
                            }),
                            Part::ToolCall { .. } => None,
                        })
                        .collect(),
                };
                for (_, part) in &parts {
                    if let Part::ToolCall { id, name } = part {
                        self.ensure_tool(id, name);
                    }
                }
                true
            }
            Event::Delta { kind, idx, chunk } => {
                let blocks = &mut self.streaming.blocks;
                if let Some(slot) = blocks.iter_mut().find(|b| match (kind, b) {
                    (pi::DeltaKind::Text, StreamBlock::Text { idx: i, .. }) => *i == idx,
                    (pi::DeltaKind::Thinking, StreamBlock::Thinking { idx: i, .. }) => *i == idx,
                    _ => false,
                }) {
                    match slot {
                        StreamBlock::Text { text: t, md, .. } => {
                            t.push_str(&chunk);
                            md.push_str(&chunk); // incremental markdown parse
                        }
                        StreamBlock::Thinking { text: t, .. } => t.push_str(&chunk),
                    }
                } else {
                    blocks.push(match kind {
                        pi::DeltaKind::Text => StreamBlock::Text {
                            idx,
                            text: chunk.clone(),
                            md: iced::widget::markdown::Content::parse(&chunk),
                        },
                        pi::DeltaKind::Thinking => StreamBlock::Thinking { idx, text: chunk },
                    });
                }
                true
            }
            Event::AssistantEnd { parts } => {
                // The final parts are authoritative — flush them into entries
                // and drop the live buffer wholesale (same content, no visible
                // flicker).
                for (_, part) in parts {
                    match part {
                        Part::Thinking(t) => {
                            if !t.is_empty() {
                                self.push_kind(PiEntryKind::Thinking(t));
                            }
                        }
                        Part::Text(t) => {
                            if !t.is_empty() {
                                self.push_kind(PiEntryKind::Assistant(AssistantBody::new(t)));
                            }
                        }
                        Part::ToolCall { id, name } => self.ensure_tool(&id, &name),
                    }
                }
                self.streaming = StreamingMsg::default();
                true
            }
            Event::ToolCallStart { id, name } => {
                self.ensure_tool(&id, &name);
                true
            }
            Event::ToolPartial { id, name, output } => {
                self.set_tool(&id, &name, output, false, false);
                true
            }
            Event::ToolResult { id, name, output, is_error } => {
                self.set_tool(&id, &name, output, true, is_error);
                true
            }
            Event::Queue { follow_up, .. } => {
                self.queued_followups = Some(follow_up.len());
                false
            }
            Event::Streaming(v) => {
                let was_streaming = self.is_streaming();
                if let PiStatus::Ready { streaming, .. } = &mut self.status {
                    *streaming = v;
                }
                // A run just finished — refresh the token/context accounting.
                if was_streaming && !v {
                    self.send_command(pi::Command::FetchStats);
                }
                false
            }
            Event::StreamError(msg) => {
                if !matches!(self.entries.last().map(|e| &e.kind), Some(PiEntryKind::Notice(n)) if *n == msg) {
                    self.push_kind(PiEntryKind::Notice(msg));
                    return true;
                }
                false
            }
            Event::SendFailed { message } => {
                // Remove the optimistic bubble of the most recent send and
                // surface the rejection.
                if let Some(echo) = self.pending_echoes.pop_back() {
                    if let Some(pos) = self
                        .entries
                        .iter()
                        .rposition(|e| matches!(&e.kind, PiEntryKind::User(t) if *t == echo))
                    {
                        self.entries.remove(pos);
                    }
                }
                self.push_kind(PiEntryKind::Notice(format!("发送失败：{message}")));
                true
            }
        }
    }

    /// Send the composer text: optimistic user bubble + worker command.
    /// Returns whether the transcript changed (bubble pushed).
    pub fn send_current(&mut self) -> bool {
        let message = self.input.text().trim().to_string();
        let image = self.pending_image.take();
        if message.is_empty() && image.is_none() {
            return false;
        }
        let session = self.active.clone().unwrap_or_default();
        let queued = self.is_streaming();
        self.pending_echoes.push_back(message.clone());
        self.push_kind(PiEntryKind::User(message.clone()));
        self.pending_image_size = None;
        self.input = text_editor::Content::new();
        self.send_command(pi::Command::Send { session, message, image, queued });
        true
    }

    /// Switch the followed session (clears the view; backfill refills it).
    pub fn watch_session(&mut self, id: String) {
        self.active = Some(id.clone());
        self.entries.clear();
        self.streaming = StreamingMsg::default();
        self.pending_echoes.clear();
        self.queued_followups = None;
        self.status = PiStatus::Connecting;
        self.send_command(pi::Command::Watch(id));
    }

    /// Append a host-side notice entry (capture failures, …).
    pub fn push_notice(&mut self, message: String) {
        self.push_kind(PiEntryKind::Notice(message));
    }

    /// Recompute the completion popup from the composer text (called after
    /// every editor action). `/` at line start → commands; a trailing `@token`
    /// → file references.
    pub fn refresh_popup(&mut self) {
        let text = self.input.text();
        let last_line = text.rsplit('\n').next().unwrap_or("");
        // Slash command: the line starts with `/` and has no space yet.
        if let Some(query) = last_line.strip_prefix('/') {
            if !query.contains(' ') && !query.contains('\t') {
                let query = query.to_string();
                let needle = query.to_ascii_lowercase();
                let mut scored: Vec<(i32, usize)> = self
                    .commands
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| {
                        fuzzy_score(&needle, &c.name.to_ascii_lowercase()).map(|s| (s, i))
                    })
                    .collect();
                scored.sort_by_key(|(s, i)| (-*s, *i));
                let matches: Vec<usize> = scored.into_iter().take(30).map(|(_, i)| i).collect();
                self.popup = Some(Popup::Slash { query, matches, active: 0 });
                return;
            }
        }
        // `@file` reference: the last whitespace-separated token is `@query`.
        if let Some(token) = last_line.split_whitespace().last() {
            if let Some(query) = token.strip_prefix('@') {
                if !query.contains('"') && query.len() < 200 {
                    let cwd = self.active_cwd();
                    // Lazily pull the file index for the session cwd.
                    if let Some(cwd) = cwd.clone() {
                        let have_index = self.files.as_ref().is_some_and(|(c, _)| *c == cwd);
                        if !have_index && self.files_requested.as_deref() != Some(cwd.as_str()) {
                            self.files_requested = Some(cwd.clone());
                            self.send_command(pi::Command::FetchFiles { cwd });
                        }
                    }
                    let query = query.to_string();
                    let needle = query.to_ascii_lowercase();
                    let mut matches: Vec<String> = self
                        .files
                        .as_ref()
                        .map(|(_, files)| {
                            let mut scored: Vec<(i32, &String)> = files
                                .iter()
                                .filter_map(|f| {
                                    fuzzy_score(&needle, &f.to_ascii_lowercase())
                                        .map(|s| (s, f))
                                })
                                .collect();
                            scored.sort_by_key(|(s, f)| (-*s, (*f).clone()));
                            scored.into_iter().take(30).map(|(_, f)| f.clone()).collect()
                        })
                        .unwrap_or_default();
                    matches.truncate(30);
                    self.popup = Some(Popup::At { query, matches, active: 0 });
                    return;
                }
            }
        }
        self.popup = None;
    }

    /// Move the popup highlight (`MenuUp`/`MenuDown`).
    pub fn menu_move(&mut self, delta: isize) {
        if let Some(popup) = &mut self.popup {
            popup.move_active(delta);
        }
    }

    /// Accept the highlighted popup item: `/name ` into the composer, or
    /// `@path ` inserted at the cursor.
    pub fn menu_accept(&mut self) {
        let Some(popup) = self.popup.take() else { return };
        match popup {
            Popup::Slash { matches, active, .. } => {
                let Some(idx) = matches.get(active).and_then(|i| self.commands.get(*i)) else {
                    return;
                };
                let text = format!("/{} ", idx.name);
                self.input = text_editor::Content::with_text(&text);
            }
            Popup::At { matches, active, .. } => {
                let Some(path) = matches.get(active) else { return };
                self.input
                    .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                        std::sync::Arc::new(format!("{path} ")),
                    )));
                self.refresh_popup();
            }
        }
    }

    /// Dismiss the popup without inserting anything.
    pub fn menu_close(&mut self) {
        self.popup = None;
    }

    /// The followed session's working directory, if known.
    pub fn active_cwd(&self) -> Option<String> {
        let id = self.active.as_ref()?;
        self.sessions
            .iter()
            .find(|s| &s.id == id)
            .and_then(|s| s.cwd.clone())
    }

    /// Open the new-session browser at the active session's directory.
    pub fn open_browse(&mut self) {
        let start = self
            .active_cwd()
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| "/".to_string());
        self.browse = Some(BrowseState {
            path: start.clone(),
            parent: None,
            dirs: Vec::new(),
            loading: true,
        });
        self.send_command(pi::Command::Browse { path: start });
    }

    /// Browse into `path` (or the parent when `None`).
    pub fn browse_to(&mut self, path: Option<String>) {
        let Some(browse) = &mut self.browse else { return };
        let target = match path {
            Some(p) => p,
            None => match browse.parent.clone() {
                Some(p) => p,
                None => return,
            },
        };
        browse.path = target.clone();
        browse.dirs.clear();
        browse.loading = true;
        self.send_command(pi::Command::Browse { path: target });
    }

    /// Create a session in the browsed directory and close the browser.
    pub fn create_session(&mut self) {
        let Some(browse) = self.browse.take() else { return };
        self.status = PiStatus::Connecting;
        self.send_command(pi::Command::NewSession { cwd: browse.path });
    }

    /// Toggle one entry's expanded flag (entries first, then live blocks).
    pub fn toggle_entry(&mut self, id: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.expanded = !e.expanded;
        } else if !self.live_expanded.remove(&id) {
            self.live_expanded.insert(id);
        }
    }

    /// Push a new live entry with a fresh id; returns the id.
    fn push_kind(&mut self, kind: PiEntryKind) -> u64 {
        let id = self.next_live_id();
        self.entries.push(PiEntry { id, kind, expanded: false });
        id
    }

    fn next_live_id(&mut self) -> u64 {
        self.next_id += 1;
        LIVE_ID_BASE + self.next_id
    }

    /// Find-or-create the Tool entry for a call id (content-addressed so a
    /// reconnect's backfill keeps identity and expand state stable).
    fn ensure_tool(&mut self, id: &str, name: &str) {
        let exists = self
            .entries
            .iter()
            .any(|e| matches!(&e.kind, PiEntryKind::Tool { id: tid, .. } if tid == id));
        if !exists {
            self.entries.push(PiEntry {
                id: stable_id("tool", id, 0),
                kind: PiEntryKind::Tool {
                    id: id.to_string(),
                    name: name.to_string(),
                    output: String::new(),
                    done: false,
                    is_error: false,
                },
                expanded: false,
            });
        }
    }

    /// Update a Tool entry's output by call id (find-or-create).
    fn set_tool(&mut self, id: &str, name: &str, output: String, done: bool, is_error: bool) {
        let pos = self
            .entries
            .iter()
            .rposition(|e| matches!(&e.kind, PiEntryKind::Tool { id: tid, .. } if tid == id));
        if let Some(pos) = pos {
            if let PiEntryKind::Tool { name: n, output: o, done: d, is_error: e, .. } =
                &mut self.entries[pos].kind
            {
                *n = name.to_string();
                *o = output;
                *d = done;
                *e = is_error;
            }
        } else {
            self.entries.push(PiEntry {
                id: stable_id("tool", id, 0),
                kind: PiEntryKind::Tool {
                    id: id.to_string(),
                    name: name.to_string(),
                    output,
                    done,
                    is_error,
                },
                expanded: false,
            });
        }
    }

    /// Stable id for a streaming block (expand state keyed by kind+index).
    fn live_block_id(idx: usize, tag: &str) -> u64 {
        stable_id(tag, &format!("live-{idx}"), 0)
    }
}

// ── Stable ids (backfill) ───────────────────────────────────────────────────

/// Subsequence fuzzy match: returns a score (higher = better) or `None`.
/// Empty needle matches everything with a neutral score.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut score = 0i32;
    let mut hay = haystack.chars().enumerate();
    let mut last: Option<usize> = None;
    for want in needle.chars() {
        let mut found = None;
        for (i, c) in hay.by_ref() {
            if c == want {
                found = Some(i);
                break;
            }
        }
        let idx = found?;
        // Consecutive matches and earlier matches score higher.
        score += match last {
            Some(prev) if idx == prev + 1 => 8,
            _ => 4,
        };
        score -= idx as i32 / 8;
        last = Some(idx);
    }
    // Prefer shorter haystacks on ties.
    score -= haystack.chars().count() as i32 / 32;
    Some(score)
}

/// FNV-1a — cheap deterministic hash for entry ids.
fn fnv1a(data: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Deterministic id from a kind tag + content key + occurrence index, so a
/// reconnect's backfill produces the same ids the live session used.
fn stable_id(tag: &str, key: &str, occurrence: usize) -> u64 {
    fnv1a(&format!("{tag}\u{1}{key}\u{1}{occurrence}"))
}

/// Map a backfilled [`Entry`] to a renderable [`PiEntry`] with a stable id.
fn stable_entry(entry: Entry) -> PiEntry {
    let (tag, key, kind) = match entry {
        Entry::User(t) => ("u", t.clone(), PiEntryKind::User(t)),
        Entry::Assistant(t) => {
            let body = AssistantBody::new(t.clone());
            ("a", t, PiEntryKind::Assistant(body))
        }
        Entry::Thinking(t) => ("t", t.clone(), PiEntryKind::Thinking(t)),
        Entry::Tool { id, name, output, done, is_error } => {
            ("tool", id.clone(), PiEntryKind::Tool { id, name, output, done, is_error })
        }
        Entry::Notice(n) => ("n", n.clone(), PiEntryKind::Notice(n)),
    };
    PiEntry { id: stable_id(tag, &key, 0), kind, expanded: false }
}

// ── Panel chrome (header identical to the Properties panel) ─────────────────

fn header(auto_collapse: bool) -> Element<'static, Message> {
    use crate::ui::dock::{DockMsg, PanelId};
    let pin_icon = if auto_collapse {
        crate::ui::icons::themed_primary_weak_text(crate::ui::icons::PIN, 12.0)
    } else {
        crate::ui::icons::themed_secondary(crate::ui::icons::PIN, 12.0)
    };
    let pin = button(pin_icon)
        .on_press(Message::Dock(DockMsg::AutoCollapseToggle(PanelId::Pi)))
        .style(move |theme: &Theme, status| {
            let mut style = button::subtle(theme, status);
            if auto_collapse {
                let palette = theme.palette();
                style.background = Some(Background::Color(palette.primary.weak.color));
                style.text_color = palette.primary.weak.text;
                style.border.color = palette.primary.base.color;
                style.border.width = 1.0;
            }
            style
        })
        .padding([3, 5]);
    let pin = tooltip(pin, tooltip_label("Auto"), tooltip::Position::Bottom)
        .gap(4)
        .style(tooltip_style);

    let close = button(crate::ui::icons::themed_secondary(
        crate::ui::icons::CLOSE,
        12.0,
    ))
    .on_press(Message::Dock(DockMsg::Close(PanelId::Pi)))
    .style(button::subtle)
    .padding([3, 5]);
    let close = tooltip(close, tooltip_label("Close"), tooltip::Position::Bottom)
        .gap(4)
        .style(tooltip_style);

    mouse_area(
        container(
            row![
                text("Pi 助手").size(12),
                Space::new().width(Length::Fill),
                pin,
                close,
            ]
            .spacing(3)
            .align_y(iced::Center),
        )
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(theme.palette().background.weak.color)),
            ..Default::default()
        })
        .width(Length::Fill)
        .padding([3, 6]),
    )
    .on_press(Message::Dock(DockMsg::DockGrab(PanelId::Pi)))
    .interaction(iced::mouse::Interaction::Grab)
    .into()
}

// ── Text helpers ────────────────────────────────────────────────────────────

fn secondary_color(theme: &Theme) -> Color {
    theme.palette().secondary.base.text
}

fn warning_color(theme: &Theme) -> Color {
    theme.palette().warning.base.text
}

/// Tooltip surface matching the panel chrome. The stock tooltip is a light box
/// with inherited (white) text — jarring on the dark panel.
fn tooltip_style(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.palette().background.weak.color)),
        border: Border {
            color: theme.palette().background.strong.color,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Tooltip label in the panel's muted-but-readable grey.
fn tooltip_label<'a>(label: impl Into<String>) -> Element<'a, Message> {
    text(label.into())
        .size(10)
        .style(|_: &Theme| iced::widget::text::Style {
            color: Some(MUTED_TEXT),
        })
        .into()
}

/// Small secondary-colored label above an entry.
fn tag_label(label: &str) -> iced::widget::Text<'_, Theme> {
    text(label.to_string())
        .size(10)
        .style(|theme: &Theme| iced::widget::text::Style {
            color: Some(secondary_color(theme)),
        })
}

/// The collapsible one-line header used by thinking / tool entries.
fn toggle_row(
    title: String,
    badge: Option<Badge>,
    expanded: bool,
    id: u64,
) -> Element<'static, Message> {
    let mut head = row![
        text(if expanded { "▾" } else { "▸" }).size(10),
        text(title).size(10),
    ]
    .spacing(4)
    .align_y(iced::Center);
    if let Some(badge) = badge {
        head = head.push(
            text(badge.label())
                .size(10)
                .style(move |theme: &Theme| iced::widget::text::Style {
                    color: Some(badge.color(theme)),
                }),
        );
    }
    // The whole row width is the hit target (fill = 收起块填满整行).
    button(head)
        .width(Length::Fill)
        .on_press(Message::Pi(PiMsg::ToggleEntry(id)))
        .style(|theme: &Theme, status| button::subtle(theme, status))
        .padding([2, 4])
        .into()
}

// ── Rows above the transcript ─────────────────────────────────────────────

/// Session switcher: follow-newest by default, user can pin another one.
fn session_picker(state: &PiPanelState) -> Element<'_, Message> {
    let selected = state
        .active
        .as_ref()
        .and_then(|id| state.sessions.iter().find(|s| &s.id == id));
    let label = |s: &pi::SessionInfo| {
        if s.label.is_empty() {
            "（无标题会话）".to_string()
        } else {
            s.label.clone()
        }
    };
    let picker = pick_list(selected, state.sessions.as_slice(), label)
        .placeholder("选择会话…")
        .width(Length::Fill)
        .text_size(11)
        .padding([2, 6])
        .menu_height(240.0)
        .on_select(|s: pi::SessionInfo| Message::Pi(PiMsg::SessionPick(s.id.clone())));
    let new_session = button(text("＋").size(11))
        .on_press(Message::Pi(PiMsg::NewSessionOpen))
        .style(|theme: &Theme, status| button::subtle(theme, status))
        .padding([2, 7]);
    container(row![picker.width(Length::Fill), new_session].spacing(4))
        .width(Length::Fill)
        .padding([4, 6])
        .into()
}

/// Status strip: connection label, optional reconnect button.
fn status_strip(state: &PiPanelState) -> Element<'_, Message> {
    let errored = matches!(state.status, PiStatus::Error(_));
    let idle = matches!(state.status, PiStatus::Idle);
    let backend_tag: Element<'_, Message> = if state.backend.is_empty() {
        column![].into()
    } else {
        text(state.backend.clone())
            .size(9)
            .style(|theme: &Theme| iced::widget::text::Style {
                color: Some(theme.palette().primary.base.color),
            })
            .into()
    };
    // Backend switch, right where the connected backend is shown. `auto` probes
    // pi-web and falls back to a local `pi --mode rpc` child; `rpc` never needs
    // pi-web at all.
    let backend_options: Vec<String> = crate::pi::BACKEND_MODES
        .iter()
        .map(|m| (*m).to_string())
        .collect();
    let selected_backend = backend_options.iter().find(|m| **m == state.mode).cloned();
    let backend_pick = tooltip(
        pick_list(selected_backend, backend_options, |m: &String| m.clone())
            .width(Length::Fixed(62.0))
            .text_size(9)
            .padding([1, 5])
            .menu_height(120.0)
            .on_select(|m: String| Message::Pi(PiMsg::BackendPick(m))),
        column![
            tooltip_label("auto：优先 pi-web，没有就用本机 pi"),
            tooltip_label("rpc：只跑本机 pi，不需要 pi-web"),
        ]
        .spacing(2),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .style(tooltip_style);
    let mut strip = row![
        backend_tag,
        backend_pick,
        text(state.status_label())
            .size(10)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(MUTED_TEXT),
            })
            .width(Length::Fill),
    ]
    .spacing(4)
    .align_y(iced::Center);
    if errored || idle {
        let reconnect = button(text(if idle { "连接" } else { "重连" }).size(10))
            .on_press(Message::Pi(PiMsg::Reconnect))
            .style(|theme: &Theme, status| button::subtle(theme, status))
            .padding([2, 6]);
        strip = strip.push(reconnect);
    }
    container(strip).width(Length::Fill).padding([2, 8]).into()
}

// ── Transcript ────────────────────────────────────────────────────────

fn entry_view<'a>(e: &'a PiEntry, theme: &'a Theme) -> Element<'a, Message> {
    match &e.kind {
        PiEntryKind::User(t) => {
            // Image-only sends carry no text; show a camera placeholder.
            let body = if t.is_empty() {
                "📷 截图".to_string()
            } else {
                t.clone()
            };
            container(
                column![tag_label("你"), text(body).size(12)]
                    .spacing(2)
                    .width(Length::Fill),
            )
            .style(|theme: &Theme| container::Style {
                background: Some(Background::Color(theme.palette().primary.weak.color)),
                border: Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .width(Length::Fill)
            .padding([6, 8])
            .into()
        }
        PiEntryKind::Assistant(body) => column![
            tag_label("Pi"),
            assistant_markdown(body, theme),
        ]
        .spacing(2)
        .padding([2, 4])
        .width(Length::Fill)
        .into(),
        PiEntryKind::Thinking(t) => {
            let head = toggle_row("思考".to_string(), None, e.expanded, e.id);
            let mut body = column![head].spacing(4).padding([2, 4]).width(Length::Fill);
            if e.expanded && !t.is_empty() {
                body = body.push(
                    text(t.clone())
                        .size(11)
                        .style(|_: &Theme| iced::widget::text::Style {
                            color: Some(MUTED_TEXT),
                        })
                        .width(Length::Fill),
                );
            }
            body.into()
        }
        PiEntryKind::Tool { name, output, done, is_error, .. } => {
            let badge = if !*done {
                Badge::Running
            } else if *is_error {
                Badge::Failed
            } else {
                Badge::Done
            };
            let head =
                toggle_row(format!("工具 · {name}"), Some(badge), e.expanded, e.id);
            let mut body = column![head].spacing(4).padding([2, 4]).width(Length::Fill);
            if e.expanded && !output.is_empty() {
                body = body.push(
                    text(output.clone())
                        .size(11)
                        .font(Font::MONOSPACE)
                        .width(Length::Fill),
                );
            }
            body.into()
        }
        PiEntryKind::Notice(n) => container(
            text(n.clone())
                .size(11)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(warning_color(theme)),
                }),
        )
        .width(Length::Fill)
        .padding([2, 4])
        .into(),
    }
}

/// Minimum column width for rendered tables — wider than the dock panel, so
/// the table scrolls horizontally instead of squeezing cells to one character.
const TABLE_COLUMN_MIN_WIDTH: f32 = 150.0;

/// Render one pipe table with fixed-width columns inside a horizontal
/// scrollable (iced's built-in markdown table squeezes in narrow panels).
fn table_view(rows: &[Vec<String>]) -> Element<'_, Message> {
    use iced::widget::{scrollable, table};
    let columns: &[String] = rows.first().map(|header| header.as_slice()).unwrap_or(&[]);
    let grid = table(
        columns.iter().enumerate().map(|(index, header)| {
            table::column(text(header.clone()).size(11), move |row: &Vec<String>| {
                text(row.get(index).cloned().unwrap_or_default()).size(11)
            })
            .width(Length::Fixed(TABLE_COLUMN_MIN_WIDTH))
        }),
        rows.iter().skip(1),
    )
    .padding_x(4.0)
    .padding_y(2.0)
    .separator_x(0);
    // Bottom padding reserves room for the floating horizontal scrollbar so it
    // doesn't cover the last row.
    scrollable(
        container(grid).padding(iced::Padding {
            top: 0.0,
            right: 0.0,
            bottom: 12.0,
            left: 0.0,
        }),
    )
    .direction(scrollable::Direction::Horizontal(
        scrollable::Scrollbar::default(),
    ))
    .into()
}

/// Render an assistant message: the stock markdown viewer, except when the
/// message contains pipe tables — those are rendered separately so they can
/// scroll horizontally instead of collapsing to one character per line.
fn assistant_markdown<'a>(body: &'a AssistantBody, theme: &'a Theme) -> Element<'a, Message> {
    let settings = iced::widget::markdown::Settings::with_text_size(12, theme);
    let Some(segments) = &body.table_split else {
        return iced::widget::markdown::view(body.md.items(), settings).map(Message::OpenUrl);
    };
    let mut column_children = column![].spacing(2).width(Length::Fill);
    for segment in segments {
        match segment {
            MdSegment::Markdown { md, .. } => {
                column_children = column_children
                    .push(iced::widget::markdown::view(md.items(), settings).map(Message::OpenUrl));
            }
            MdSegment::Table(rows) => {
                column_children = column_children.push(table_view(rows));
            }
        }
    }
    column_children.into()
}

/// The live assistant message (thinking blocks + growing text).
fn streaming_view<'a>(state: &'a PiPanelState, theme: &'a Theme) -> Option<Element<'a, Message>> {
    if state.streaming.blocks.is_empty() {
        return None;
    }
    let mut col = column![].spacing(2).width(Length::Fill);
    for block in &state.streaming.blocks {
        match block {
            StreamBlock::Thinking { idx, text: t } => {
                // Live thinking stays a collapsed summary unless toggled; the
                // toggle id is stable across deltas so expand state holds.
                let id = PiPanelState::live_block_id(*idx, "t");
                let expanded = state.live_expanded.contains(&id);
                let head = toggle_row("思考中…".to_string(), None, expanded, id);
                let mut body = column![head].spacing(4).padding([2, 4]).width(Length::Fill);
                if expanded && !t.is_empty() {
                    body = body.push(
                        text(t.clone())
                            .size(11)
                            .style(|_: &Theme| iced::widget::text::Style {
                                color: Some(MUTED_TEXT),
                            })
                            .width(Length::Fill),
                    );
                }
                col = col.push(body);
            }
            StreamBlock::Text { text: t, md, .. } => {
                let body: Element<'_, Message> = if t.is_empty() && md.items().is_empty() {
                    text("…")
                        .size(12)
                        .style(|theme: &Theme| iced::widget::text::Style {
                            color: Some(secondary_color(theme)),
                        })
                        .into()
                } else {
                    // Streaming uses the fast path; the finalized entry gets
                    // the table-aware rendering.
                    iced::widget::markdown::view(
                        md.items(),
                        iced::widget::markdown::Settings::with_text_size(12, theme),
                    )
                    .map(Message::OpenUrl)
                };
                col = col.push(
                    column![tag_label("Pi"), body]
                        .spacing(2)
                        .padding([2, 4])
                        .width(Length::Fill),
                );
            }
        }
    }
    Some(col.into())
}

/// Empty-state hint shown when the transcript has nothing to render.
fn empty_state(state: &PiPanelState) -> Element<'static, Message> {
    let hint: String = match &state.status {
        PiStatus::Error(_) => {
            // Both backends are mentioned: with `OCS_PI_MODE=rpc` no pi-web is
            // needed at all (pi is spawned directly).
            "未连接。\n· pi-web 后端：先启动 pi-web（默认 http://127.0.0.1:30141）\n· rpc 后端：在状态条把后端切到 rpc（只需本机 pi，无需 pi-web）\n端点可用 OCS_PI_ENDPOINT 覆盖，模式可用 OCS_PI_MODE=auto|web|rpc 覆盖。".to_string()
        }
        PiStatus::Ready { .. } if state.sessions.is_empty() => {
            "pi-web 没有会话。\n在网页端新建一个会话后，本面板会自动跟随。".to_string()
        }
        PiStatus::Ready { .. } => "暂无消息 — 在下方输入框发送第一条".to_string(),
        PiStatus::Connecting => "正在连接…".to_string(),
        PiStatus::Idle => "未连接 — 点击上方「连接」开始".to_string(),
    };
    container(
        column![
            text("Pi 助手").size(13),
            // Full-contrast body text: the empty-state hint carries setup
            // instructions the user must read, so a dim secondary gray is
            // not enough (视觉验收曾点名对比度不足).
            text(hint)
                .size(11)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(theme.palette().background.base.text),
                }),
        ]
        .spacing(8),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .padding(16)
    .into()
}

// ── Rows below the transcript ───────────────────────────────────────────────

/// Composer: multiline editor + send row. Enter sends, Shift+Enter breaks the
/// line (see the `key_binding` interception below).
fn composer<'a>(
    state: &'a PiPanelState,
    selection_label: &'a str,
) -> Element<'a, Message> {
    let can_send =
        !state.input.text().trim().is_empty() || state.pending_image.is_some();
    let menu_open = state.popup.as_ref().map(|p| p.len() > 0).unwrap_or(false);

    // Attached-screenshot chip (click × to drop it before sending).
    let attachment: Option<Element<'_, Message>> = state.pending_image.as_ref().map(|img| {
        let dims = state
            .pending_image_size
            .map(|(w, h)| format!(" {}×{}", w, h))
            .unwrap_or_default();
        let chip = row![
            text(format!("📷 截图（{} KB）{}", img.base64.len() * 3 / 4 / 1024, dims)).size(10),
            Space::new().width(Length::Fill),
            button(text("✕").size(10))
                .on_press(Message::Pi(PiMsg::ClearImage))
                .style(|theme: &Theme, status| button::subtle(theme, status))
                .padding([1, 5]),
        ]
        .align_y(iced::Center);
        container(chip)
            .style(|theme: &Theme| container::Style {
                background: Some(Background::Color(theme.palette().primary.weak.color)),
                border: Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .width(Length::Fill)
            .padding([3, 6])
            .into()
    });
    let chip_column: Element<'_, Message> = match attachment {
        Some(chip) => column![chip].spacing(4).into(),
        None => column![].into(),
    };

    // Canvas selection summary (same detection the Properties panel uses).
    let selection_row: Element<'_, Message> = if selection_label.is_empty() {
        column![].into()
    } else {
        container(
            row![
                text("◉").size(10).style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(theme.palette().primary.base.color),
                }),
                text(selection_label.to_string()).size(10),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .width(Length::Fill)
        .padding([2, 4])
        .into()
    };

    let editor = text_editor(&state.input)
        .placeholder("向 Pi 发送…（Enter 发送 / Shift+Enter 换行）")
        .size(12)
        .height(Length::Fixed(COMPOSER_H))
        .padding(4)
        .key_binding(move |kp| {
            use iced::keyboard::{key::Named, Key};
            use iced::widget::text_editor::{Binding, Status};
            let focused = matches!(kp.status, Status::Focused { .. });
            let plain_enter = matches!(kp.key, Key::Named(Named::Enter)) && !kp.modifiers.shift();
            let paste = kp.key.to_latin(kp.physical_key) == Some('v')
                && kp.modifiers.command()
                && !kp.modifiers.alt();
            if focused && menu_open {
                // The completion popup owns the navigation keys while open.
                if matches!(kp.key, Key::Named(Named::ArrowUp)) {
                    return Some(Binding::Custom(Message::Pi(PiMsg::MenuUp)));
                }
                if matches!(kp.key, Key::Named(Named::ArrowDown)) {
                    return Some(Binding::Custom(Message::Pi(PiMsg::MenuDown)));
                }
                if plain_enter || matches!(kp.key, Key::Named(Named::Tab)) {
                    return Some(Binding::Custom(Message::Pi(PiMsg::MenuAccept)));
                }
                if matches!(kp.key, Key::Named(Named::Escape)) {
                    return Some(Binding::Custom(Message::Pi(PiMsg::MenuClose)));
                }
            }
            if focused && plain_enter {
                Some(Binding::Custom(Message::Pi(PiMsg::Send)))
            } else if focused && paste {
                // Image clipboard first; text falls back in `on_pi_paste`.
                Some(Binding::Custom(Message::Pi(PiMsg::Paste)))
            } else {
                Binding::from_key_press(kp)
            }
        })
        .on_action(|a| Message::Pi(PiMsg::Editor(a)));

    let send = button(text("发送").size(11))
        .on_press_maybe(can_send.then(|| Message::Pi(PiMsg::Send)))
        .style(move |theme: &Theme, status| {
            let mut style = button::subtle(theme, status);
            if can_send {
                let palette = theme.palette();
                style.background = Some(Background::Color(palette.primary.weak.color));
                style.text_color = palette.primary.weak.text;
            }
            style
        })
        .padding([4, 10]);

    let mut hint_text = if state.is_streaming() {
        "Pi 生成中…Enter 会排队为后续消息".to_string()
    } else {
        "Enter 发送 / Shift+Enter 换行".to_string()
    };
    if let Some(n) = state.queued_followups {
        if n > 0 {
            hint_text = format!("已排队 {n} 条 · {hint_text}");
        }
    }
    let hint = text(hint_text)
        .size(10)
        .style(|_: &Theme| iced::widget::text::Style {
            color: Some(MUTED_TEXT),
        });

    // Model switcher + thinking level below the input box.
    let model_row: Element<'_, Message> = if state.models.is_empty() {
        column![].into()
    } else {
        let selected = state.current_model.as_ref().and_then(|(provider, id)| {
            state
                .models
                .iter()
                .find(|m| &m.provider == provider && &m.id == id)
        });
        let picker = pick_list(
            selected,
            state.models.as_slice(),
            move |m: &pi::ModelInfo| m.name.clone(),
        )
        .placeholder("模型…")
        .width(Length::Fill)
        .text_size(11)
        .padding([2, 6])
        .menu_height(220.0)
        .on_select(|m: pi::ModelInfo| Message::Pi(PiMsg::ModelPick(m)));
        // Thinking levels supported by the current model (`off`… `max`).
        let levels: Vec<String> = state
            .current_model
            .as_ref()
            .and_then(|(provider, id)| {
                let key = format!("{provider}:{id}");
                state
                    .thinking_levels
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone())
            })
            .unwrap_or_default();
        let mut row = row![picker.width(Length::Fill)].spacing(4);
        if !levels.is_empty() {
            let selected = state
                .current_thinking
                .as_ref()
                .and_then(|c| levels.iter().find(|l| *l == c).cloned());
            row = row.push(
                pick_list(selected, levels, move |l: &String| format!("思考:{l}"))
                    .placeholder("思考…")
                    .width(Length::Fixed(96.0))
                    .text_size(11)
                    .padding([2, 6])
                    .menu_height(180.0)
                    .on_select(|l: String| Message::Pi(PiMsg::ThinkingPick(l))),
            );
        }
        // Manual context compaction (disabled while a run is streaming).
        let compact = button(text("压缩").size(10))
            .on_press_maybe(
                (!state.is_streaming()).then_some(Message::Pi(PiMsg::Compact)),
            )
            .style(|theme: &Theme, status| button::subtle(theme, status))
            .padding([2, 7]);
        row = row.push(compact);
        container(row).width(Length::Fill).padding([2, 2]).into()
    };

    // Token/cost/context accounting for the session.
    let stats_row: Element<'_, Message> = match &state.stats {
        Some(stats) if stats.input > 0 || stats.context_tokens.is_some() => {
            let short = |n: u64| -> String {
                if n >= 1_000_000 {
                    format!("{:.1}M", n as f64 / 1_000_000.0)
                } else if n >= 1_000 {
                    format!("{:.1}k", n as f64 / 1_000.0)
                } else {
                    n.to_string()
                }
            };
            let mut line = format!(
                "↑{} ↓{} · 缓存 {}/{}",
                short(stats.input),
                short(stats.output),
                short(stats.cache_read),
                short(stats.cache_write),
            );
            if let (Some(tokens), Some(window)) = (stats.context_tokens, stats.context_window) {
                // pi-web may report `percent: null`（压缩后）；兜底自算。
                let percent = stats
                    .context_percent
                    .map(u64::from)
                    .or_else(|| (window > 0).then(|| tokens * 100 / window))
                    .map(|p| format!("{p}%"))
                    .unwrap_or_default();
                line = format!(
                    "{line} · 上下文 {percent}（{}/{}）",
                    short(tokens),
                    short(window)
                );
            }
            if let Some(cost) = stats.cost {
                line = format!("{line} · ${cost:.3}");
            }
            container(
                text(line)
                    .size(9)
                    .style(|_: &Theme| iced::widget::text::Style {
                        color: Some(MUTED_TEXT),
                    })
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .padding([1, 4])
            .into()
        }
        _ => column![].into(),
    };

    let popup_row: Element<'_, Message> = popup_view(state);
    let browse_row: Element<'_, Message> = match &state.browse {
        Some(browse) => browse_view(browse),
        None => column![].into(),
    };
    let ui_request_row: Element<'_, Message> = match &state.pending_ui {
        Some(request) => ui_request_view(request, &state.ui_answer),
        None => column![].into(),
    };

    container(
        column![
            selection_row,
            ui_request_row,
            browse_row,
            chip_column,
            popup_row,
            editor,
            row![hint, Space::new().width(Length::Fill), send]
                .spacing(6)
                .align_y(iced::Center),
            model_row,
            stats_row,
        ]
        .spacing(4),
    )
    .style(move |theme: &Theme| container::Style {
        border: Border {
            color: theme.palette().background.strong.color,
            width: 1.0,
            ..Default::default()
        },
        background: Some(Background::Color(theme.palette().background.weak.color)),
        ..Default::default()
    })
    .width(Length::Fill)
    .padding([6, 8])
    .into()
}

/// Pending approval dialog (extension UI request) above the editor.
fn ui_request_view<'a>(
    request: &'a PendingUi,
    answer: &'a text_editor::Content,
) -> Element<'a, Message> {
    let mut body = column![
        text(request.title.clone()).size(11),
    ]
    .spacing(4)
    .width(Length::Fill);
    if let Some(message) = &request.message {
        body = body.push(
            text(message.clone())
                .size(10)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(secondary_color(theme)),
                }),
        );
    }
    // `input` / `editor` requests are answered with free text.
    if request.method == "input" || request.method == "editor" {
        body = body.push(
            text_editor(answer)
                .size(11)
                .height(Length::Fixed(if request.method == "editor" { 84.0 } else { 34.0 }))
                .padding(3)
                .on_action(|a| Message::Pi(PiMsg::UiEdit(a))),
        );
    }
    let mut actions = row![].spacing(4).align_y(iced::Center);
    match request.method.as_str() {
        "select" => {
            // Options go **one per line, full width**. An `ask_question` option is a
            // label *plus* a description (easily 80+ chars), so a horizontal row of
            // buttons got squeezed into a one-character-wide column each and wrapped
            // vertically — a 300px-tall wall of text with one option legible.
            let mut options = column![].spacing(3).width(Length::Fill);
            for option in request.options.iter().take(8) {
                options = options.push(
                    button(text(option.clone()).size(10).width(Length::Fill))
                        .on_press(Message::Pi(PiMsg::UiAnswer {
                            value: Some(option.clone()),
                            confirmed: None,
                        }))
                        .style(|theme: &Theme, status| button::subtle(theme, status))
                        .width(Length::Fill)
                        .padding([3, 8]),
                );
            }
            // A long list must not push the transcript out of the panel: cap it and
            // scroll (the extra right padding keeps the floating scrollbar off the
            // wrapped text).
            let options: Element<'a, Message> = if request.options.len() > 3 {
                scrollable(options.padding(iced::Padding {
                    right: 12.0,
                    ..Default::default()
                }))
                .height(Length::Fixed(150.0))
                .into()
            } else {
                options.into()
            };
            body = body.push(options);
        }
        "confirm" => {
            actions = actions.push(
                button(text("允许").size(10))
                    .on_press(Message::Pi(PiMsg::UiAnswer {
                        value: None,
                        confirmed: Some(true),
                    }))
                    .style(|theme: &Theme, status| {
                        let mut style = button::subtle(theme, status);
                        style.background =
                            Some(Background::Color(theme.palette().success.weak.color));
                        style.text_color = theme.palette().success.weak.text;
                        style
                    })
                    .padding([3, 10]),
            );
            actions = actions.push(
                button(text("拒绝").size(10))
                    .on_press(Message::Pi(PiMsg::UiAnswer {
                        value: None,
                        confirmed: Some(false),
                    }))
                    .style(|theme: &Theme, status| button::subtle(theme, status))
                    .padding([3, 10]),
            );
        }
        // `input` / `editor`: answer with the free-text editor above.
        _ => {
            actions = actions.push(
                button(text("确定").size(10))
                    .on_press(Message::Pi(PiMsg::UiSubmit))
                    .style(|theme: &Theme, status| {
                        let mut style = button::subtle(theme, status);
                        style.background =
                            Some(Background::Color(theme.palette().success.weak.color));
                        style.text_color = theme.palette().success.weak.text;
                        style
                    })
                    .padding([3, 10]),
            );
        }
    }
    actions = actions.push(Space::new().width(Length::Fill));
    actions = actions.push(
        button(text("取消").size(10))
            .on_press(Message::Pi(PiMsg::UiCancel))
            .style(|theme: &Theme, status| button::subtle(theme, status))
            .padding([3, 8]),
    );
    body = body.push(actions);
    container(body)
        .style(|theme: &Theme| {
            // Match the composer's own text editor instead of the theme's loud
            // `warning.weak` fill (a lavender wall behind the options). The thin
            // warning border keeps the "needs an answer" signal.
            let editor = text_editor::default(theme, text_editor::Status::Active);
            container::Style {
                background: Some(editor.background),
                border: Border {
                    color: theme.palette().warning.base.color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .width(Length::Fill)
        .padding(6)
        .into()
}

/// Completion popup above the editor: slash commands or `@` files.
fn popup_view(state: &PiPanelState) -> Element<'_, Message> {
    let Some(popup) = &state.popup else {
        return column![].into();
    };
    if popup.len() == 0 {
        return column![].into();
    }
    let mut list = column![].spacing(1).width(Length::Fill);
    let active = popup.active();
    for (row_index, (label, hint)) in popup_rows(state, popup).into_iter().enumerate() {
        let is_active = row_index == active;
        let content = row![
            text(label).size(11),
            Space::new().width(8),
            text(hint)
                .size(10)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(secondary_color(theme)),
                })
                .width(Length::Fill),
        ]
        .align_y(iced::Center);
        list = list.push(
            container(content)
                .style(move |theme: &Theme| container::Style {
                    background: is_active
                        .then(|| Background::Color(theme.palette().primary.weak.color)),
                    ..Default::default()
                })
                .width(Length::Fill)
                .padding([2, 6]),
        );
    }
    container(list)
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(theme.palette().background.weakest.color)),
            border: Border {
                color: theme.palette().background.strong.color,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .width(Length::Fill)
        .padding(2)
        .into()
}

/// `(label, hint)` rows for the popup list (capped at 8 visible entries).
fn popup_rows(state: &PiPanelState, popup: &Popup) -> Vec<(String, String)> {
    const MAX_VISIBLE: usize = 8;
    let start = popup.active().saturating_sub(MAX_VISIBLE - 1);
    match popup {
        Popup::Slash { matches, .. } => matches
            .iter()
            .skip(start)
            .take(MAX_VISIBLE)
            .filter_map(|i| state.commands.get(*i))
            .map(|c| {
                let hint = if c.source.is_empty() {
                    c.description.clone()
                } else {
                    format!("[{}] {}", c.source, c.description)
                };
                (format!("/{}", c.name), hint)
            })
            .collect(),
        Popup::At { matches, .. } => matches
            .iter()
            .skip(start)
            .take(MAX_VISIBLE)
            .map(|path| ("@".to_string() + path, String::new()))
            .collect(),
    }
}

/// New-session directory browser row.
fn browse_view(browse: &BrowseState) -> Element<'_, Message> {
    let mut list = column![].spacing(1).width(Length::Fill);
    if browse.loading {
        list = list.push(
            text("读取目录…")
                .size(10)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(secondary_color(theme)),
                }),
        );
    }
    for (name, path) in browse.dirs.iter().take(8) {
        let target = path.clone();
        list = list.push(
            button(text(format!("📁 {name}")).size(11))
                .width(Length::Fill)
                .on_press(Message::Pi(PiMsg::DirOpen(target)))
                .style(|theme: &Theme, status| button::subtle(theme, status))
                .padding([2, 6]),
        );
    }
    if browse.dirs.is_empty() && !browse.loading {
        list = list.push(
            text("（没有子目录）")
                .size(10)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(secondary_color(theme)),
                }),
        );
    }
    let header = row![
        button(text("↑ 上级").size(10))
            .on_press(Message::Pi(PiMsg::DirUp))
            .style(|theme: &Theme, status| button::subtle(theme, status))
            .padding([2, 6]),
        text(browse.path.clone())
            .size(10)
            .style(|theme: &Theme| iced::widget::text::Style {
                color: Some(secondary_color(theme)),
            })
            .width(Length::Fill),
    ]
    .spacing(4)
    .align_y(iced::Center);
    let actions = row![
        button(text("在此新建会话").size(10))
            .on_press(Message::Pi(PiMsg::CreateSession))
            .style(|theme: &Theme, status| {
                let mut style = button::subtle(theme, status);
                style.background = Some(Background::Color(theme.palette().primary.weak.color));
                style.text_color = theme.palette().primary.weak.text;
                style
            })
            .padding([3, 8]),
        Space::new().width(Length::Fill),
        button(text("取消").size(10))
            .on_press(Message::Pi(PiMsg::NewSessionCancel))
            .style(|theme: &Theme, status| button::subtle(theme, status))
            .padding([3, 8]),
    ]
    .align_y(iced::Center);
    container(column![header, list, actions].spacing(4))
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(theme.palette().background.weakest.color)),
            border: Border {
                color: theme.palette().primary.base.color,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .width(Length::Fill)
        .padding(4)
        .into()
}

// ── Panel body ──────────────────────────────────────────────────────────────

/// Panel chrome: title + pin + close, exactly like the Properties panel so the
/// dock drag/resize/collapse affordances all behave identically.
pub fn view<'a>(
    state: &'a PiPanelState,
    width: f32,
    auto_collapse: bool,
    selection_label: &'a str,
    theme: &'a Theme,
) -> Element<'a, Message> {
    let nothing_to_show =
        state.entries.is_empty() && state.streaming.blocks.is_empty();
    let transcript: Element<'_, Message> = if nothing_to_show {
        empty_state(state)
    } else {
        let mut list = column![].spacing(10).width(Length::Fill);
        for e in &state.entries {
            list = list.push(entry_view(e, theme));
        }
        if let Some(live) = streaming_view(state, theme) {
            list = list.push(live);
        }
        // iced scrollbars float above the content; the right padding keeps the
        // vertical one from covering text.
        scrollable(list.padding(iced::Padding {
            top: 2.0,
            right: 12.0,
            bottom: 2.0,
            left: 0.0,
        }))
        .id(iced::widget::Id::new(TRANSCRIPT_ID))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    };

    // **`Fixed(width)` is load-bearing**: the dock hands every expanded panel
    // the column width it computed, and a `Fill`-width panel instead competes
    // with the drawing canvas for the same row space (which made the panel
    // take half the window). Every built-in panel pins its width this way.
    column![
        header(auto_collapse),
        session_picker(state),
        status_strip(state),
        transcript,
        composer(state, selection_label),
    ]
    .width(Length::Fixed(width))
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi::DeltaKind;

    fn state() -> PiPanelState {
        PiPanelState::empty()
    }

    #[test]
    fn split_tables_detects_pipe_tables() {
        let text = "前言\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n后记";
        let segments = split_tables(text);
        assert_eq!(segments.len(), 3, "{segments:?}");
        assert!(matches!(&segments[0], MdSegment::Markdown { text, .. } if text.contains("前言")));
        match &segments[1] {
            MdSegment::Table(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["A".to_string(), "B".to_string()]);
                assert_eq!(rows[1], vec!["1".to_string(), "2".to_string()]);
            }
            other => panic!("expected a table, got {other:?}"),
        }
        assert!(matches!(&segments[2], MdSegment::Markdown { text, .. } if text.contains("后记")));
    }

    #[test]
    fn assistant_body_only_splits_when_a_table_exists() {
        assert!(AssistantBody::new("普通文本，没有表格".into()).table_split.is_none());
        assert!(AssistantBody::new("带 | 竖线 但没表格".into()).table_split.is_none());
        let with_table = AssistantBody::new("| A |\n|---|\n| 1 |\n".into());
        assert!(with_table.table_split.is_some());
        assert_eq!(with_table.text, "| A |\n|---|\n| 1 |\n");
    }

    #[test]
    fn user_echo_consumes_optimistic_bubble() {
        let mut s = state();
        s.input = text_editor::Content::with_text("你好");
        assert!(s.send_current());
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.pending_echoes.len(), 1);
        // The SSE echo of the same message is consumed, not duplicated.
        assert!(!s.apply(Event::User("你好".into())));
        assert_eq!(s.entries.len(), 1);
        // A different user message still renders.
        assert!(s.apply(Event::User("再来一条".into())));
        assert_eq!(s.entries.len(), 2);
    }

    #[test]
    fn streaming_text_appends_in_place_and_finalizes() {
        let mut s = state();
        assert!(s.apply(Event::MsgStart { parts: vec![] }));
        assert!(s.apply(Event::Delta { kind: DeltaKind::Text, idx: 0, chunk: "你好".into() }));
        assert!(s.apply(Event::Delta { kind: DeltaKind::Text, idx: 0, chunk: "，世界".into() }));
        // Still one live block, not entries.
        assert_eq!(s.streaming.blocks.len(), 1);
        assert_eq!(s.entries.len(), 0);
        assert!(s.apply(Event::AssistantEnd { parts: vec![(0, Part::Text("你好，世界".into()))] }));
        // Final flush lands as one Assistant entry; live buffer cleared.
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(&s.entries[0].kind, PiEntryKind::Assistant(body) if body.text == "你好，世界"));
        assert!(s.streaming.blocks.is_empty());
    }

    #[test]
    fn thinking_block_streams_collapsed_and_lands_before_text() {
        let mut s = state();
        s.apply(Event::MsgStart { parts: vec![] });
        s.apply(Event::Delta { kind: DeltaKind::Thinking, idx: 0, chunk: "先想".into() });
        s.apply(Event::Delta { kind: DeltaKind::Thinking, idx: 0, chunk: "一想".into() });
        s.apply(Event::Delta { kind: DeltaKind::Text, idx: 1, chunk: "答案".into() });
        // Two live blocks: thinking at idx 0, text at idx 1.
        assert_eq!(s.streaming.blocks.len(), 2);
        assert!(s.apply(Event::AssistantEnd {
            parts: vec![(0, Part::Thinking("先想一想".into())), (1, Part::Text("答案".into()))],
        }));
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(&s.entries[0].kind, PiEntryKind::Thinking(_)));
        assert!(matches!(&s.entries[1].kind, PiEntryKind::Assistant(_)));
        // Thinking entries start collapsed.
        assert!(!s.entries[0].expanded);
    }

    #[test]
    fn tool_lifecycle_keyed_by_call_id() {
        let mut s = state();
        s.apply(Event::ToolCallStart { id: "t1".into(), name: "bash".into() });
        s.apply(Event::ToolPartial { id: "t1".into(), name: "bash".into(), output: "部分输出".into() });
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(&s.entries[0].kind, PiEntryKind::Tool { output, done: false, .. } if output == "部分输出"));
        // Duplicate start (toolcall_start + tool_execution_start) is idempotent.
        s.apply(Event::ToolCallStart { id: "t1".into(), name: "bash".into() });
        assert_eq!(s.entries.len(), 1);
        s.apply(Event::ToolResult { id: "t1".into(), name: "bash".into(), output: "全部输出".into(), is_error: false });
        assert!(matches!(&s.entries[0].kind, PiEntryKind::Tool { output, done: true, is_error: false, .. } if output == "全部输出"));
    }

    #[test]
    fn backfill_replace_keeps_stable_tool_ids() {
        let mut s = state();
        s.apply(Event::ToolCallStart { id: "t9".into(), name: "read".into() });
        let live_tool_id = s.entries[0].id;
        // Reconnect: backfill reconstructs the same tool call from the file.
        let replaced = s.apply(Event::Replace(vec![Entry::Tool {
            id: "t9".into(),
            name: "read".into(),
            output: "内容".into(),
            done: true,
            is_error: false,
        }]));
        assert!(replaced);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].id, live_tool_id, "tool id must survive reconnect backfill");
    }

    #[test]
    fn send_failed_removes_optimistic_bubble() {
        let mut s = state();
        s.input = text_editor::Content::with_text("会被拒绝的消息");
        s.send_current();
        assert_eq!(s.entries.len(), 1);
        assert!(s.apply(Event::SendFailed { message: "Session not found".into() }));
        // Bubble removed, notice added.
        assert!(matches!(&s.entries[0].kind, PiEntryKind::Notice(n) if n.contains("Session not found")));
        assert_eq!(s.entries.len(), 1);
        assert!(s.pending_echoes.is_empty());
    }

    #[test]
    fn queued_send_marks_message_and_streaming_flag_updates() {
        let mut s = state();
        s.status = PiStatus::Ready { session: "s1".into(), streaming: false };
        s.apply(Event::Streaming(true));
        assert!(s.is_streaming());
        s.apply(Event::Streaming(false));
        assert!(!s.is_streaming());
        s.apply(Event::Queue { steering: vec![], follow_up: vec!["a".into(), "b".into()] });
        assert_eq!(s.queued_followups, Some(2));
    }

    #[test]
    fn user_picked_session_beats_follow_newest() {
        let mut s = state();
        s.apply(Event::Sessions(vec![
            pi::SessionInfo { id: "newest".into(), label: "最新".into(), path: None, cwd: None },
            pi::SessionInfo { id: "picked".into(), label: "手选".into(), path: None, cwd: None },
        ]));
        // Default: newest.
        assert_eq!(s.active.as_deref(), Some("newest"));
        s.watch_session("picked".into());
        assert_eq!(s.active.as_deref(), Some("picked"));
        assert!(s.entries.is_empty());
        // A later session list refresh must not steal the user's pick.
        s.apply(Event::Sessions(vec![pi::SessionInfo {
            id: "newer".into(),
            label: "更新".into(),
            path: None,
            cwd: None,
        }]));
        assert_eq!(s.active.as_deref(), Some("picked"));
    }

    #[test]
    fn status_ready_updates_active_session() {
        let mut s = state();
        s.apply(Event::Status(Status::Ready { session: "abc".into(), streaming: true }));
        assert_eq!(s.active.as_deref(), Some("abc"));
        assert!(s.is_streaming());
        assert!(s.status_label().contains("生成中"));
    }
}
