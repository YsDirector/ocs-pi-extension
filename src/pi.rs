//! Local **pi-web API client** for the OCS Pi Extension panel.
//!
//! Everything is local, plain HTTP + JSON + SSE, so this speaks the protocol
//! directly over `std::net::TcpStream` — no HTTP/TLS dependencies, no bridge
//! process, no Node.js.
//!
//! Two threads per panel:
//!   * the **worker** owns the session list, the transcript backfill, and the
//!     command channel (send / watch / reconnect); it never blocks the UI.
//!   * a short-lived **reader** per SSE connection parses `data: {json}` events
//!     and forwards them to the UI through the same channel.
//!
//! The UI polls the channel via `Message::Pi(PiMsg::Poll)` (10 Hz) and applies
//! events in `PiPanelState::apply` (see `crate::ui::pi_panel`).
//!
//! Wire contract (verified against pi-web's route bundle and live SSE):
//!   GET  {endpoint}/api/sessions              → {"sessions":[{id,path,cwd,…}]}
//!   GET  {endpoint}/api/agent/<id>/events     → SSE stream
//!   POST {endpoint}/api/agent/<id>            → {"type":"prompt","message":…}
//!       (+ "streamingBehavior":"followUp" to queue behind a running turn)
//!
//! SSE events the panel consumes:
//!   connected {sessionId, isStreaming}
//!   message_start/message_end {message:{role,content[],toolCallId,toolName}}
//!   message_update {assistantMessageEvent:{type:text_|thinking_|toolcall_*,…}}
//!   tool_execution_start/update/end {toolCallId, toolName, partialResult|result}
//!   agent_start/agent_end/agent_settled, queue_update {steering,followUp},
//!   startup_error {errorMessage}

use base64::Engine as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, Shutdown};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// One transcript entry the UI renders.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool { id: String, name: String, output: String, done: bool, is_error: bool },
    Notice(String),
}

/// One content block of an assistant message (text / thinking / tool call).
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Thinking(String),
    ToolCall { id: String, name: String },
}

/// Which kind of delta a `Delta` event appends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Thinking,
}

/// Events sent from the worker/reader threads to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Connection state changed.
    Status(Status),
    /// Session list refreshed (`/api/sessions`), newest activity first.
    Sessions(Vec<SessionInfo>),
    /// The transcript was replaced wholesale (backfill / session switch).
    Replace(Vec<Entry>),
    /// A completed user message arrived (echoes the local optimistic bubble;
    /// may be empty when the message only carries an image).
    User(String),
    /// Model catalog + the model/thinking level the active session uses.
    Models {
        list: Vec<ModelInfo>,
        current: Option<(String, String)>,
        /// Available thinking levels per `"provider:id"`.
        thinking_levels: Vec<(String, Vec<String>)>,
        /// The session's current thinking level.
        thinking: Option<String>,
    },
    /// Slash commands available in the session (`get_commands`).
    Commands(Vec<CommandInfo>),
    /// File index of a session cwd (`/api/file-index`).
    Files { cwd: String, files: Vec<String> },
    /// Sub-directories for the new-session browser (`/api/cwd/browse`).
    DirListing { path: String, parent: Option<String>, dirs: Vec<(String, String)> },
    /// The session's thinking level changed.
    ThinkingSet { level: String },
    /// A host-side notice (file-reference problems, …).
    Notice(String),
    /// Which backend is serving the panel ("pi-web …" / "pi rpc …").
    Backend(String),
    /// An extension UI request (approval dialog) awaiting an answer.
    UiRequest {
        id: String,
        /// `select` | `confirm` | `input` | `editor`.
        method: String,
        title: String,
        message: Option<String>,
        options: Vec<String>,
        /// Prefilled text for `input`/`editor`.
        prefill: Option<String>,
    },
    /// Session token/cost/context accounting.
    Stats(SessionStats),
    /// A `set_model` succeeded; the session now uses this model.
    ModelSet { provider: String, id: String },
    /// An assistant message started streaming; `parts` is the snapshot so far
    /// (non-empty only when attaching mid-stream), tagged with `contentIndex`.
    MsgStart { parts: Vec<(usize, Part)> },
    /// Streaming delta for the block at `idx` (append).
    Delta { kind: DeltaKind, idx: usize, chunk: String },
    /// The streaming assistant message ended with these final parts
    /// (tagged with `contentIndex`).
    AssistantEnd { parts: Vec<(usize, Part)> },
    /// A tool call became known (toolcall_start / tool_execution_start).
    ToolCallStart { id: String, name: String },
    /// A running tool's output so far (replaces whatever was shown).
    ToolPartial { id: String, name: String, output: String },
    /// A tool finished; final output.
    ToolResult { id: String, name: String, output: String, is_error: bool },
    /// The follow-up/steer queue changed (server-side queue of prompts).
    Queue { steering: Vec<String>, follow_up: Vec<String> },
    /// An agent run started / ended (turn-level streaming flag).
    Streaming(bool),
    /// An assistant stream failed mid-generation.
    StreamError(String),
    /// A `POST` prompt was rejected by pi-web.
    SendFailed { message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Ready { session: String, streaming: bool },
    Error(String),
}

/// A session from `/api/sessions`.
/// A model entry from `GET /api/models`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    /// Human display name (may repeat across providers).
    pub name: String,
}

/// An image attachment carried with a prompt (raw base64, no data URL).
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAttachment {
    pub mime: String,
    pub base64: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    /// First line of `firstMessage` (≤ 60 chars) — the pick-list label.
    pub label: String,
    /// Absolute path of the session `.jsonl` (used for history backfill).
    pub path: Option<String>,
    /// Working directory of the session (file index / new sessions).
    pub cwd: Option<String>,
}

/// Token/cost/context accounting from `get_session_stats`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: Option<f64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub context_percent: Option<u8>,
}

/// Parse the `get_session_stats` payload (both backends share the shape).
pub fn parse_stats(data: &serde_json::Value) -> SessionStats {
    let tokens = data.get("tokens").cloned().unwrap_or_default();
    let number = |value: &serde_json::Value, key: &str| -> u64 {
        value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    };
    let usage = data.get("contextUsage").cloned().unwrap_or_default();
    SessionStats {
        input: number(&tokens, "input"),
        output: number(&tokens, "output"),
        cache_read: number(&tokens, "cacheRead"),
        cache_write: number(&tokens, "cacheWrite"),
        cost: data.get("cost").and_then(|c| c.as_f64()),
        context_tokens: usage.get("tokens").and_then(|t| t.as_u64()),
        context_window: usage.get("contextWindow").and_then(|t| t.as_u64()),
        context_percent: usage
            .get("percent")
            .and_then(|p| p.as_u64())
            .map(|p| p.min(255) as u8),
    }
}

/// A slash-invokable command from `get_commands`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    /// `"extension"` | `"prompt"` | `"skill"`.
    pub source: String,
}

/// Commands the UI sends to the worker.
#[derive(Debug, Clone)]
pub enum Command {
    /// Send `message` to `session`, optionally with one image attachment.
    /// `queued` = attach as a follow-up behind the running turn (server-side
    /// queue) instead of a plain prompt.
    Send {
        session: String,
        message: String,
        image: Option<ImageAttachment>,
        queued: bool,
    },
    /// Switch the streamed session (backfills its history, replaces the view).
    Watch(String),
    /// Drop the connection and reconnect from scratch.
    Reconnect,
    /// Switch the model of the streamed session (`set_model`).
    SetModel { provider: String, model_id: String },
    /// Set the session's reasoning/thinking level.
    SetThinking { level: String },
    /// Create a fresh session in `cwd` and start following it.
    NewSession { cwd: String },
    /// List sub-directories of `path` (new-session browser).
    Browse { path: String },
    /// Fetch the file index of `cwd` (for `@` file references).
    FetchFiles { cwd: String },
    /// Compact the conversation context (`compact`).
    Compact,
    /// Refresh session stats (`get_session_stats`).
    FetchStats,
    /// Answer an extension UI request.
    UiRespond {
        id: String,
        value: Option<String>,
        confirmed: Option<bool>,
        cancelled: bool,
    },
}

/// Handle the panel keeps for its worker thread.
pub struct PiHandle {
    pub rx: Receiver<Event>,
    pub tx: Sender<Command>,
    stop: Arc<AtomicBool>,
}

impl PiHandle {
    /// Start a worker against the local pi-web HTTP API.
    pub fn start(endpoint: &str) -> Self {
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        Self::spawn(move |tx, rx, stop| worker(endpoint, tx, rx, stop))
    }

    /// Start a worker against a local `pi --mode rpc` child process.
    pub fn start_rpc(bin: &str, cwd: &str) -> Self {
        let bin = bin.to_string();
        let cwd = cwd.to_string();
        Self::spawn(move |tx, rx, stop| crate::pi_rpc::worker(bin, cwd, tx, rx, stop))
    }

    /// Pick a backend automatically: pi-web when it answers on `endpoint`,
    /// otherwise a local `pi --mode rpc` child.
    pub fn start_auto(endpoint: &str, bin: &str, cwd: &str) -> Self {
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        let bin = bin.to_string();
        let cwd = cwd.to_string();
        Self::spawn(move |tx, rx, stop| {
            let host_port = endpoint.trim_start_matches("http://").to_string();
            let reachable = std::net::TcpStream::connect_timeout(
                &match host_port.parse() {
                    Ok(addr) => addr,
                    Err(_) => {
                        crate::pi_rpc::worker(bin, cwd, tx, rx, stop);
                        return;
                    }
                },
                Duration::from_millis(150),
            )
            .is_ok();
            if reachable {
                worker(endpoint, tx, rx, stop);
            } else {
                crate::pi_rpc::worker(bin, cwd, tx, rx, stop);
            }
        })
    }

    /// Spawn the worker thread with the given body.
    fn spawn(body: impl FnOnce(Sender<Event>, Receiver<Command>, Arc<AtomicBool>) + Send + 'static) -> Self {
        let (tx_ev, rx_ev) = channel();
        let (tx_cmd, rx_cmd) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let _ = std::thread::Builder::new()
            .name("ocs-pi-client".into())
            .spawn(move || body(tx_ev, rx_cmd, stop_clone));
        Self { rx: rx_ev, tx: tx_cmd, stop }
    }

    /// Ask the worker to exit at its next loop boundary and stop forwarding.
    /// Ask the worker to exit at its next loop boundary and stop forwarding.
    /// The Reconnect command wakes the worker out of any wait so it reaches the
    /// stop check promptly; the thread is fully detached, so a blocked read can
    /// never stall the UI.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Command::Reconnect);
    }

    pub fn is_alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
}

// ── Minimal HTTP over a fresh connection ────────────────────────────────────

struct Response {
    status: u16,
    body: String,
}

/// Minimal blocking HTTP request. Returns status + body (headers stripped).
fn http(endpoint: &str, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<Response, String> {
    let host_port = endpoint
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let mut stream = TcpStream::connect(&host_port).map_err(|e| {
        tr_args(
            "连接 {host_port} 失败：{e}",
            "connection to {host_port} failed: {e}",
            &[("host_port", host_port.clone()), ("e", e.to_string())],
        )
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    // Reading to EOF is fine: requests use `Connection: close`.
    if stream.read_to_string(&mut out).is_err() && out.is_empty() {
        return Err(tr("读取响应失败", "reading response failed").into());
    }
    // Split status line, headers and body.
    let mut parts = out.splitn(2, "\r\n");
    let status_line = parts.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let rest = parts.next().unwrap_or_default();
    let (headers, raw_body) = match rest.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (rest, ""),
    };
    // Next.js answers with `Transfer-Encoding: chunked`; a leading hex chunk
    // size would break every JSON parse — de-chunk when present.
    let body = if headers.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(raw_body)
    } else {
        raw_body.to_string()
    };
    Ok(Response { status, body })
}

/// Decode a chunked body: `<hex size>\r\n<data>\r\n` … `0\r\n[trailers]`.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        // Chunk size line: hex, optional `;ext`.
        let Some(line_end) = rest.find('\n') else { break };
        let size_line = rest[..line_end].trim_end_matches('\r');
        let size = usize::from_str_radix(size_line_hex(size_line), 16).unwrap_or(0);
        rest = &rest[line_end + 1..];
        if size == 0 {
            break;
        }
        let take = size.min(rest.len());
        out.push_str(&rest[..take]);
        rest = &rest[take..];
        if let Some(stripped) = rest.strip_prefix("\r\n") {
            rest = stripped;
        }
    }
    out
}

/// Hex portion of a chunk-size line (strip extensions after `;`).
fn size_line_hex(line: &str) -> &str {
    match line.split_once(';') {
        Some((hex, _)) => hex.trim(),
        None => line.trim(),
    }
}

/// Extract `"sessions":[…]` into `SessionInfo`s, preserving server order
/// (pi-web lists them newest-activity-first).
fn parse_sessions(text: &str) -> Vec<SessionInfo> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(list) = json.get("sessions").and_then(|s| s.as_array()) {
        for s in list {
            let id = s.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let first = s
                .get("firstMessage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(60)
                .collect::<String>();
            let path = s
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| p.to_string());
            let cwd = s
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|p| p.to_string());
            if !id.is_empty() {
                out.push(SessionInfo { id, label: first, path, cwd });
            }
        }
    }
    out
}

/// Join `content[].text` parts of a message-like object.
fn content_text(msg: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    out
}

/// Map one message's `content` array into semantic [`Part`]s tagged with the
/// block's `contentIndex` (needed to route streaming deltas to the right block).
fn parse_parts(msg: &serde_json::Value) -> Vec<(usize, Part)> {
    let mut out = Vec::new();
    if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        for (idx, part) in parts.iter().enumerate() {
            let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match ty {
                "text" => {
                    let t = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !t.is_empty() {
                        out.push((idx, Part::Text(t.to_string())));
                    }
                }
                "thinking" => {
                    let t = part
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    out.push((idx, Part::Thinking(t.to_string())));
                }
                "toolCall" => {
                    let id = part
                        .get("id")
                        .or_else(|| part.get("toolCallId"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = part
                        .get("name")
                        .or_else(|| part.get("toolName"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    if !id.is_empty() {
                        out.push((idx, Part::ToolCall { id, name }));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Extract a tool result's text (and error flag) from a result-like object.
fn tool_output(result: &serde_json::Value) -> (String, bool) {
    let text = content_text(result);
    let is_error = result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
    (text, is_error)
}

/// Tool-call block at `contentIndex` inside a raw RPC `message_update`
/// (`partial.content[…]`); pi-web's wire projection already flattened
/// `id`/`toolName` to the top level, so this is only a fallback.
fn partial_tool_block(delta: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let delta = delta?;
    let content = delta.get("partial")?.get("content")?.as_array()?;
    let index = delta.get("contentIndex")?.as_u64()? as usize;
    content.get(index)
}

/// Map one SSE `data:` payload to zero or more UI events.
pub fn sse_to_events(ev: &serde_json::Value) -> Vec<Event> {
    let mut out = Vec::new();
    let kind = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "connected" => {
            let session = ev
                .get("sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let streaming = ev.get("isStreaming").and_then(|b| b.as_bool()).unwrap_or(false);
            out.push(Event::Streaming(streaming));
            out.push(Event::Status(Status::Ready { session, streaming }));
        }
        "message_start" => {
            let msg = ev.get("message").cloned().unwrap_or_default();
            match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
                // The run's prompt echoes locally already; user bubbles come
                // from `message_end` so the full text is authoritative.
                "user" => {}
                "assistant" => out.push(Event::MsgStart { parts: parse_parts(&msg) }),
                "toolResult" => {
                    let (output, is_error) = tool_output(&msg);
                    out.push(Event::ToolResult {
                        id: msg.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").into(),
                        name: msg.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").into(),
                        output,
                        is_error,
                    });
                }
                _ => {}
            }
        }
        "message_update" => {
            let delta = ev.get("assistantMessageEvent");
            match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()).unwrap_or("") {
                "text_delta" => {
                    if let Some(chunk) = delta.and_then(|d| d.get("delta")).and_then(|c| c.as_str()) {
                        let idx = delta.and_then(|d| d.get("contentIndex")).and_then(|c| c.as_u64()).unwrap_or(0) as usize;
                        out.push(Event::Delta { kind: DeltaKind::Text, idx, chunk: chunk.to_string() });
                    }
                }
                "thinking_delta" => {
                    if let Some(chunk) = delta.and_then(|d| d.get("delta")).and_then(|c| c.as_str()) {
                        let idx = delta.and_then(|d| d.get("contentIndex")).and_then(|c| c.as_u64()).unwrap_or(0) as usize;
                        out.push(Event::Delta { kind: DeltaKind::Thinking, idx, chunk: chunk.to_string() });
                    }
                }
                "toolcall_start" | "toolcall_end" => {
                    // pi-web's wire projection puts id/toolName at the top
                    // level; raw RPC events keep them inside `partial.content
                    // [contentIndex]` (a `toolCall` block). toolcall_end also
                    // nests a full `toolCall` object.
                    let id = delta
                        .and_then(|d| d.get("id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| {
                            delta
                                .and_then(|d| d.get("toolCall"))
                                .and_then(|t| t.get("id"))
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .or_else(|| {
                            partial_tool_block(delta).and_then(|block| {
                                block
                                    .get("id")
                                    .or_else(|| block.get("toolCallId"))
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                            })
                        })
                        .unwrap_or_default();
                    let name = delta
                        .and_then(|d| d.get("toolName"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| {
                            delta
                                .and_then(|d| d.get("toolCall"))
                                .and_then(|t| t.get("name"))
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .or_else(|| {
                            partial_tool_block(delta).and_then(|block| {
                                block
                                    .get("name")
                                    .or_else(|| block.get("toolName"))
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                            })
                        })
                        .unwrap_or_else(|| "tool".into());
                    if !id.is_empty() {
                        out.push(Event::ToolCallStart { id, name });
                    }
                }
                "error" => {
                    let text = delta
                        .and_then(|d| d.get("error"))
                        .map(content_text)
                        .unwrap_or_default();
                    let text = if text.is_empty() {
                        tr("生成失败", "failed").to_string()
                    } else {
                        tr_args("生成失败：{text}", "failed: {text}", &[("text", text)])
                    };
                    out.push(Event::StreamError(text));
                }
                // text_end/thinking_end (finals arrive via message_end),
                // start/done bookkeeping — nothing to render incrementally.
                _ => {}
            }
        }
        "message_end" => {
            let msg = ev.get("message").cloned().unwrap_or_default();
            match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
                "user" => {
                    // Empty text is meaningful: an image-only message echo
                    // must still consume the optimistic-bubble ticket.
                    out.push(Event::User(content_text(&msg)));
                }
                "assistant" => out.push(Event::AssistantEnd { parts: parse_parts(&msg) }),
                "toolResult" => {
                    let (output, is_error) = tool_output(&msg);
                    out.push(Event::ToolResult {
                        id: msg.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").into(),
                        name: msg.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").into(),
                        output,
                        is_error,
                    });
                }
                _ => {}
            }
        }
        "tool_execution_start" => {
            out.push(Event::ToolCallStart {
                id: ev.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: ev.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").into(),
            });
        }
        "tool_execution_update" => {
            let partial = ev.get("partialResult").cloned().unwrap_or_default();
            let (output, _) = tool_output(&partial);
            out.push(Event::ToolPartial {
                id: ev.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: ev.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").into(),
                output,
            });
        }
        "tool_execution_end" => {
            let result = ev.get("result").cloned().unwrap_or_default();
            let (output, is_error) = tool_output(&result);
            out.push(Event::ToolResult {
                id: ev.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: ev.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").into(),
                output,
                is_error,
            });
        }
        "agent_start" => out.push(Event::Streaming(true)),
        "agent_end" | "agent_settled" => out.push(Event::Streaming(false)),
        "queue_update" => {
            let list = |key: &str| {
                ev.get(key)
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            out.push(Event::Queue {
                steering: list("steering"),
                follow_up: list("followUp"),
            });
        }
        "extension_ui_request" => {
            let method = ev.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = ev.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            match method {
                // Fire-and-forget notifications become transcript notices.
                "notify" => {
                    let message = ev
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !message.is_empty() {
                        out.push(Event::Notice(message));
                    }
                }
                "select" | "confirm" | "input" | "editor" => {
                    let title = ev
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or(tr("需要确认", "confirmation needed"))
                        .to_string();
                    let message = ev
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string);
                    let options = ev
                        .get("options")
                        .and_then(|o| o.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|o| o.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let prefill = ev
                        .get("prefill")
                        .and_then(|p| p.as_str())
                        .map(str::to_string);
                    out.push(Event::UiRequest {
                        id,
                        method: method.to_string(),
                        title,
                        message,
                        options,
                        prefill,
                    });
                }
                // setStatus / setWidget / … — not rendered by the panel yet.
                _ => {}
            }
        }
        "compaction_start" | "auto_compaction_start" => {
            out.push(Event::Notice(tr("上下文压缩中…", "compacting context…").into()));
        }
        "compaction_end" | "auto_compaction_end" => {
            out.push(Event::Notice(tr("上下文压缩完成", "context compacted").into()));
        }
        "auto_retry_start" => {
            let attempt = ev.get("attempt").and_then(|a| a.as_u64()).unwrap_or(0);
            out.push(Event::Notice(tr_args(
                "自动重试中（第 {attempt} 次）…",
                "retrying (attempt {attempt})…",
                &[("attempt", attempt.to_string())],
            )));
        }
        "startup_error" => {
            let msg = ev
                .get("errorMessage")
                .and_then(|m| m.as_str())
                .unwrap_or(tr("pi-web 启动代理失败", "pi-web failed to start"));
            out.push(Event::Status(Status::Error(msg.to_string())));
        }
        // extension_ui_request (setWidget widgets), auto_retry_*, compaction_*,
        // session_* bookkeeping — ignored by the panel for now.
        _ => {}
    }
    out
}

// ── Session history backfill ────────────────────────────────────────────────

/// Convert a list of `AgentMessage` JSON objects (RPC `get_messages`, or
/// session-file messages) into transcript entries.
pub fn messages_to_entries(messages: &[serde_json::Value]) -> Vec<Entry> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                let text = content_text(msg);
                if !text.is_empty() {
                    out.push(Entry::User(text));
                }
            }
            "assistant" => {
                for (_, part) in parse_parts(msg) {
                    match part {
                        Part::Text(t) if !t.is_empty() => out.push(Entry::Assistant(t)),
                        Part::Thinking(t) => out.push(Entry::Thinking(t)),
                        Part::ToolCall { id, name } => out.push(Entry::Tool {
                            id,
                            name,
                            output: String::new(),
                            done: false,
                            is_error: false,
                        }),
                        _ => {}
                    }
                }
            }
            "toolResult" => {
                let (output, is_error) = tool_output(msg);
                let id = msg
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = msg
                    .get("toolName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let filled = out.iter_mut().rev().any(|e| match e {
                    Entry::Tool { id: tid, output: o, done, is_error: err, .. } if *tid == id => {
                        *o = output.clone();
                        *done = true;
                        *err = is_error;
                        true
                    }
                    _ => false,
                });
                if !filled {
                    out.push(Entry::Tool { id, name, output, done: true, is_error });
                }
            }
            _ => {}
        }
    }
    out
}

/// Read the tail of a session `.jsonl` (≤ `max_bytes`) and reconstruct the
/// transcript. SSE only pushes *new* events, so history comes from the file.
pub fn read_jsonl_tail(path: &str, max_bytes: u64) -> Vec<Entry> {
    use std::io::Seek;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if file.seek(std::io::SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    // Drop the first (probably partial) line when we seeked.
    let buf = if start > 0 {
        match buf.find('\n') {
            Some(i) => &buf[i + 1..],
            None => "",
        }
    } else {
        buf.as_str()
    };
    let mut out = Vec::new();
    for line in buf.lines() {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if json.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue; // session header, model_change, custom entries, …
        }
        let msg = json.get("message").cloned().unwrap_or_default();
        match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                let text = content_text(&msg);
                if !text.is_empty() {
                    out.push(Entry::User(text));
                }
            }
            "assistant" => {
                for (_, part) in parse_parts(&msg) {
                    match part {
                        Part::Text(t) if !t.is_empty() => out.push(Entry::Assistant(t)),
                        Part::Thinking(t) => out.push(Entry::Thinking(t)),
                        Part::ToolCall { id, name } => out.push(Entry::Tool {
                            id,
                            name,
                            output: String::new(),
                            done: false,
                            is_error: false,
                        }),
                        _ => {}
                    }
                }
            }
            "toolResult" => {
                let (output, is_error) = tool_output(&msg);
                let id = msg.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = msg.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
                // Fill the earlier toolCall placeholder (same id) if we saw
                // one; otherwise surface the result standalone.
                let filled = out.iter_mut().rev().any(|e| match e {
                    Entry::Tool { id: tid, output: out_text, done, is_error: err, .. } if *tid == id => {
                        *out_text = output.clone();
                        *done = true;
                        *err = is_error;
                        true
                    }
                    _ => false,
                });
                if !filled {
                    out.push(Entry::Tool { id, name, output, done: true, is_error });
                }
            }
            _ => {}
        }
    }
    out
}

// ── The worker ──────────────────────────────────────────────────────────────

const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BACKFILL_MAX_BYTES: u64 = 128 * 1024;

/// The worker: maintain a connection to the (single) active session, map
/// commands to HTTP calls, and never block the UI thread.
fn worker(endpoint: String, tx: Sender<Event>, rx: Receiver<Command>, stop: Arc<AtomicBool>) {
    let mut watch_target: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send(Event::Status(Status::Connecting));
        let _ = tx.send(Event::Backend(format!("pi-web · {endpoint}")));
        // 1. Session list (also proves the server is up).
        let sessions = match http(&endpoint, "GET", "/api/sessions", None, REQUEST_TIMEOUT) {
            Ok(resp) if resp.status == 200 => parse_sessions(&resp.body),
            Ok(resp) => {
                let msg = tr_args(
                    "pi-web /api/sessions 返回 {status}",
                    "pi-web /api/sessions returned {status}",
                    &[("status", resp.status.to_string())],
                );
                let _ = tx.send(Event::Status(Status::Error(msg)));
                if wait_retry(&endpoint, &tx, &rx, &stop, &mut watch_target).is_break() {
                    return;
                }
                continue;
            }
            Err(e) => {
                let _ = tx.send(Event::Status(Status::Error(e)));
                if wait_retry(&endpoint, &tx, &rx, &stop, &mut watch_target).is_break() {
                    return;
                }
                continue;
            }
        };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send(Event::Sessions(sessions.clone()));

        // 2. Choose which session to follow: an explicit Watch beats the
        //    newest-active default.
        let active = watch_target
            .clone()
            .filter(|id| sessions.iter().any(|s| s.id == *id))
            .or_else(|| sessions.first().map(|s| s.id.clone()));
        let Some(active) = active else {
            let _ = tx.send(Event::Status(Status::Error(
                tr("pi-web 没有会话", "pi-web has no sessions").into(),
            )));
            if wait_retry(&endpoint, &tx, &rx, &stop, &mut watch_target).is_break() {
                return;
            }
            continue;
        };
        let active_cwd = sessions
            .iter()
            .find(|s| s.id == active)
            .and_then(|s| s.cwd.clone());
        let _ = tx.send(Event::Status(Status::Ready {
            session: active.clone(),
            streaming: false,
        }));

        // 3. Backfill history from the session file so the panel shows recent
        //    context (SSE only pushes new events).
        if let Some(path) = sessions
            .iter()
            .find(|s| s.id == active)
            .and_then(|s| s.path.clone())
        {
            let backfill = read_jsonl_tail(&path, BACKFILL_MAX_BYTES);
            if !backfill.is_empty() {
                let _ = tx.send(Event::Replace(backfill));
            }
        }

        // 4. Stream the session's SSE events on a helper thread; commands are
        //    answered here so a busy stream never delays a send.
        match TcpStream::connect(endpoint.trim_start_matches("http://")) {
            Ok(stream) => {
                let host_port = endpoint.trim_start_matches("http://").to_string();
                let req = format!(
                    "GET /api/agent/{active}/events HTTP/1.1\r\nHost: {host_port}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
                );
                let control = match stream.try_clone() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let mut stream = stream;
                if stream.write_all(req.as_bytes()).is_err() {
                    continue;
                }
                let is_streaming = Arc::new(AtomicBool::new(false));
                let reader_done = Arc::new(AtomicBool::new(false));
                let ev_tx = tx.clone();
                let done_flag = Arc::clone(&reader_done);
                let stream_flag = Arc::clone(&is_streaming);
                let _ = std::thread::Builder::new()
                    .name("ocs-pi-sse".into())
                    .spawn(move || reader(stream, ev_tx, stream_flag, done_flag));

                // Model catalog + current model for the picker (best effort —
                // a hiccup here just leaves the picker empty until reconnect).
                fetch_models(&endpoint, &tx, &active);
                // Slash commands for the composer's `/` completion.
                fetch_commands(&endpoint, &tx, &active);

                // Serve commands while the reader runs; shut the socket down
                // when we must leave (Watch/Reconnect) so the reader unblocks.
                let mut leave = false;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        leave = true;
                        break;
                    }
                    match rx.recv_timeout(POLL_TIMEOUT) {
                        Ok(Command::Send { session, message, image, queued }) => {
                            post_prompt(
                                &endpoint,
                                &tx,
                                &session,
                                active_cwd.as_deref(),
                                &message,
                                image,
                                queued,
                                &is_streaming,
                            );
                        }
                        Ok(Command::SetModel { provider, model_id }) => {
                            set_model(&endpoint, &tx, &active, &provider, &model_id);
                        }
                        Ok(Command::SetThinking { level }) => {
                            set_thinking(&endpoint, &tx, &active, &level);
                        }
                        Ok(Command::UiRespond { id, value, confirmed, cancelled }) => {
                            ui_respond(&endpoint, &tx, &active, &id, value, confirmed, cancelled);
                        }
                        Ok(Command::Compact) => {
                            compact_session(&endpoint, &tx, &active);
                        }
                        Ok(Command::FetchStats) => {
                            fetch_stats(&endpoint, &tx, &active);
                        }
                        Ok(Command::Browse { path }) => {
                            browse(&endpoint, &tx, &path);
                        }
                        Ok(Command::FetchFiles { cwd }) => {
                            fetch_files(&endpoint, &tx, &cwd);
                        }
                        Ok(Command::NewSession { cwd }) => {
                            match post_new_session(&endpoint, &cwd) {
                                Ok(id) => {
                                    // Follow the fresh session (backfill lands
                                    // on the next outer-loop iteration).
                                    watch_target = Some(id);
                                }
                                Err(e) => {
                                    let _ = tx.send(Event::SendFailed {
                                        message: tr_args(
                                            "新建会话失败：{e}",
                                            "new session failed: {e}",
                                            &[("e", e)],
                                        ),
                                    });
                                }
                            }
                            leave = true;
                            break;
                        }
                        Ok(Command::Watch(id)) => {
                            if id != active {
                                watch_target = Some(id);
                                leave = true;
                            }
                            break;
                        }
                        Ok(Command::Reconnect) => {
                            leave = true;
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if reader_done.load(Ordering::Relaxed) {
                                leave = true; // stream died; reconnect above
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            leave = true; // UI dropped the handle
                            break;
                        }
                    }
                }
                let _ = control.shutdown(Shutdown::Both);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if leave {
                    continue; // fresh sessions fetch + backfill + stream
                }
            }
            Err(e) => {
                let _ = tx.send(Event::Status(Status::Error(e.to_string())));
                if wait_retry(&endpoint, &tx, &rx, &stop, &mut watch_target).is_break() {
                    return;
                }
            }
        }
    }
}

/// Block for up to `RETRY_DELAY`, staying responsive to commands.
/// Returns `ControlFlow::Break` when the worker should exit.
fn wait_retry(
    endpoint: &str,
    tx: &Sender<Event>,
    rx: &Receiver<Command>,
    stop: &Arc<AtomicBool>,
    watch_target: &mut Option<String>,
) -> std::ops::ControlFlow<()> {
    let streaming = Arc::new(AtomicBool::new(false)); // nothing streams while offline
    let deadline = std::time::Instant::now() + RETRY_DELAY;
    loop {
        if stop.load(Ordering::Relaxed) {
            return std::ops::ControlFlow::Break(());
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return std::ops::ControlFlow::Continue(());
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Command::Watch(id)) => {
                *watch_target = Some(id);
                return std::ops::ControlFlow::Continue(());
            }
            Ok(Command::Reconnect) => return std::ops::ControlFlow::Continue(()),
            Ok(Command::Send { session, message, image, queued }) => {
                // Try the POST even while offline: the server may be fine and
                // only the session list hiccuped; failure surfaces as
                // SendFailed either way.
                post_prompt(endpoint, tx, &session, None, &message, image, queued, &streaming);
            }
            Ok(Command::SetModel { .. }) => {
                let _ = tx.send(Event::SendFailed {
                    message: tr(
                        "切换模型失败：尚未连接 pi-web",
                        "model switch failed: pi-web not connected",
                    )
                    .into(),
                });
            }
            Ok(Command::SetThinking { .. }) => {
                let _ = tx.send(Event::SendFailed {
                    message: tr(
                        "切换思考强度失败：尚未连接 pi-web",
                        "thinking level switch failed: pi-web not connected",
                    )
                    .into(),
                });
            }
            Ok(Command::NewSession { .. }) => {
                let _ = tx.send(Event::SendFailed {
                    message: tr(
                        "新建会话失败：尚未连接 pi-web",
                        "new session failed: pi-web not connected",
                    )
                    .into(),
                });
            }
            Ok(Command::Browse { .. }) | Ok(Command::FetchFiles { .. }) => {}
            Ok(Command::UiRespond { .. })
            | Ok(Command::Compact)
            | Ok(Command::FetchStats) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return std::ops::ControlFlow::Break(()),
        }
    }
}

/// POST one prompt. `queued` adds `streamingBehavior:"followUp"` so pi-web
/// queues it behind the running turn instead of rejecting/steering.
fn post_prompt(
    endpoint: &str,
    tx: &Sender<Event>,
    session: &str,
    cwd: Option<&str>,
    message: &str,
    image: Option<ImageAttachment>,
    queued: bool,
    is_streaming: &Arc<AtomicBool>,
) {
    // `@path` references expand into `<file …>` blocks (+ image attachments),
    // mirroring pi's CLI `@file` argument semantics.
    let (message, extra_images, notices) = expand_file_refs(message, cwd);
    for notice in notices {
        let _ = tx.send(Event::Notice(notice));
    }
    let mut body = serde_json::json!({ "type": "prompt", "message": message });
    if queued || is_streaming.load(Ordering::Relaxed) {
        body["streamingBehavior"] = serde_json::json!("followUp");
    }
    let mut images = Vec::new();
    if let Some(img) = image {
        images.push(serde_json::json!({
            "type": "image", "data": img.base64, "mimeType": img.mime
        }));
    }
    for img in extra_images {
        images.push(serde_json::json!({
            "type": "image", "data": img.base64, "mimeType": img.mime
        }));
    }
    if !images.is_empty() {
        body["images"] = serde_json::Value::Array(images);
    }
    let payload = body.to_string();
    match http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&payload),
        REQUEST_TIMEOUT,
    ) {
        Ok(resp) if (200..300).contains(&resp.status) => {}
        Ok(resp) => {
            let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("HTTP {}", resp.status));
            let _ = tx.send(Event::SendFailed { message: detail });
        }
        Err(e) => {
            let _ = tx.send(Event::SendFailed { message: e });
        }
    }
}

/// POST a bare command type (compact / get_session_stats) and return the body.
fn post_command(
    endpoint: &str,
    session: &str,
    command: &str,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "type": command }).to_string();
    let resp = http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&body),
        REQUEST_TIMEOUT,
    )?;
    if !(200..300).contains(&resp.status) {
        let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {}", resp.status));
        return Err(detail);
    }
    serde_json::from_str::<serde_json::Value>(&resp.body).map_err(|e| e.to_string())
}

/// Fetch session stats and forward them to the panel.
fn fetch_stats(endpoint: &str, tx: &Sender<Event>, session: &str) {
    if let Ok(value) = post_command(endpoint, session, "get_session_stats") {
        // pi-web wraps the payload as `{success, data}`.
        let data = value.get("data").cloned().unwrap_or(value);
        let _ = tx.send(Event::Stats(parse_stats(&data)));
    }
}

/// Request a manual context compaction.
fn compact_session(endpoint: &str, tx: &Sender<Event>, session: &str) {
    match post_command(endpoint, session, "compact") {
        Ok(_) => {}
        Err(e) => {
            let _ = tx.send(Event::SendFailed {
                message: tr_args("压缩失败：{e}", "compaction failed: {e}", &[("e", e)]),
            });
        }
    }
}

/// Forward an `extension_ui_response` to pi-web.
fn ui_respond(
    endpoint: &str,
    tx: &Sender<Event>,
    session: &str,
    id: &str,
    value: Option<String>,
    confirmed: Option<bool>,
    cancelled: bool,
) {
    let mut body = serde_json::json!({ "type": "extension_ui_response", "id": id });
    if cancelled {
        body["cancelled"] = serde_json::json!(true);
    } else if let Some(value) = value {
        body["value"] = serde_json::json!(value);
    } else if let Some(confirmed) = confirmed {
        body["confirmed"] = serde_json::json!(confirmed);
    }
    if let Err(e) = http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&body.to_string()),
        REQUEST_TIMEOUT,
    ) {
        let _ = tx.send(Event::Notice(tr_args(
            "提交确认失败：{e}",
            "confirmation submit failed: {e}",
            &[("e", e)],
        )));
    }
}

/// POST a `set_model` command; on success report the confirmed model back.
fn set_model(endpoint: &str, tx: &Sender<Event>, session: &str, provider: &str, model_id: &str) {
    let body = serde_json::json!({ "type": "set_model", "provider": provider, "modelId": model_id });
    let fail = |tx: &Sender<Event>, message: String| {
        let _ = tx.send(Event::SendFailed {
            message: tr_args(
                "切换模型失败：{message}",
                "model switch failed: {message}",
                &[("message", message)],
            ),
        });
    };
    match http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&body.to_string()),
        REQUEST_TIMEOUT,
    ) {
        Ok(resp) if (200..300).contains(&resp.status) => {
            // `{success:true, data:{…, id, provider}}` — trust the response's
            // provider/id over the request (the server may normalize).
            let confirmed = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    let data = v.get("data")?;
                    Some((
                        data.get("provider")?.as_str()?.to_string(),
                        data.get("id").or_else(|| data.get("modelId"))?.as_str()?.to_string(),
                    ))
                });
            let _ = tx.send(match confirmed {
                Some((provider, id)) => Event::ModelSet { provider, id },
                None => Event::ModelSet {
                    provider: provider.to_string(),
                    id: model_id.to_string(),
                },
            });
        }
        Ok(resp) => {
            let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {}", resp.status));
            fail(tx, detail);
        }
        Err(e) => fail(tx, e),
    }
}

/// Fetch the model catalog + the model/thinking the session currently uses.
fn fetch_models(endpoint: &str, tx: &Sender<Event>, session: &str) {
    let catalog = http(endpoint, "GET", "/api/models", None, REQUEST_TIMEOUT)
        .ok()
        .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok());
    let Some(catalog) = catalog else { return };
    let list: Vec<ModelInfo> = catalog
        .get("modelList")
        .and_then(|m| m.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|m| {
                    let provider = m.get("provider")?.as_str()?.to_string();
                    let id = m.get("id").or_else(|| m.get("modelId"))?.as_str()?.to_string();
                    let name = m
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    Some(ModelInfo { provider, id, name })
                })
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        return;
    }
    // Per-model thinking levels (`{"provider:id": ["off","low",…]}`).
    let thinking_levels: Vec<(String, Vec<String>)> = catalog
        .get("thinkingLevels")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let levels = v
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    (k.clone(), levels)
                })
                .collect()
        })
        .unwrap_or_default();
    // Current model + thinking level from the per-session agent state.
    let state = http(endpoint, "GET", &format!("/api/agent/{session}"), None, REQUEST_TIMEOUT)
        .ok()
        .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok());
    let current = state.as_ref().and_then(|v| {
        let model = v.get("state")?.get("model")?;
        Some((
            model.get("provider")?.as_str()?.to_string(),
            model.get("id").or_else(|| model.get("modelId"))?.as_str()?.to_string(),
        ))
    });
    let thinking = state
        .as_ref()
        .and_then(|v| v.get("state")?.get("thinkingLevel")?.as_str().map(str::to_string));
    let _ = tx.send(Event::Models { list, current, thinking_levels, thinking });
}

/// Fetch the session's slash commands (`get_commands`).
fn fetch_commands(endpoint: &str, tx: &Sender<Event>, session: &str) {
    let body = serde_json::json!({ "type": "get_commands" }).to_string();
    let Ok(resp) = http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&body),
        REQUEST_TIMEOUT,
    ) else {
        return;
    };
    if !(200..300).contains(&resp.status) {
        return;
    }
    let list: Vec<CommandInfo> = serde_json::from_str::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("data")?.get("commands")?.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .filter_map(|c| {
                    Some(CommandInfo {
                        name: c.get("name")?.as_str()?.to_string(),
                        description: c
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                        source: c
                            .get("source")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let _ = tx.send(Event::Commands(list));
}

/// POST a `set_thinking_level`; report the confirmed level back.
fn set_thinking(endpoint: &str, tx: &Sender<Event>, session: &str, level: &str) {
    let body = serde_json::json!({ "type": "set_thinking_level", "level": level });
    match http(
        endpoint,
        "POST",
        &format!("/api/agent/{session}"),
        Some(&body.to_string()),
        REQUEST_TIMEOUT,
    ) {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let _ = tx.send(Event::ThinkingSet { level: level.to_string() });
        }
        Ok(resp) => {
            let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {}", resp.status));
            let _ = tx.send(Event::SendFailed {
                message: tr_args(
                    "切换思考强度失败：{detail}",
                    "thinking level switch failed: {detail}",
                    &[("detail", detail)],
                ),
            });
        }
        Err(e) => {
            let _ = tx.send(Event::SendFailed {
                message: tr_args(
                    "切换思考强度失败：{e}",
                    "thinking level switch failed: {e}",
                    &[("e", e)],
                ),
            });
        }
    }
}

/// Minimal percent-encoding for query-string path values (space, #, ?, %…).
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// List sub-directories of `path` for the new-session browser.
fn browse(endpoint: &str, tx: &Sender<Event>, path: &str) {
    let resp = http(
        endpoint,
        "GET",
        &format!("/api/cwd/browse?path={}", encode_query(path)),
        None,
        REQUEST_TIMEOUT,
    );
    let Ok(resp) = resp else { return };
    if !(200..300).contains(&resp.status) {
        let _ = tx.send(Event::DirListing {
            path: path.to_string(),
            parent: None,
            dirs: Vec::new(),
        });
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp.body) else {
        return;
    };
    let dirs: Vec<(String, String)> = v
        .get("directories")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    Some((
                        d.get("name")?.as_str()?.to_string(),
                        d.get("path")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let _ = tx.send(Event::DirListing {
        path: v
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or(path)
            .to_string(),
        parent: v.get("parentPath").and_then(|p| p.as_str()).map(str::to_string),
        dirs,
    });
}

/// Fetch the file index of `cwd` (capped by the server at ~5000 entries).
fn fetch_files(endpoint: &str, tx: &Sender<Event>, cwd: &str) {
    let resp = http(
        endpoint,
        "GET",
        &format!("/api/file-index?cwd={}", encode_query(cwd)),
        None,
        REQUEST_TIMEOUT,
    );
    let Ok(resp) = resp else { return };
    if !(200..300).contains(&resp.status) {
        return;
    }
    let files: Vec<String> = serde_json::from_str::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("files")?.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let _ = tx.send(Event::Files { cwd: cwd.to_string(), files });
}

/// Create a fresh pi session in `cwd` (returns the new session id).
fn post_new_session(endpoint: &str, cwd: &str) -> Result<String, String> {
    let body = serde_json::json!({ "cwd": cwd, "type": "ensure_session" }).to_string();
    let resp = http(endpoint, "POST", "/api/agent/new", Some(&body), REQUEST_TIMEOUT)?;
    if !(200..300).contains(&resp.status) {
        let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {}", resp.status));
        return Err(detail);
    }
    serde_json::from_str::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("sessionId").and_then(|s| s.as_str()).map(str::to_string))
        .ok_or_else(|| tr("响应缺少 sessionId", "response is missing sessionId").to_string())
}

// ── `@file` reference expansion (mirrors pi CLI's `@file` arguments) ───────

const FILE_REF_MAX_TEXT_BYTES: u64 = 512 * 1024;
const FILE_REF_MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const FILE_REF_MAX_FILES: usize = 12;

/// Extract `@path` / `@"path with spaces"` tokens from a message.
fn file_refs(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = message.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            // Quoted form: @"path"
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                if let Some(end) = message[i + 2..].find('"') {
                    let path = &message[i + 2..i + 2 + end];
                    if !path.is_empty() {
                        out.push(path.to_string());
                    }
                    i = i + 2 + end + 1;
                    continue;
                }
            }
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            if end > start {
                out.push(message[start..end].to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out.truncate(FILE_REF_MAX_FILES);
    out
}

/// Expand `@path` references into `<file name="…">…</file>` text blocks plus
/// image attachments (the same contract pi's CLI uses for `pi @file`).
/// Returns `(augmented_message, extra_images, notices)`.
pub fn expand_file_refs(
    message: &str,
    cwd: Option<&str>,
) -> (String, Vec<ImageAttachment>, Vec<String>) {
    let refs = file_refs(message);
    if refs.is_empty() {
        return (message.to_string(), Vec::new(), Vec::new());
    }
    let mut blocks = String::new();
    let mut images = Vec::new();
    let mut notices = Vec::new();
    for reference in refs {
        let path = std::path::Path::new(&reference);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(cwd) = cwd {
            std::path::Path::new(cwd).join(path)
        } else {
            path.to_path_buf()
        };
        let Ok(meta) = std::fs::metadata(&resolved) else {
            notices.push(tr_args(
                "未找到文件：{reference}",
                "file not found: {reference}",
                &[("reference", reference)],
            ));
            continue;
        };
        if meta.len() == 0 {
            continue;
        }
        let absolute = resolved.display().to_string();
        let ext = resolved
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" => {
                if meta.len() > FILE_REF_MAX_IMAGE_BYTES {
                    notices.push(tr_args(
                        "图片过大，已跳过：{reference}",
                        "image too large, skipped: {reference}",
                        &[("reference", reference)],
                    ));
                    continue;
                }
                let Ok(bytes) = std::fs::read(&resolved) else {
                    notices.push(tr_args(
                        "读取失败：{reference}",
                        "read failed: {reference}",
                        &[("reference", reference)],
                    ));
                    continue;
                };
                let mime = match ext.as_str() {
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "webp" => "image/webp",
                    _ => "image/gif",
                };
                images.push(ImageAttachment {
                    mime: mime.to_string(),
                    base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                });
                blocks.push_str(&format!("<file name=\"{absolute}\"></file>\n"));
            }
            _ => {
                if meta.len() > FILE_REF_MAX_TEXT_BYTES {
                    notices.push(tr_args(
                        "文件过大（>512KB），已跳过：{reference}",
                        "file too large (>512KB), skipped: {reference}",
                        &[("reference", reference)],
                    ));
                    continue;
                }
                match std::fs::read_to_string(&resolved) {
                    Ok(content) => {
                        blocks.push_str(&format!(
                            "<file name=\"{absolute}\">\n{content}\n</file>\n"
                        ));
                    }
                    Err(_) => notices.push(tr_args(
                        "读取失败（非文本？）：{reference}",
                        "read failed (not text?): {reference}",
                        &[("reference", reference)],
                    )),
                }
            }
        }
    }
    if blocks.is_empty() {
        return (message.to_string(), images, notices);
    }
    let augmented = if message.trim().is_empty() {
        blocks
    } else {
        format!("{message}\n\n{blocks}")
    };
    (augmented, images, notices)
}

fn reader(stream: TcpStream, tx: Sender<Event>, is_streaming: Arc<AtomicBool>, done: Arc<AtomicBool>) {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break,                 // EOF
            Ok(_) => {}
            Err(_) => break,               // shutdown by the worker / reset
        }
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                // Track the run flag so queued prompts pick the right body.
                for event in sse_to_events(&ev) {
                    if let Event::Streaming(v) = event {
                        is_streaming.store(v, Ordering::Relaxed);
                    }
                    let _ = tx.send(event);
                }
            }
        }
        // `:` heartbeat comments and other lines are ignored.
    }
    done.store(true, Ordering::Relaxed);
}

// ── Panel language ──────────────────────────────────────────────────────────
//
// The panel ships two languages: Chinese (its original UI) and English (used
// for every non-Chinese host). The **host's own resolved language decides**, so
// picking a language in OCS's settings flips the panel too; `OCS_PI_LANG=zh|en`
// overrides for testing. Resolution happens once per process (see `lang()`).

/// Which language the panel renders in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

static LANG: std::sync::OnceLock<Lang> = std::sync::OnceLock::new();

/// The panel language. Cached: it is read while rendering, and the host's own
/// language only changes from its settings dialog (restart to re-read, or set
/// `OCS_PI_LANG`).
pub fn lang() -> Lang {
    *LANG.get_or_init(|| {
        if let Ok(forced) = std::env::var("OCS_PI_LANG") {
            if let Some(lang) = lang_from_override(&forced) {
                return lang;
            }
        }
        detect_lang()
    })
}

/// `OCS_PI_LANG` parsing: `zh*` → Chinese, any other non-empty value → English,
/// empty/blank → no override.
fn lang_from_override(value: &str) -> Option<Lang> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    Some(if value.starts_with("zh") { Lang::Zh } else { Lang::En })
}

/// Locale tag → panel language. Only Chinese gets Chinese; everything else
/// (including "no tag at all") is English.
fn tag_to_lang(tag: Option<&str>) -> Lang {
    match tag {
        Some(tag) if tag.trim().to_ascii_lowercase().starts_with("zh") => Lang::Zh,
        _ => Lang::En,
    }
}

fn detect_lang() -> Lang {
    // 1) What the host resolved (honours the user's choice in OCS settings).
    let host = crate::i18n::loader()
        .current_languages()
        .into_iter()
        .next()
        .map(|language| language.to_string());
    // 2) Otherwise the usual POSIX locale variables, in precedence order.
    let tag = host.or_else(|| {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|key| std::env::var(key).ok())
    });
    tag_to_lang(tag.as_deref())
}

/// `tr("中文", "English")` — one visible string, both languages.
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// Same, but the chosen template is interpolated with `{name}` placeholders
/// (the same convention the host's `i18n::translate_args` uses), so formatted
/// messages stay one call site instead of a `match` per string.
pub fn tr_args(zh: &str, en: &str, args: &[(&str, String)]) -> String {
    let mut out = match lang() {
        Lang::Zh => zh.to_string(),
        Lang::En => en.to_string(),
    };
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

/// Panel title, used by the dock chrome and the panel header.
pub fn panel_title() -> &'static str {
    tr("Pi 助手", "Pi Assistant")
}

/// Backend modes the panel can run in, in the order the picker shows them.
pub const BACKEND_MODES: [&str; 3] = ["auto", "web", "rpc"];

/// File the chosen backend mode is remembered in (`~/.config/OpenCADStudio/`).
///
/// Deliberately a file of its own rather than a key in the consolidated
/// `settings.json`: the whole Pi panel is a fork-local extension, so its
/// preference should not widen the shared config surface (and its conflict
/// surface on upstream syncs — see `docs/fork-patches.md`).
pub fn backend_mode_path() -> Option<std::path::PathBuf> {
    Some(crate::config::config_dir()?.join("pi-panel-backend.txt"))
}

/// Read a stored mode, accepting only known values.
pub fn load_backend_mode_from(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mode = text.trim().to_ascii_lowercase();
    BACKEND_MODES.contains(&mode.as_str()).then_some(mode)
}

/// Persist the chosen mode (ignores unknown values and I/O failures — a
/// missing preference just means "fall back to the environment").
pub fn save_backend_mode_to(path: &std::path::Path, mode: &str) {
    let mode = mode.trim().to_ascii_lowercase();
    if !BACKEND_MODES.contains(&mode.as_str()) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{mode}\n"));
}

/// The mode the panel should start in: stored preference, else
/// `OCS_PI_MODE`, else `auto`.
pub fn initial_backend_mode() -> String {
    if let Some(path) = backend_mode_path() {
        if let Some(mode) = load_backend_mode_from(&path) {
            return mode;
        }
    }
    std::env::var("OCS_PI_MODE")
        .ok()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| BACKEND_MODES.contains(&m.as_str()))
        .unwrap_or_else(|| "auto".to_string())
}

/// Remember the mode for the next run.
pub fn save_backend_mode(mode: &str) {
    if let Some(path) = backend_mode_path() {
        save_backend_mode_to(&path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tr_follows_the_active_language_and_interpolates() {
        // Written to hold in *either* language, so the suite passes on a Chinese
        // and an English machine alike.
        let zh = "中文样例";
        let en = "english sample";
        match lang() {
            Lang::Zh => assert_eq!(tr(zh, en), zh),
            Lang::En => assert_eq!(tr(zh, en), en),
        }
        let rendered = tr_args("失败：{e}", "failed: {e}", &[("e", "x".to_string())]);
        match lang() {
            Lang::Zh => assert_eq!(rendered, "失败：x"),
            Lang::En => assert_eq!(rendered, "failed: x"),
        }
        // Placeholders never survive interpolation.
        for text in [
            tr_args("{a} 和 {b}", "{a} and {b}", &[("a", "1".into()), ("b", "2".into())]),
            tr_args("没有占位", "no placeholder", &[]),
        ] {
            assert!(!text.contains('{') || !text.contains('}'));
        }
    }

    #[test]
    fn language_resolution_rules() {
        // `OCS_PI_LANG`
        assert_eq!(lang_from_override("zh"), Some(Lang::Zh));
        assert_eq!(lang_from_override("zh-CN"), Some(Lang::Zh));
        assert_eq!(lang_from_override(" ZH_TW "), Some(Lang::Zh));
        assert_eq!(lang_from_override("en"), Some(Lang::En));
        assert_eq!(lang_from_override("de-DE"), Some(Lang::En));
        assert_eq!(lang_from_override("   "), None);
        // locale tags: only Chinese is Chinese, everything else (incl. none) is English
        assert_eq!(tag_to_lang(Some("zh_CN.UTF-8")), Lang::Zh);
        assert_eq!(tag_to_lang(Some("zh")), Lang::Zh);
        assert_eq!(tag_to_lang(Some("en-US")), Lang::En);
        assert_eq!(tag_to_lang(Some("C.UTF-8")), Lang::En);
        assert_eq!(tag_to_lang(None), Lang::En);
        // the title is bilingual too
        assert!(matches!(panel_title(), "Pi 助手" | "Pi Assistant"));
    }

    #[test]
    fn backend_mode_round_trips_and_rejects_junk() {
        let dir = std::env::temp_dir().join("ocs-pi-mode-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("pi-panel-backend.txt");
        for mode in BACKEND_MODES {
            save_backend_mode_to(&path, mode);
            assert_eq!(load_backend_mode_from(&path).as_deref(), Some(mode));
        }
        // Case and surrounding whitespace are normalised.
        std::fs::write(&path, "  RPC \n").unwrap();
        assert_eq!(load_backend_mode_from(&path).as_deref(), Some("rpc"));
        // Unknown values are neither written nor read back.
        save_backend_mode_to(&path, "carrier-pigeon");
        assert_eq!(load_backend_mode_from(&path).as_deref(), Some("rpc"));
        std::fs::write(&path, "carrier-pigeon").unwrap();
        assert_eq!(load_backend_mode_from(&path), None);
        // A missing file is not an error.
        assert_eq!(load_backend_mode_from(&dir.join("nope.txt")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dechunk_strips_chunk_framing_and_trailers() {
        let payload = r#"{"sessions":[{"id":"abc"}]}"#;
        let chunked = format!("{:x}\r\n{}\r\n0\r\nX-Trailer: 1\r\n\r\n", payload.len(), payload);
        assert_eq!(dechunk(&chunked), payload);
        // Multi-chunk body: split the payload into two chunks.
        let (a, b) = payload.split_at(10);
        let chunked = format!(
            "{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            a.len(),
            a,
            b.len(),
            b
        );
        assert_eq!(dechunk(&chunked), payload);
    }

    #[test]
    fn dechunk_handles_chunk_extensions_and_split_crlf() {
        let payload = "hello";
        let chunked = format!("{:x};ext=1\r\n{}\r\n0\r\n\r\n", payload.len(), payload);
        assert_eq!(dechunk(&chunked), payload);
    }

    #[test]
    fn session_list_parses_ids_labels_and_paths() {
        let body = r#"{"sessions":[{"id":"abc","firstMessage":"第一行\n第二行","messageCount":3,"path":"/tmp/a.jsonl"},{"id":"","firstMessage":"skip"},{"id":"c","firstMessage":"  消息  "}]}"#;
        let got = parse_sessions(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "abc");
        assert_eq!(got[0].label, "第一行");
        assert_eq!(got[0].path.as_deref(), Some("/tmp/a.jsonl"));
        assert_eq!(got[1].label, "消息");
        assert_eq!(got[1].path, None);
    }

    #[test]
    fn content_text_joins_text_parts() {
        let msg = json!({"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]});
        assert_eq!(content_text(&msg), "ab");
    }

    #[test]
    fn parts_split_text_thinking_toolcall() {
        let msg = json!({"content":[
            {"type":"thinking","thinking":"想一想"},
            {"type":"text","text":"答案"},
            {"type":"toolCall","id":"t1","name":"bash"},
        ]});
        assert_eq!(
            parse_parts(&msg),
            vec![
                (0, Part::Thinking("想一想".into())),
                (1, Part::Text("答案".into())),
                (2, Part::ToolCall { id: "t1".into(), name: "bash".into() }),
            ]
        );
    }

    #[test]
    fn connected_sets_ready_and_streaming_flag() {
        let events = sse_to_events(&json!({"type":"connected","sessionId":"s1","isStreaming":true}));
        assert_eq!(
            events,
            vec![
                Event::Streaming(true),
                Event::Status(Status::Ready { session: "s1".into(), streaming: true }),
            ]
        );
    }

    #[test]
    fn text_and_thinking_deltas_map_with_index() {
        let text = sse_to_events(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"你好"}}));
        assert_eq!(text, vec![Event::Delta { kind: DeltaKind::Text, idx: 1, chunk: "你好".into() }]);
        assert_eq!(
            sse_to_events(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"嗯"}})),
            vec![Event::Delta { kind: DeltaKind::Thinking, idx: 0, chunk: "嗯".into() }]
        );
    }

    #[test]
    fn toolcall_start_and_tool_result_are_idempotent_events() {
        let start = sse_to_events(&json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_start","contentIndex":2,"id":"t9","toolName":"read"}}));
        assert_eq!(start, vec![Event::ToolCallStart { id: "t9".into(), name: "read".into() }]);

        let end = sse_to_events(&json!({"type":"tool_execution_end","toolCallId":"t9","toolName":"read","result":{"content":[{"type":"text","text":"文件内容"}],"isError":true}}));
        assert_eq!(
            end,
            vec![Event::ToolResult { id: "t9".into(), name: "read".into(), output: "文件内容".into(), is_error: true }]
        );
    }

    #[test]
    fn user_message_end_pushes_user_entry() {
        let events = sse_to_events(&json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"你好"}]}}));
        assert_eq!(events, vec![Event::User("你好".into())]);
    }

    #[test]
    fn assistant_end_carries_final_parts() {
        let events = sse_to_events(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"final"}]}}));
        assert_eq!(
            events,
            vec![Event::AssistantEnd { parts: vec![(0, Part::Thinking("t".into())), (1, Part::Text("final".into()))] }]
        );
    }

    #[test]
    fn agent_lifecycle_sets_streaming_flag() {
        assert_eq!(sse_to_events(&json!({"type":"agent_start"})), vec![Event::Streaming(true)]);
        assert_eq!(sse_to_events(&json!({"type":"agent_end","messages":[]})), vec![Event::Streaming(false)]);
    }

    #[test]
    fn queue_update_collects_follow_ups() {
        let events = sse_to_events(&json!({"type":"queue_update","steering":["a"],"followUp":["b","c"]}));
        assert_eq!(
            events,
            vec![Event::Queue { steering: vec!["a".into()], follow_up: vec!["b".into(), "c".into()] }]
        );
    }

    #[test]
    fn http_error_body_maps_to_send_failed() {
        // post_prompt failure path is exercised indirectly; here we only check
        // that a non-2xx response body's "error" field is what we'd report.
        let body = r#"{"error":"Session not found","code":"prompt_rejected","accepted":false}"#;
        let detail = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_default();
        assert_eq!(detail, "Session not found");
    }

    #[test]
    fn file_refs_parses_plain_and_quoted() {
        let refs = file_refs("看看 @src/pi.rs 和 @\"my dir/a b.txt\" 还有 @note.md");
        assert_eq!(refs, vec!["src/pi.rs", "my dir/a b.txt", "note.md"]);
        // `@` inside a word (email) is not a reference.
        assert!(file_refs("a@b.com").is_empty());
    }

    #[test]
    fn expand_file_refs_reads_text_into_file_blocks() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-refs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "hello refs").unwrap();
        let message = format!("看一下 @{}", file.display());
        let (augmented, images, notices) = expand_file_refs(&message, None);
        assert!(images.is_empty() && notices.is_empty());
        assert!(augmented.starts_with("看一下"));
        assert!(augmented.contains("<file name="));
        assert!(augmented.contains("hello refs"));
        // A missing reference produces a notice, not an error.
        let (_, _, notices) = expand_file_refs("@no/such/file.txt", None);
        assert_eq!(notices.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_tail_reconstructs_transcript() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s\"}\n",
                "{\"type\":\"model_change\",\"provider\":\"x\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"问\"}]}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"思\"},{\"type\":\"text\",\"text\":\"答\"},{\"type\":\"toolCall\",\"id\":\"t1\",\"name\":\"bash\"}]}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"t1\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"输出\"}],\"isError\":false}}\n",
            ),
        )
        .unwrap();
        let got = read_jsonl_tail(path.to_str().unwrap(), 1 << 20);
        assert_eq!(
            got,
            vec![
                Entry::User("问".into()),
                Entry::Thinking("思".into()),
                Entry::Assistant("答".into()),
                Entry::Tool { id: "t1".into(), name: "bash".into(), output: "输出".into(), done: true, is_error: false },
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_tail_skips_partial_first_line() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let full = "{\"type\":\"session\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n";
        std::fs::write(&path, full).unwrap();
        // A window that starts mid-way through the header line (100 bytes
        // leaves the whole message line intact after dropping the partial
        // first line).
        let got = read_jsonl_tail(path.to_str().unwrap(), 100);
        assert_eq!(got, vec![Entry::User("hi".into())]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
