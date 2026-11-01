//! Claude Code integration: hooks in ~/.claude/settings.json POST each event to
//! a tiny HTTP endpoint on localhost, and Clawd acts out what the session is doing.

use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::{
    claude::{self, Haiku},
    settings::Lang,
};

pub const PORT: u16 = 47823;
const MARKER: &str = "127.0.0.1:47823/hook/";
const STATUS_MARKER: &str = "127.0.0.1:47823/status/";
const PERMISSION_MARKER: &str = "127.0.0.1:47823/permission/";
/// How long Clawd's card waits for a click before Claude Code asks the usual way.
/// Claude Code holds the question while the hook runs, so this is how long a user who
/// isn't looking at Clawd waits for the prompt in the terminal: enough to read the card
/// and click, short enough not to stall a session.
const PERMISSION_WAIT: Duration = Duration::from_secs(25);
const EVENTS: [&str; 7] =
    ["UserPromptSubmit", "PreToolUse", "Stop", "Notification", "SessionEnd", "SubagentStop", "StopFailure"];
const MAX_BODY: usize = 8 << 20;
const TRANSCRIPT_TAIL: u64 = 256 << 10;
const THOUGHT_EVERY: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ClaudeEvent {
    Start { session: String, project: String },
    /// `background` is a shell command Claude started with run_in_background.
    Tool { session: String, project: String, tool: String, detail: String, background: bool },
    Thought { session: String, text: String },
    Attention { session: String, project: String, message: String },
    Stop { session: String, project: String },
    Report { session: String, project: String, text: String },
    End { session: String },
    /// A subagent (Explore, Plan, ...) finished; the main session goes on.
    Subagent { session: String, project: String, agent: String },
    /// The turn ended because of an API error: rate limit, login, overload...
    Failure { session: String, project: String, error: String },
    /// Plan usage from the status line.
    Limits { five_hour: Option<Window>, seven_day: Option<Window> },
    /// Claude Code wants to use a tool and asks the user: Clawd shows Allow / Deny.
    Permission { id: u64, project: String, tool: String, detail: String, description: String },
    /// That question was answered or timed out; the card goes away.
    PermissionDone { id: u64 },
}

/// What a permission request is about: the full command, file or address.
fn permission_detail(tool: &str, input: &Value) -> String {
    let detail = match tool {
        "Bash" | "PowerShell" => str_field(input, "command").to_string(),
        "Read" | "Edit" | "MultiEdit" | "Write" => str_field(input, "file_path").to_string(),
        "NotebookEdit" => str_field(input, "notebook_path").to_string(),
        "WebFetch" => str_field(input, "url").to_string(),
        "WebSearch" => str_field(input, "query").to_string(),
        "Grep" | "Glob" => str_field(input, "pattern").to_string(),
        _ => input.to_string(),
    };
    let detail = detail.trim();
    match detail.char_indices().nth(600) {
        Some((i, _)) => format!("{}…", &detail[..i]),
        None => detail.to_string(),
    }
}

/// The hook's answer to Claude Code: allow, or deny with a note for the model.
fn permission_reply(allow: bool) -> Value {
    let decision = if allow {
        json!({ "behavior": "allow" })
    } else {
        json!({ "behavior": "deny", "message": "The user denied this from Clawd, the desktop pet." })
    };
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision } })
}

/// One rate-limit window: percent used and when it resets (unix seconds).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Window {
    pub used: f64,
    pub resets_at: Option<u64>,
    /// When it resets as `/usage` words it ("Oct 26, 12:20am"), if there's no timestamp.
    pub resets: Option<String>,
}

fn limit_window(v: &Value) -> Option<Window> {
    let used = v.get("used_percentage")?.as_f64()?;
    let resets_at = v.get("resets_at").and_then(|r| r.as_u64().or_else(|| r.as_f64().map(|f| f as u64)));
    Some(Window { used, resets_at, resets: None })
}

/// One "Current …: 67% used · resets Oct 26, 12:20am (Europe/Moscow)" line of `/usage`.
fn usage_line(text: &str, label: &str) -> Option<Window> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with(label))?;
    let rest = line[label.len()..].trim_start_matches(':').trim();
    let used: f64 = rest.split('%').next()?.trim().parse().ok()?;
    let resets = rest
        .split_once("resets")
        .map(|(_, r)| r.trim())
        .map(|r| r.split(" (").next().unwrap_or(r).trim().to_string())
        .filter(|r| !r.is_empty());
    Some(Window { used, resets_at: None, resets })
}

/// Reads the plan limits out of `claude -p /usage` output.
pub fn parse_usage(text: &str) -> Option<ClaudeEvent> {
    let five_hour = usage_line(text, "Current session");
    let seven_day = usage_line(text, "Current week (all models)").or_else(|| usage_line(text, "Current week"));
    (five_hour.is_some() || seven_day.is_some()).then_some(ClaudeEvent::Limits { five_hour, seven_day })
}

/// Reads `rate_limits` from Claude Code's status line input.
pub fn parse_limits(body: &Value) -> Option<ClaudeEvent> {
    let limits = body.get("rate_limits")?;
    let five_hour = limits.get("five_hour").and_then(limit_window);
    let seven_day = limits.get("seven_day").and_then(limit_window);
    (five_hour.is_some() || seven_day.is_some()).then_some(ClaudeEvent::Limits { five_hour, seven_day })
}

struct Parsed {
    event: ClaudeEvent,
    transcript: Option<PathBuf>,
    last_message: Option<String>,
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

/// Claude Code's shell tools: Bash everywhere, PowerShell on Windows.
pub fn is_shell(tool: &str) -> bool {
    matches!(tool, "Bash" | "PowerShell")
}

/// Short human description of what a tool call is about.
fn tool_detail(tool: &str, input: &Value) -> String {
    let detail = match tool {
        "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let path = str_field(input, "file_path");
            let path = if path.is_empty() { str_field(input, "notebook_path") } else { path };
            file_name(path)
        }
        "Bash" | "PowerShell" => str_field(input, "command").to_string(),
        "Grep" | "Glob" => str_field(input, "pattern").to_string(),
        "WebSearch" => str_field(input, "query").to_string(),
        "WebFetch" => {
            let url = str_field(input, "url");
            url.split('/').nth(2).unwrap_or(url).to_string()
        }
        "Task" | "Agent" => str_field(input, "description").to_string(),
        _ => String::new(),
    };
    // Commands are shown as a terminal block, so they can be longer than a file name.
    clip(&detail, if is_shell(tool) { 160 } else { 40 })
}

fn parse(body: &Value, internal: &Path) -> Option<Parsed> {
    let cwd = str_field(body, "cwd");
    // Clawd's own Haiku calls run in `internal`; never react to those.
    if !cwd.is_empty() && Path::new(cwd).starts_with(internal) {
        return None;
    }
    let session = str_field(body, "session_id").to_string();
    let project = file_name(cwd);
    let event = match str_field(body, "hook_event_name") {
        "UserPromptSubmit" => ClaudeEvent::Start { session, project },
        "PreToolUse" => {
            let tool = str_field(body, "tool_name").to_string();
            let input = body.get("tool_input").unwrap_or(&Value::Null);
            let detail = tool_detail(&tool, input);
            let background = input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
            ClaudeEvent::Tool { session, project, tool, detail, background }
        }
        "Notification" => ClaudeEvent::Attention {
            session,
            project,
            message: clip(str_field(body, "message"), 80),
        },
        "Stop" => ClaudeEvent::Stop { session, project },
        "SessionEnd" => ClaudeEvent::End { session },
        "SubagentStop" => ClaudeEvent::Subagent { session, project, agent: str_field(body, "agent_type").to_string() },
        "StopFailure" => {
            // `error` is a short code like "rate_limit"; be lenient about its shape.
            let error = match body.get("error") {
                Some(Value::String(code)) => code.clone(),
                Some(other) => str_field(other, "type").to_string(),
                None => String::new(),
            };
            ClaudeEvent::Failure { session, project, error }
        }
        _ => return None,
    };
    let transcript = Some(str_field(body, "transcript_path"))
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let last_message = Some(str_field(body, "last_assistant_message"))
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string);
    Some(Parsed { event, transcript, last_message })
}

/// The text of the most recent assistant message in a session transcript (JSONL).
pub fn last_assistant_text(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TRANSCRIPT_TAIL))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let tail = String::from_utf8_lossy(&buf);
    tail.lines().rev().find_map(|line| {
        let entry: Value = serde_json::from_str(line).ok()?;
        if str_field(&entry, "type") != "assistant" {
            return None;
        }
        let text: Vec<&str> = entry
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .filter(|block| str_field(block, "type") == "text")
            .map(|block| str_field(block, "text"))
            .filter(|t| !t.trim().is_empty())
            .collect();
        (!text.is_empty()).then(|| text.join("\n"))
    })
}

/// First sentence of a message, cleaned of markdown, for when Haiku isn't available.
fn first_sentence(text: &str, max: usize) -> String {
    let plain: String = text.chars().filter(|c| !matches!(c, '#' | '*' | '`' | '_' | '>')).collect();
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    // A sentence ends at . ! ? followed by a space, so "main.rs" stays whole.
    let end = plain
        .char_indices()
        .enumerate()
        .find(|&(n, (i, c))| {
            n >= 10 && matches!(c, '.' | '!' | '?') && plain[i + c.len_utf8()..].starts_with(' ')
        })
        .map(|(_, (i, c))| i + c.len_utf8())
        .unwrap_or(plain.len());
    clip(&plain[..end], max)
}

#[derive(Default)]
struct Session {
    last_text: String,
    last_thought: Option<Instant>,
    thinking: bool,
}

pub struct Hub {
    haiku: Arc<Haiku>,
    sessions: Mutex<HashMap<String, Session>>,
    emit: Box<dyn Fn(ClaudeEvent) + Send + Sync>,
    /// Whether Claude Code's permission questions go to Clawd's card at all.
    pub ask_permissions: AtomicBool,
    pending: Mutex<HashMap<u64, mpsc::Sender<bool>>>,
    next_id: AtomicU64,
}

impl Hub {
    pub fn new(haiku: Arc<Haiku>, ask_permissions: bool, emit: impl Fn(ClaudeEvent) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            haiku,
            sessions: Mutex::new(HashMap::new()),
            emit: Box::new(emit),
            ask_permissions: AtomicBool::new(ask_permissions),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    /// Shows Clawd's Allow / Deny card and waits for a click. None means "no answer":
    /// switched off, our own Haiku call, or nobody clicked in time.
    fn ask_permission(&self, body: &Value) -> Option<bool> {
        if !self.ask_permissions.load(Ordering::Relaxed) {
            return None;
        }
        let cwd = str_field(body, "cwd");
        if !cwd.is_empty() && Path::new(cwd).starts_with(&self.haiku.workdir) {
            return None;
        }
        let tool = str_field(body, "tool_name").to_string();
        let input = body.get("tool_input").unwrap_or(&Value::Null);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (self.emit)(ClaudeEvent::Permission {
            id,
            project: file_name(cwd),
            detail: permission_detail(&tool, input),
            description: clip(str_field(input, "description"), 120),
            tool,
        });
        let answer = rx.recv_timeout(PERMISSION_WAIT).ok();
        self.pending.lock().unwrap().remove(&id);
        (self.emit)(ClaudeEvent::PermissionDone { id });
        answer
    }

    /// The user clicked Allow or Deny on card `id`.
    pub fn answer_permission(&self, id: u64, allow: bool) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(allow);
        }
    }

    /// Runs Haiku if it's available and enabled; None means "fall back to plain text".
    fn haiku(&self, prompt: impl FnOnce(Lang) -> String, max: usize) -> Option<String> {
        match self.haiku.run(prompt) {
            Ok(out) => claude::phrases(&out, max).into_iter().next(),
            Err(e) => {
                eprintln!("haiku: {e}");
                None
            }
        }
    }

    /// Plan limits read some other way (`claude -p /usage`), passed on like the status line's.
    pub fn limits(&self, event: ClaudeEvent) {
        (self.emit)(event);
    }

    /// Status line updates carry the plan's usage limits.
    pub fn status(&self, body: &Value) {
        if let Some(limits) = parse_limits(body) {
            (self.emit)(limits);
        }
    }

    pub fn handle(self: &Arc<Self>, body: &Value) {
        let Some(parsed) = parse(body, &self.haiku.workdir) else {
            return;
        };
        let latest = || {
            parsed
                .last_message
                .clone()
                .or_else(|| parsed.transcript.as_deref().and_then(last_assistant_text))
        };
        match &parsed.event {
            ClaudeEvent::Start { session, .. } => {
                // Remember the previous answer so it isn't mistaken for a new thought.
                let text = parsed.transcript.as_deref().and_then(last_assistant_text).unwrap_or_default();
                self.sessions.lock().unwrap().entry(session.clone()).or_default().last_text = text;
                (self.emit)(parsed.event.clone());
            }
            ClaudeEvent::Tool { session, .. } => {
                (self.emit)(parsed.event.clone());
                let Some(text) = parsed.transcript.as_deref().and_then(last_assistant_text) else {
                    return;
                };
                {
                    let mut sessions = self.sessions.lock().unwrap();
                    let state = sessions.entry(session.clone()).or_default();
                    let due = state.last_thought.is_none_or(|t| t.elapsed() >= THOUGHT_EVERY);
                    if text == state.last_text || state.thinking || !due {
                        return;
                    }
                    state.last_text = text.clone();
                    state.thinking = true;
                    state.last_thought = Some(Instant::now());
                }
                let hub = self.clone();
                let session = session.clone();
                thread::spawn(move || {
                    let thought = hub
                        .haiku(|lang| claude::thought_prompt(lang, &clip(&text, 1500)), 60)
                        .unwrap_or_else(|| first_sentence(&text, 60));
                    if let Some(state) = hub.sessions.lock().unwrap().get_mut(&session) {
                        state.thinking = false;
                    }
                    (hub.emit)(ClaudeEvent::Thought { session, text: thought });
                });
            }
            ClaudeEvent::Stop { session, project } => {
                (self.emit)(parsed.event.clone());
                let Some(text) = latest() else {
                    return;
                };
                let hub = self.clone();
                let (session, project) = (session.clone(), project.clone());
                thread::spawn(move || {
                    let report = hub
                        .haiku(|lang| claude::done_prompt(lang, &project, &clip(&text, 2000)), 80)
                        .unwrap_or_else(|| first_sentence(&text, 70));
                    (hub.emit)(ClaudeEvent::Report { session, project, text: report });
                });
            }
            ClaudeEvent::End { session } => {
                self.sessions.lock().unwrap().remove(session);
                (self.emit)(parsed.event.clone());
            }
            _ => (self.emit)(parsed.event.clone()),
        }
    }
}

/// Reads one HTTP request; returns its body if it is `POST /hook/<token>`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Hook,
    Status,
    Permission,
}

fn read_request(stream: &mut TcpStream, token: &str) -> Option<(Route, Vec<u8>)> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 << 10 {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut request = lines.next()?.split_whitespace();
    let (method, path) = (request.next()?, request.next()?);
    let route = match path.strip_suffix(token) {
        Some("/hook/") => Route::Hook,
        Some("/status/") => Route::Status,
        Some("/permission/") => Route::Permission,
        _ => return None,
    };
    if method != "POST" {
        return None;
    }
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())?;
    if length > MAX_BODY {
        return None;
    }
    let mut body = buf.split_off(header_end);
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Some((route, body))
}

pub fn serve(token: String, hub: Arc<Hub>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", PORT))?;
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream, &token);
            // A permission question keeps the connection open until the user answers,
            // so it gets its own thread; curl prints the reply for Claude Code to read.
            if let Some((Route::Permission, body)) = &request {
                let (hub, body) = (hub.clone(), body.clone());
                thread::spawn(move || {
                    let answer = serde_json::from_slice::<Value>(&body).ok().and_then(|v| hub.ask_permission(&v));
                    let reply = answer.map(|allow| permission_reply(allow).to_string()).unwrap_or_default();
                    let status = if reply.is_empty() { "204 No Content" } else { "200 OK" };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                });
                continue;
            }
            let status = if request.is_some() { "204 No Content" } else { "404 Not Found" };
            let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            drop(stream);
            let Some((route, body)) = request else { continue };
            let Ok(value) = serde_json::from_slice::<Value>(&body) else { continue };
            match route {
                Route::Hook => hub.handle(&value),
                Route::Status => hub.status(&value),
                Route::Permission => {}
            }
        }
    });
    Ok(())
}

/// A per-install secret in the hook URL, so web pages can't make Clawd talk.
pub fn load_token(dir: &Path) -> String {
    let path = dir.join("hook-token");
    if let Ok(token) = fs::read_to_string(&path) {
        let token = token.trim();
        if token.len() >= 16 && token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return token.to_string();
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::new();
    for i in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id() ^ i);
        token.push_str(&format!("{:016x}", h.finish()));
    }
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(&path, &token);
    token
}

pub fn settings_path() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| claude::home_dir().map(|h| h.join(".claude")))?;
    Some(dir.join("settings.json"))
}

fn hook_command(token: &str) -> String {
    format!(
        "curl -s -m 2 -o /dev/null -H Content-Type:application/json --data-binary @- http://{MARKER}{token} || true"
    )
}

/// Waits for the answer from Clawd's card and prints it for Claude Code. When Clawd
/// isn't running or nobody clicks, it prints nothing and Claude Code asks as usual.
fn permission_command(token: &str) -> String {
    let wait = PERMISSION_WAIT.as_secs() + 5;
    format!("curl -s -m {wait} -H Content-Type:application/json --data-binary @- http://{PERMISSION_MARKER}{token} || true")
}

/// The status line hands its input (which has the plan limits) to Clawd and then
/// on to the user's own status line command, if they had one.
/// How the wrapper hands the status line input on to the user's own command.
const WRAPPED_PREFIX: &str = "printf '%s' \"$input\" | { ";
const WRAPPED_SUFFIX: &str = "\n}";

fn status_command(token: &str, original: Option<&str>) -> String {
    let post =
        format!("curl -s -m 1 -o /dev/null -H Content-Type:application/json --data-binary @- http://{STATUS_MARKER}{token}");
    match original {
        Some(cmd) => format!("input=$(cat); printf '%s' \"$input\" | {post}; {WRAPPED_PREFIX}{cmd}{WRAPPED_SUFFIX}"),
        None => format!("{post} || true"),
    }
}

/// The user's own status line, rebuilt from Clawd's wrapper around it: for when the
/// backup in Clawd's settings is gone. None when the wrapper wraps nothing.
fn unwrap_status(line: &Value) -> Option<Value> {
    let command = str_field(line, "command");
    let after_post = &command[command.find(STATUS_MARKER)?..];
    let start = after_post.find(WRAPPED_PREFIX)? + WRAPPED_PREFIX.len();
    let original = after_post[start..].strip_suffix(WRAPPED_SUFFIX)?;
    let mut users = line.clone();
    users["command"] = json!(original);
    Some(users)
}

fn is_clawd_status(line: &Value) -> bool {
    str_field(line, "command").contains(STATUS_MARKER)
}

/// Puts Clawd's status line in, keeping the user's own as a backup, or puts theirs
/// back. Returns the backup to keep.
fn with_status_line(settings: &mut Map<String, Value>, token: Option<&str>, backup: Option<Value>) -> Option<Value> {
    let users = match settings.get("statusLine").cloned() {
        Some(line) if is_clawd_status(&line) => backup.or_else(|| unwrap_status(&line)),
        other => other,
    };
    let Some(token) = token else {
        match users {
            Some(line) => settings.insert("statusLine".into(), line),
            None => settings.remove("statusLine"),
        };
        return None;
    };
    let original = users
        .as_ref()
        .filter(|l| str_field(l, "type") == "command")
        .map(|l| str_field(l, "command"))
        .filter(|c| !c.is_empty());
    let mut line = users.clone().filter(Value::is_object).unwrap_or_else(|| json!({}));
    line["type"] = json!("command");
    line["command"] = json!(status_command(token, original));
    settings.insert("statusLine".into(), line);
    users
}

fn is_clawd_group(group: &Value) -> bool {
    group.get("hooks").and_then(Value::as_array).is_some_and(|hooks| {
        hooks.iter().any(|h| {
            let command = str_field(h, "command");
            command.contains(MARKER) || command.contains(PERMISSION_MARKER)
        })
    })
}

/// Removes Clawd's hooks from settings and, given the token, adds fresh ones.
fn with_hooks(mut settings: Value, token: Option<&str>) -> Result<Value, String> {
    let root = settings.as_object_mut().ok_or("settings.json is not a JSON object")?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or("\"hooks\" in settings.json is not an object")?;
    for event in EVENTS.iter().copied().chain(["PermissionRequest"]) {
        let groups = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
        let groups = groups.as_array_mut().ok_or(format!("hooks.{event} is not an array"))?;
        groups.retain(|g| !is_clawd_group(g));
        if let Some(token) = token {
            let hook = if event == "PermissionRequest" {
                json!({ "type": "command", "command": permission_command(token), "timeout": PERMISSION_WAIT.as_secs() + 10 })
            } else {
                json!({ "type": "command", "command": hook_command(token), "timeout": 5 })
            };
            let group = if event == "PreToolUse" || event == "PermissionRequest" {
                json!({ "matcher": "*", "hooks": [hook] })
            } else {
                json!({ "hooks": [hook] })
            };
            groups.push(group);
        }
        if groups.is_empty() {
            hooks.remove(event);
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(settings)
}

pub fn is_installed(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|s| s.contains(MARKER))
}

/// Adds (with a token) or removes Clawd's hooks and status line, keeping everything
/// else intact. `backup` is the user's own status line saved by an earlier install;
/// returns the one to keep now. An unchanged file isn't rewritten.
pub fn set_installed(path: &Path, token: Option<&str>, backup: Option<Value>) -> Result<Option<Value>, String> {
    let original = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let settings = match &original {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).map_err(|e| format!("settings.json is not valid JSON: {e}"))?
        }
        _ => Value::Object(Map::new()),
    };
    let mut updated = with_hooks(settings, token)?;
    let root = updated.as_object_mut().ok_or("settings.json is not a JSON object")?;
    let backup = with_status_line(root, token, backup);
    let pretty = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())? + "\n";
    if original.as_deref() == Some(pretty.as_str()) {
        return Ok(backup);
    }
    if let Some(text) = &original {
        let backup = path.with_extension("json.clawd-backup");
        if !backup.exists() {
            fs::write(&backup, text).map_err(|e| e.to_string())?;
        }
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.clawd-tmp");
    fs::write(&tmp, pretty).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clawd-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_keeps_user_settings_and_uninstall_restores_them() {
        let user = json!({
            "model": "opus",
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send done" }] }] }
        });
        let installed = with_hooks(user.clone(), Some("abc")).unwrap();
        assert_eq!(installed["model"], "opus");
        assert_eq!(installed["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(installed["hooks"]["PreToolUse"][0]["matcher"], "*");
        // Reinstalling replaces rather than duplicates.
        assert!(str_field(&installed["hooks"]["PermissionRequest"][0]["hooks"][0], "command").contains("/permission/abc"));
        let again = with_hooks(installed, Some("def")).unwrap();
        assert_eq!(again["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(again.to_string().contains("/hook/def") && !again.to_string().contains("/hook/abc"));
        assert_eq!(with_hooks(again, None).unwrap(), user);
        assert_eq!(with_hooks(json!({}), None).unwrap(), json!({}));
    }

    #[test]
    fn refuses_to_touch_broken_settings() {
        let dir = temp_dir("broken");
        let path = dir.join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(set_installed(&path, Some("abc"), None).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(!dir.join("settings.json.clawd-backup").exists());
    }

    #[test]
    fn install_writes_backup_and_detects_itself() {
        let dir = temp_dir("install");
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(set_installed(&path, Some("tok123"), None).unwrap(), None);
        assert!(is_installed(&path));
        assert_eq!(fs::read_to_string(dir.join("settings.json.clawd-backup")).unwrap(), r#"{"theme":"dark"}"#);
        set_installed(&path, None, None).unwrap();
        assert!(!is_installed(&path));
        let restored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(restored, json!({"theme": "dark"}));
    }

    #[test]
    fn parses_hook_events() {
        let internal = Path::new("/home/u/.local/share/clawd/haiku");
        let tool = json!({
            "hook_event_name": "PreToolUse", "session_id": "s1", "cwd": "/home/u/proj",
            "tool_name": "Edit", "tool_input": { "file_path": "/home/u/proj/src/main.rs" }
        });
        let parsed = parse(&tool, internal).unwrap();
        assert_eq!(
            parsed.event,
            ClaudeEvent::Tool {
                session: "s1".into(),
                project: "proj".into(),
                tool: "Edit".into(),
                detail: "main.rs".into(),
                background: false
            }
        );
        let own = json!({ "hook_event_name": "Stop", "cwd": "/home/u/.local/share/clawd/haiku" });
        assert!(parse(&own, internal).is_none());
        let bash = json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": { "command": "cargo   test\n --all" } });
        assert!(matches!(parse(&bash, internal).unwrap().event, ClaudeEvent::Tool { detail, .. } if detail == "cargo test --all"));
        let ps = json!({
            "hook_event_name": "PreToolUse", "tool_name": "PowerShell",
            "tool_input": { "command": "Get-Date", "run_in_background": true }
        });
        assert!(matches!(
            parse(&ps, internal).unwrap().event,
            ClaudeEvent::Tool { detail, background: true, .. } if detail == "Get-Date"
        ));
    }

    #[test]
    fn parses_subagents_failures_and_limits() {
        let internal = Path::new("/x/haiku");
        let sub = json!({ "hook_event_name": "SubagentStop", "session_id": "s", "cwd": "/p/app", "agent_type": "Explore" });
        assert_eq!(
            parse(&sub, internal).unwrap().event,
            ClaudeEvent::Subagent { session: "s".into(), project: "app".into(), agent: "Explore".into() }
        );
        let fail = json!({ "hook_event_name": "StopFailure", "session_id": "s", "cwd": "/p/app", "error": "rate_limit" });
        assert!(matches!(parse(&fail, internal).unwrap().event, ClaudeEvent::Failure { error, .. } if error == "rate_limit"));
        let status = json!({ "rate_limits": {
            "five_hour": { "used_percentage": 23.5, "resets_at": 1767225600 },
            "seven_day": { "used_percentage": 41 }
        }});
        assert_eq!(
            parse_limits(&status),
            Some(ClaudeEvent::Limits {
                five_hour: Some(Window { used: 23.5, resets_at: Some(1767225600), resets: None }),
                seven_day: Some(Window { used: 41.0, resets_at: None, resets: None }),
            })
        );
        assert_eq!(parse_limits(&json!({ "model": {} })), None);
    }

    #[test]
    fn reads_limits_from_usage_output() {
        let text = "You are currently using your subscription to power your Claude Code usage\n\n\
                    Current session: 67% used · resets Oct 26, 12:20am (Europe/Moscow)\n\
                    Current week (all models): 80.5% used · resets Oct 30, 11pm (Europe/Moscow)\n\
                    Current week (Sonnet only): 12% used\n";
        assert_eq!(
            parse_usage(text),
            Some(ClaudeEvent::Limits {
                five_hour: Some(Window { used: 67.0, resets_at: None, resets: Some("Oct 26, 12:20am".into()) }),
                seven_day: Some(Window { used: 80.5, resets_at: None, resets: Some("Oct 30, 11pm".into()) }),
            })
        );
        assert_eq!(parse_usage("Not logged in"), None);
    }

    #[test]
    fn permission_replies_match_claude_codes_format() {
        assert_eq!(permission_reply(true)["hookSpecificOutput"]["decision"]["behavior"], "allow");
        let deny = permission_reply(false);
        assert_eq!(deny["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(permission_detail("Bash", &json!({ "command": "  rm -rf build  " })), "rm -rf build");
        assert_eq!(permission_detail("Edit", &json!({ "file_path": "/p/main.rs" })), "/p/main.rs");
    }

    #[test]
    fn permission_waits_for_the_click() {
        let (seen, sink) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
        let haiku = Arc::new(Haiku {
            workdir: PathBuf::from("/nowhere"),
            cli: std::sync::RwLock::new(None),
            lang: std::sync::RwLock::new(Lang::En),
            enabled: AtomicBool::new(false),
        });
        let (log, slot) = (seen.clone(), sink.clone());
        let hub = Hub::new(haiku, true, move |e| {
            if let ClaudeEvent::Permission { id, .. } = &e {
                *slot.lock().unwrap() = Some(*id);
            }
            log.lock().unwrap().push(e);
        });
        let asker = hub.clone();
        let waiting = thread::spawn(move || {
            asker.ask_permission(&json!({ "tool_name": "Bash", "tool_input": { "command": "ls" }, "cwd": "/p/app" }))
        });
        let id = loop {
            if let Some(id) = *sink.lock().unwrap() {
                break id;
            }
            thread::sleep(Duration::from_millis(5));
        };
        hub.answer_permission(id, false);
        assert_eq!(waiting.join().unwrap(), Some(false));
        assert!(matches!(seen.lock().unwrap().last(), Some(ClaudeEvent::PermissionDone { .. })));
        hub.ask_permissions.store(false, Ordering::Relaxed);
        assert_eq!(hub.ask_permission(&json!({ "tool_name": "Bash" })), None);
    }

    #[test]
    fn status_line_wraps_the_users_own_and_gives_it_back() {
        let users = json!({ "type": "command", "command": "~/bin/line.sh", "padding": 0 });
        let mut settings = json!({ "statusLine": users.clone() });
        let root = settings.as_object_mut().unwrap();
        let backup = with_status_line(root, Some("tok"), None);
        assert_eq!(backup, Some(users.clone()));
        let command = str_field(&root["statusLine"], "command").to_string();
        assert!(command.contains("/status/tok") && command.contains("~/bin/line.sh"));
        assert_eq!(root["statusLine"]["padding"], 0);
        // Installing again keeps the user's line, not a wrapper around the wrapper.
        let again = with_status_line(root, Some("tok"), backup.clone());
        assert_eq!(again, Some(users.clone()));
        assert_eq!(str_field(&root["statusLine"], "command").matches("/status/").count(), 1);
        assert_eq!(with_status_line(root, None, again), None);
        assert_eq!(root["statusLine"], users);

        // Clawd's own settings lost the backup: the user's line comes back out of the wrapper.
        let mut wrapped = json!({ "statusLine": users.clone() });
        let root = wrapped.as_object_mut().unwrap();
        with_status_line(root, Some("tok"), None);
        assert_eq!(with_status_line(root, Some("tok2"), None), Some(users.clone()));
        assert!(str_field(&root["statusLine"], "command").contains("~/bin/line.sh"));
        assert_eq!(with_status_line(root, None, None), None);
        assert_eq!(root["statusLine"], users);

        let mut empty = json!({});
        let root = empty.as_object_mut().unwrap();
        assert_eq!(with_status_line(root, Some("tok"), None), None);
        assert!(str_field(&root["statusLine"], "command").ends_with("|| true"));
        with_status_line(root, None, None);
        assert!(root.is_empty());
    }

    #[test]
    fn reads_last_assistant_text_from_transcript() {
        let dir = temp_dir("transcript");
        let path = dir.join("t.jsonl");
        let lines = [
            json!({"type": "user", "message": {"role": "user", "content": "fix it"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Сначала посмотрю тесты."}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
        ];
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        fs::write(&path, text.join("\n")).unwrap();
        assert_eq!(last_assistant_text(&path).as_deref(), Some("Сначала посмотрю тесты."));
    }

    #[test]
    fn first_sentence_is_short_and_plain() {
        assert_eq!(first_sentence("**Готово!** Я починил тесты. Ещё...", 60), "Готово! Я починил тесты.");
        assert_eq!(first_sentence("## Итог: всё работает", 60), "Итог: всё работает");
        assert_eq!(first_sentence("Исправил баг в `main.rs`. Проверь.", 60), "Исправил баг в main.rs.");
    }

    #[test]
    fn reads_requests_with_the_right_token_only() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let send = |path: &'static str| {
            thread::spawn(move || {
                let mut s = TcpStream::connect(addr).unwrap();
                let body = r#"{"a":1}"#;
                write!(s, "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            })
        };
        send("/hook/secret");
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_request(&mut stream, "secret").is_some_and(|(r, b)| r == Route::Hook && b == br#"{"a":1}"#));
        send("/status/secret");
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_request(&mut stream, "secret").is_some_and(|(r, _)| r == Route::Status));
        send("/hook/wrong");
        let (mut stream, _) = listener.accept().unwrap();
        assert!(read_request(&mut stream, "secret").is_none());
    }
}
