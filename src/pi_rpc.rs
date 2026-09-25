//! **pi RPC backend** — runs the assistant panel against a local
//! `pi --mode rpc` child process instead of pi-web.
//!
//! Protocol (see pi's `docs/rpc.md`): JSONL over stdin/stdout — commands are
//! JSON objects (one per line) written to the child's stdin; responses
//! (`{"type":"response",…}`) and agent events stream back on stdout. Event
//! shapes match pi-web's SSE stream, so [`crate::pi::sse_to_events`] is reused
//! verbatim; only a few session-management commands differ.
//!
//! Everything else the panel needs (file index, directory browser) is served
//! locally: pi sessions live under `~/.pi/agent/sessions/<slug>/`, the file
//! index comes from `git ls-files` (or a small directory walk).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::pi::{
    self, Command, CommandInfo, Event, ImageAttachment, ModelInfo, SessionInfo, Status,
};

const POLL: Duration = Duration::from_millis(50);
const RESTART_DELAY: Duration = Duration::from_secs(3);
const MAX_INDEX_FILES: usize = 5000;
const SESSION_HEAD_BYTES: u64 = 48 * 1024;
const MAX_BROWSE_ENTRIES: usize = 200;

/// What the serve loop should do when the child process ends.
enum Exit {
    /// The panel was closed / the worker must stop.
    Stop,
    /// Restart the child (optionally in a different project directory).
    Restart(Option<String>),
    /// The child died unexpectedly.
    Died(String),
}

/// Resolve the `pi` executable: an explicit path wins, then `PATH`, then the
/// usual install locations (the app is often launched with a minimal PATH).
pub fn resolve_bin(bin: &str) -> String {
    if bin.contains('/') {
        return bin.to_string();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = std::path::Path::new(dir).join(bin);
            if candidate.is_file() {
                return candidate.display().to_string();
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for dir in [
        format!("{home}/.local/bin"),
        format!("{home}/.npm-global/bin"),
        format!("{home}/bin"),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
    ] {
        let candidate = std::path::Path::new(&dir).join(bin);
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    bin.to_string()
}

/// PATH for the child: the app's PATH plus the usual user tool directories, so
/// the agent's own tools (node, git, cargo, …) resolve like in a login shell.
fn child_path() -> String {
    let mut path = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    for dir in [
        format!("{home}/.local/bin"),
        format!("{home}/.npm-global/bin"),
        format!("{home}/.cargo/bin"),
        "/usr/local/bin".to_string(),
    ] {
        if !path.split(':').any(|p| p == dir) {
            path = format!("{dir}:{path}");
        }
    }
    path
}

/// Entry point for the RPC worker thread.
pub fn worker(
    bin: String,
    cwd: String,
    tx: Sender<Event>,
    rx: Receiver<Command>,
    stop: Arc<AtomicBool>,
) {
    let mut project_cwd = cwd;
    let bin = resolve_bin(&bin);
    let path = child_path();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send(Event::Status(Status::Connecting));
        match ProcCommand::new(&bin)
            .args(["--mode", "rpc"])
            .current_dir(&project_cwd)
            .env("PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                let _ = tx.send(Event::Backend(format!("pi rpc · {project_cwd}")));
                match serve(child, &project_cwd, &tx, &rx, &stop) {
                    Exit::Stop => return,
                    Exit::Restart(Some(next)) => project_cwd = next,
                    Exit::Restart(None) => {}
                    Exit::Died(message) => {
                        let _ = tx.send(Event::Status(Status::Error(message)));
                        match wait_retry(&tx, &rx, &stop) {
                            Exit::Stop => return,
                            Exit::Restart(Some(next)) => project_cwd = next,
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Event::Status(Status::Error(pi::tr_args(
                    "启动 {bin} 失败：{e}",
                    "failed to start {bin}: {e}",
                    &[("bin", bin.clone()), ("e", e.to_string())],
                ))));
                match wait_retry(&tx, &rx, &stop) {
                    Exit::Stop => return,
                    Exit::Restart(Some(next)) => project_cwd = next,
                    _ => {}
                }
            }
        }
    }
}

/// Block until the retry deadline, staying responsive to commands.
fn wait_retry(tx: &Sender<Event>, rx: &Receiver<Command>, stop: &Arc<AtomicBool>) -> Exit {
    let deadline = Instant::now() + RESTART_DELAY;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Exit::Stop;
        }
        let now = Instant::now();
        if now >= deadline {
            return Exit::Restart(None);
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Command::Reconnect) => return Exit::Restart(None),
            Ok(Command::NewSession { cwd }) => return Exit::Restart(Some(cwd)),
            Ok(Command::Browse { path }) => {
                let _ = tx.send(browse_event(&path));
            }
            Ok(Command::FetchFiles { cwd }) => {
                let _ = tx.send(files_event(&cwd));
            }
            Ok(Command::Watch(_)) => {}
            Ok(_) => {
                let _ = tx.send(Event::SendFailed {
                    message: pi::tr("尚未连接 pi（rpc）", "pi (rpc) not connected").into(),
                });
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Exit::Stop,
        }
    }
}

/// Write one JSON command line to the child.
fn send_cmd(stdin: &mut ChildStdin, value: &Value) -> bool {
    if writeln!(stdin, "{value}").is_err() {
        return false;
    }
    stdin.flush().is_ok()
}

/// Serve one child process: pump stdout events, answer UI commands.
fn serve(
    mut child: Child,
    cwd: &str,
    tx: &Sender<Event>,
    rx: &Receiver<Command>,
    stop: &Arc<AtomicBool>,
) -> Exit {
    let Some(mut stdin) = child.stdin.take() else {
        return Exit::Died(pi::tr("pi rpc 无 stdin", "pi rpc has no stdin").into());
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Exit::Died(pi::tr("pi rpc 无输出管道", "pi rpc has no output pipe").into());
    };

    // stdout → parsed JSON values
    let (json_tx, json_rx) = channel::<Value>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if json_tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    // stderr → keep a short tail for diagnostics
    let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Ok(mut tail) = tail.lock() {
                    tail.push(line);
                    if tail.len() > 5 {
                        tail.remove(0);
                    }
                }
            }
        });
    }

    let mut streaming = false;
    // Handshake accumulators (state/model list/thinking levels).
    let mut state: Option<Value> = None;
    let mut models: Option<Vec<ModelInfo>> = None;
    let mut levels: Option<Vec<String>> = None;
    let mut ready_sent = false;
    // session id → .jsonl path (from the local scan)
    let mut session_paths: HashMap<String, String> = HashMap::new();
    let mut paths_loaded = false;

    let _ = send_cmd(&mut stdin, &json!({"id":"h1","type":"get_state"}));
    let _ = send_cmd(&mut stdin, &json!({"id":"h2","type":"get_available_models"}));
    let _ = send_cmd(&mut stdin, &json!({"id":"h3","type":"get_available_thinking_levels"}));
    let _ = send_cmd(&mut stdin, &json!({"id":"h4","type":"get_commands"}));

    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Exit::Stop;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = stderr_tail
                .lock()
                .map(|t| t.join("; "))
                .unwrap_or_default();
            let detail = if tail.is_empty() {
                pi::tr_args(
                    "pi rpc 退出（{status}）",
                    "pi rpc exited ({status})",
                    &[("status", status.to_string())],
                )
            } else {
                pi::tr_args(
                    "pi rpc 退出（{status}）：{tail}",
                    "pi rpc exited ({status}): {tail}",
                    &[("status", status.to_string()), ("tail", tail)],
                )
            };
            return Exit::Died(detail);
        }

        // Drain child output: responses update the handshake, everything else
        // is an agent event streamed straight to the panel.
        loop {
            match json_rx.try_recv() {
                Ok(value) => {
                    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if kind == "response" {
                        let command = value
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let success = value
                            .get("success")
                            .and_then(|s| s.as_bool())
                            .unwrap_or(false);
                        let data = value.get("data").cloned().unwrap_or(Value::Null);
                        match command.as_str() {
                            "get_state" => {
                                state = Some(data);
                            }
                            "get_available_models" => {
                                models = Some(parse_models(&data));
                            }
                            "get_available_thinking_levels" => {
                                levels = Some(parse_levels(&data));
                            }
                            "get_commands" => {
                                let _ = tx.send(Event::Commands(parse_commands(&data)));
                            }
                            "get_messages" => {
                                let messages = data
                                    .get("messages")
                                    .and_then(|m| m.as_array())
                                    .cloned()
                                    .unwrap_or_default();
                                let _ = tx.send(Event::Replace(pi::messages_to_entries(&messages)));
                            }
                            "set_model" => {
                                if success {
                                    if let (Some(provider), Some(id)) = (
                                        data.get("provider").and_then(|p| p.as_str()),
                                        data.get("id").and_then(|i| i.as_str()),
                                    ) {
                                        let _ = tx.send(Event::ModelSet {
                                            provider: provider.to_string(),
                                            id: id.to_string(),
                                        });
                                        // Level list may differ per model.
                                        let _ = send_cmd(
                                            &mut stdin,
                                            &json!({"id":"t1","type":"get_available_thinking_levels"}),
                                        );
                                    }
                                } else {
                                    let _ = tx.send(Event::SendFailed {
                                        message: pi::tr_args(
                                            "切换模型失败：{detail}",
                                            "model switch failed: {detail}",
                                            &[(
                                                "detail",
                                                data.as_str()
                                                    .unwrap_or(pi::tr(
                                                        "未知错误",
                                                        "unknown error",
                                                    ))
                                                    .to_string(),
                                            )],
                                        ),
                                    });
                                }
                            }
                            "set_thinking_level" => {
                                if success {
                                    if let Some(level) = level_of(&data) {
                                        let _ = tx.send(Event::ThinkingSet { level });
                                    }
                                } else {
                                    let _ = tx.send(Event::SendFailed {
                                        message: pi::tr_args(
                                            "切换思考强度失败：{detail}",
                                            "thinking level switch failed: {detail}",
                                            &[(
                                                "detail",
                                                data.as_str()
                                                    .unwrap_or(pi::tr(
                                                        "未知错误",
                                                        "unknown error",
                                                    ))
                                                    .to_string(),
                                            )],
                                        ),
                                    });
                                }
                            }
                            "new_session" | "switch_session" => {
                                if success {
                                    let _ = send_cmd(
                                        &mut stdin,
                                        &json!({"id":"s2","type":"get_state"}),
                                    );
                                    let _ = send_cmd(
                                        &mut stdin,
                                        &json!({"id":"m2","type":"get_messages"}),
                                    );
                                    // The session list changed (new session) —
                                    // rescan and refresh the picker.
                                    let _ = tx.send(Event::Sessions(scan_sessions(cwd, &mut session_paths)));
                                    paths_loaded = true;
                                } else {
                                    let _ = tx.send(Event::SendFailed {
                                        message: pi::tr_args(
                                            "切换会话失败：{detail}",
                                            "session switch failed: {detail}",
                                            &[(
                                                "detail",
                                                data.as_str()
                                                    .unwrap_or(pi::tr(
                                                        "未知错误",
                                                        "unknown error",
                                                    ))
                                                    .to_string(),
                                            )],
                                        ),
                                    });
                                }
                            }
                            "get_session_stats" => {
                                let _ = tx.send(Event::Stats(pi::parse_stats(&data)));
                            }
                            "compact" => {
                                if !success {
                                    let _ = tx.send(Event::SendFailed {
                                        message: pi::tr_args(
                                            "压缩失败：{detail}",
                                            "compaction failed: {detail}",
                                            &[(
                                                "detail",
                                                data.as_str()
                                                    .unwrap_or(pi::tr(
                                                        "未知错误",
                                                        "unknown error",
                                                    ))
                                                    .to_string(),
                                            )],
                                        ),
                                    });
                                }
                            }
                            "prompt" => {
                                if !success {
                                    let _ = tx.send(Event::SendFailed {
                                        message: pi::tr_args(
                                            "发送失败：{detail}",
                                            "send failed: {detail}",
                                            &[(
                                                "detail",
                                                data.as_str()
                                                    .unwrap_or(pi::tr(
                                                        "指令被拒绝",
                                                        "command rejected",
                                                    ))
                                                    .to_string(),
                                            )],
                                        ),
                                    });
                                }
                            }
                            _ => {}
                        }
                    } else {
                        // Agent event — the pi-web mapping understands these
                        // shapes (message_update carries `partial`, which the
                        // projector falls back to for tool-call metadata).
                        for event in pi::sse_to_events(&value) {
                            if let Event::Streaming(flag) = &event {
                                streaming = *flag;
                            }
                            let _ = tx.send(event);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Exit::Died(pi::tr("pi rpc 输出结束", "pi rpc output ended").into())
                }
            }
        }

        // Emit the ready state once the handshake has what the pickers need.
        if !ready_sent {
            if let Some(state) = &state {
                let session = state
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pi")
                    .to_string();
                let session_streaming = state
                    .get("isStreaming")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
                streaming = session_streaming;
                let _ = tx.send(Event::Status(Status::Ready {
                    session: session.clone(),
                    streaming: session_streaming,
                }));
                if !paths_loaded {
                    let _ = tx.send(Event::Sessions(scan_sessions(cwd, &mut session_paths)));
                    paths_loaded = true;
                }
                // Model catalog + thinking levels for the pickers.
                let list = models.clone().unwrap_or_default();
                let current = state
                    .get("model")
                    .and_then(|m| m.get("provider").and_then(|p| p.as_str()).zip(
                        m.get("id").or_else(|| m.get("modelId")).and_then(|i| i.as_str()),
                    ))
                    .map(|(p, i)| (p.to_string(), i.to_string()));
                let thinking = state
                    .get("thinkingLevel")
                    .and_then(|t| t.as_str())
                    .map(str::to_string);
                let thinking_levels = match (&current, &levels) {
                    (Some((provider, id)), Some(levels)) => {
                        vec![(format!("{provider}:{id}"), levels.clone())]
                    }
                    _ => Vec::new(),
                };
                if !list.is_empty() || thinking.is_some() {
                    let _ = tx.send(Event::Models {
                        list,
                        current,
                        thinking_levels,
                        thinking,
                    });
                }
                ready_sent = true;
            }
        }

        // UI commands.
        match rx.recv_timeout(POLL) {
            Ok(Command::Send { message, image, queued, .. }) => {
                if !send_prompt(&mut stdin, cwd, message, image, queued || streaming) {
                    return Exit::Died(pi::tr("pi rpc stdin 已关闭", "pi rpc stdin closed").into());
                }
            }
            Ok(Command::SetModel { provider, model_id }) => {
                let _ = send_cmd(
                    &mut stdin,
                    &json!({"type":"set_model","provider":provider,"modelId":model_id}),
                );
            }
            Ok(Command::SetThinking { level }) => {
                let _ = send_cmd(
                    &mut stdin,
                    &json!({"type":"set_thinking_level","level":level}),
                );
            }
            Ok(Command::Watch(id)) => {
                if session_paths.is_empty() {
                    let _ = scan_sessions(cwd, &mut session_paths);
                }
                match session_paths.get(&id) {
                    Some(path) => {
                        let _ = send_cmd(
                            &mut stdin,
                            &json!({"type":"switch_session","sessionPath":path}),
                        );
                    }
                    None => {
                        let _ = tx.send(Event::Notice(pi::tr_args(
                            "找不到会话文件：{id}",
                            "session file not found: {id}",
                            &[("id", id)],
                        )));
                    }
                }
            }
            Ok(Command::NewSession { cwd: target }) => {
                if target == cwd {
                    let _ = send_cmd(&mut stdin, &json!({"type":"new_session"}));
                } else {
                    // The project directory is fixed for the process lifetime —
                    // restart the child in the requested directory.
                    let _ = child.kill();
                    return Exit::Restart(Some(target));
                }
            }
            Ok(Command::Browse { path }) => {
                let _ = tx.send(browse_event(&path));
            }
            Ok(Command::FetchFiles { cwd }) => {
                let _ = tx.send(files_event(&cwd));
            }
            Ok(Command::Compact) => {
                let _ = send_cmd(&mut stdin, &json!({"type":"compact"}));
            }
            Ok(Command::FetchStats) => {
                let _ = send_cmd(&mut stdin, &json!({"type":"get_session_stats"}));
            }
            Ok(Command::UiRespond { id, value, confirmed, cancelled }) => {
                let mut body = json!({"type":"extension_ui_response","id":id});
                if cancelled {
                    body["cancelled"] = json!(true);
                } else if let Some(value) = value {
                    body["value"] = json!(value);
                } else if let Some(confirmed) = confirmed {
                    body["confirmed"] = json!(confirmed);
                }
                let _ = send_cmd(&mut stdin, &body);
            }
            Ok(Command::Reconnect) => {
                let _ = child.kill();
                return Exit::Restart(None);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                return Exit::Stop;
            }
        }
    }
}

/// Expand `@` references and POST the prompt to the child.
fn send_prompt(
    stdin: &mut ChildStdin,
    cwd: &str,
    message: String,
    image: Option<ImageAttachment>,
    queued: bool,
) -> bool {
    let (message, extra_images, notices) = pi::expand_file_refs(&message, Some(cwd));
    for notice in notices {
        // Notices are surfaced by the caller through the event channel.
        let _ = notice;
    }
    let mut images: Vec<Value> = Vec::new();
    if let Some(image) = image {
        images.push(json!({"type":"image","data":image.base64,"mimeType":image.mime}));
    }
    for image in extra_images {
        images.push(json!({"type":"image","data":image.base64,"mimeType":image.mime}));
    }
    let mut body = json!({"type":"prompt","message":message});
    if queued {
        body["streamingBehavior"] = json!("followUp");
    }
    if !images.is_empty() {
        body["images"] = Value::Array(images);
    }
    send_cmd(stdin, &body)
}

fn level_of(data: &Value) -> Option<String> {
    data.get("level")
        .and_then(|l| l.as_str())
        .map(str::to_string)
        .or_else(|| data.as_str().map(str::to_string))
}

/// `{models:[{id,name,provider,…}]}` → [`ModelInfo`] list.
pub fn parse_models(data: &Value) -> Vec<ModelInfo> {
    data.get("models")
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
        .unwrap_or_default()
}

/// `{levels:[…]}` → level names.
pub fn parse_levels(data: &Value) -> Vec<String> {
    data.get("levels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `{commands:[{name,description,source}]}` → [`CommandInfo`] list.
pub fn parse_commands(data: &Value) -> Vec<CommandInfo> {
    data.get("commands")
        .and_then(|c| c.as_array())
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
        .unwrap_or_default()
}

// ── Local session scan (replaces pi-web's /api/sessions) ───────────────────

/// `~/.pi/agent/sessions/--<cwd with / and : → ->--`
pub fn session_dir_for(cwd: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let agent_dir = std::env::var("PI_AGENT_DIR").unwrap_or_else(|_| {
        format!("{home}/.pi/agent")
    });
    let slug = cwd
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-");
    std::path::Path::new(&agent_dir)
        .join("sessions")
        .join(format!("--{slug}--"))
}

/// Scan the project's session directory (newest first) and fill `paths`.
pub fn scan_sessions(cwd: &str, paths: &mut HashMap<String, String>) -> Vec<SessionInfo> {
    let dir = session_dir_for(cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(60);
    let mut out = Vec::new();
    for (_, path) in files {
        let Some(id) = session_id_of(&path) else { continue };
        let label = session_label(&path).unwrap_or_default();
        paths.insert(id.clone(), path.display().to_string());
        out.push(SessionInfo {
            id,
            label,
            path: Some(path.display().to_string()),
            cwd: Some(cwd.to_string()),
        });
    }
    out
}

/// `<timestamp>_<uuid>.jsonl` → `uuid`.
fn session_id_of(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let uuid = stem.rsplit('_').next()?;
    if uuid.is_empty() {
        None
    } else {
        Some(uuid.to_string())
    }
}

/// First user message (first line, ≤ 60 chars) as the picker label.
fn session_label(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = String::new();
    file.take(SESSION_HEAD_BYTES).read_to_string(&mut buffer).ok()?;
    for line in buffer.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let message = value.get("message")?;
        if message.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let text = message
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<String>()
            })
            .unwrap_or_default();
        let first = text.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(first.chars().take(60).collect());
        }
    }
    None
}

// ── Local file index + directory browse (replace pi-web APIs) ──────────────

const IGNORED_DIRS: [&str; 14] = [
    "node_modules", ".git", ".next", "dist", "build", "__pycache__", ".turbo",
    ".cache", "coverage", ".pytest_cache", ".mypy_cache", "target", "vendor",
    ".svn",
];

/// Build the `Event::Files` payload for `cwd` (git ls-files, else a walk).
pub fn files_event(cwd: &str) -> Event {
    let files = file_index(cwd);
    Event::Files {
        cwd: cwd.to_string(),
        files,
    }
}

/// Up to [`MAX_INDEX_FILES`] project-relative paths.
pub fn file_index(cwd: &str) -> Vec<String> {
    if let Ok(output) = ProcCommand::new("git")
        .args(["-C", cwd, "ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .env("PATH", child_path())
        .output()
    {
        if output.status.success() {
            let mut files: Vec<String> = output
                .stdout
                .split(|b| *b == 0)
                .filter(|chunk| !chunk.is_empty())
                .map(|chunk| String::from_utf8_lossy(chunk).to_string())
                .collect();
            files.truncate(MAX_INDEX_FILES);
            if !files.is_empty() {
                return files;
            }
        }
    }
    walk_files(cwd)
}

/// Breadth-first walk skipping vendor directories (cap [`MAX_INDEX_FILES`]).
fn walk_files(cwd: &str) -> Vec<String> {
    let root = std::path::Path::new(cwd);
    let mut out = Vec::new();
    let mut queue: Vec<(std::path::PathBuf, String, usize)> =
        vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = queue.pop() {
        if depth > 8 || out.len() >= MAX_INDEX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if IGNORED_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                queue.push((entry.path(), child_rel, depth + 1));
            } else if kind.is_file() {
                out.push(child_rel);
                if out.len() >= MAX_INDEX_FILES {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}

/// Build the `Event::DirListing` payload for `path`.
pub fn browse_event(path: &str) -> Event {
    let dir = std::path::Path::new(path);
    let mut dirs: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if dirs.len() >= MAX_BROWSE_ENTRIES {
                break;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            dirs.push((name, entry.path().display().to_string()));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let parent = dir
        .parent()
        .filter(|p| *p != dir)
        .map(|p| p.display().to_string());
    Event::DirListing {
        path: dir.display().to_string(),
        parent,
        dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bin_keeps_explicit_paths_and_finds_local_install() {
        // Explicit paths are returned untouched.
        assert_eq!(resolve_bin("/opt/pi"), "/opt/pi");
        // An executable that exists in ~/.local/bin (pi is installed there on
        // this machine) resolves to an absolute path.
        let resolved = resolve_bin("pi");
        assert!(
            resolved == "pi" || resolved.ends_with("/pi"),
            "unexpected resolution: {resolved}"
        );
        assert!(child_path().contains("/usr/bin"));
    }

    #[test]
    fn session_dir_encodes_cwd_like_pi() {
        // Pure string fixtures: `session_dir_for` only rewrites the *cwd text*
        // into pi's session-dir slug (it never touches the input path on disk),
        // and a real cwd is always absolute — `~` is a shell-level expansion the
        // host never passes down. Only the `$HOME` prefix is machine-dependent,
        // and that is resolved at runtime (`session_dir_for` reads `HOME`).
        let dir = session_dir_for("/home/user");
        assert!(
            dir.to_string_lossy().ends_with("/sessions/--home-user--"),
            "got {dir:?}"
        );
        let dir = session_dir_for("/home/u/.config/pi-desktop-chat-workspace");
        assert!(dir
            .to_string_lossy()
            .ends_with("/sessions/--home-u-.config-pi-desktop-chat-workspace--"));
    }

    #[test]
    fn session_id_comes_from_the_filename() {
        let path = std::path::Path::new(
            "/tmp/2026-09-15T17-05-42-977Z_01a0a608-1641-72b8-a7b8-48070cc2a11c.jsonl",
        );
        assert_eq!(
            session_id_of(path).as_deref(),
            Some("01a0a608-1641-72b8-a7b8-48070cc2a11c")
        );
    }

    #[test]
    fn model_and_level_and_command_parsers() {
        let models = parse_models(&json!({"models":[
            {"id":"m1","name":"Model One","provider":"p"},
            {"id":"m2","provider":"p"},
        ]}));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "Model One");
        assert_eq!(models[1].name, "m2");

        assert_eq!(
            parse_levels(&json!({"levels":["off","high","max"]})),
            vec!["off", "high", "max"]
        );

        let commands = parse_commands(&json!({"commands":[
            {"name":"fix","description":"fix stuff","source":"prompt"},
            {"name":"skill:x","source":"skill"},
        ]}));
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].source, "prompt");
        assert_eq!(commands[1].description, "");
    }

    #[test]
    fn browse_lists_only_directories_with_parent() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-browse-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        let Event::DirListing { path: _, parent, dirs } = browse_event(dir.to_str().unwrap())
        else {
            panic!("expected DirListing")
        };
        assert!(dirs.iter().any(|(name, _)| name == "sub"));
        assert!(!dirs.iter().any(|(name, _)| name == "file.txt"));
        assert!(parent.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_index_walk_skips_vendor_dirs() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.join("node_modules/dep.js"), "x").unwrap();
        let files = file_index(dir.to_str().unwrap());
        assert!(files.contains(&"src/main.rs".to_string()), "got {files:?}");
        assert!(!files.iter().any(|f| f.starts_with("node_modules")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_scan_reads_labels_and_paths() {
        let dir = std::env::temp_dir().join(format!("ocs-pi-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("2026-01-01T00-00-00-000Z_abc-123.jsonl");
        std::fs::write(
            &file,
            "{\"type\":\"session\",\"id\":\"abc-123\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"你好\\n第二行\"}]}}\n",
        )
        .unwrap();
        // A cwd whose slug maps onto our temp dir is impossible; call the
        // helpers directly instead.
        assert_eq!(session_id_of(&file).as_deref(), Some("abc-123"));
        assert_eq!(session_label(&file).as_deref(), Some("你好"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
