#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod claude;
mod context;
mod droppaths;
mod files;
mod hooks;
mod reminders;
mod screens;
mod settings;
mod shell;
mod tray;
mod update;

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
    thread,
    time::Duration,
};

use serde::Serialize;
use tauri::{Emitter, Manager, State, WebviewWindow};
use tauri_plugin_notification::NotificationExt;

use claude::Haiku;
use context::ContextEvent;
use hooks::ClaudeEvent;
use settings::{Lang, Memory, Settings, Vitals};

pub const WINDOW: &str = "pet";
const CURSOR_POLL: Duration = Duration::from_millis(16);
const REFIT_EVERY: u32 = 120;
const MAX_FACTS: usize = 8;
/// How often Clawd asks `claude -p /usage` for the plan limits on his own.
const USAGE_EVERY: Duration = Duration::from_secs(5 * 60);
const MAX_RECENT: usize = 6;

#[derive(Clone, Copy, PartialEq, Serialize)]
struct Cursor {
    x: f64,
    y: f64,
}

/// The top edge of the focused window, in the overlay's CSS pixels: Clawd can sit on it.
#[derive(Clone, Copy, Debug, Serialize)]
struct Platform {
    x: f64,
    y: f64,
    w: f64,
}

#[derive(Clone, Debug, Serialize)]
struct Remind {
    text: String,
}

pub struct AppState {
    settings_path: PathBuf,
    pub settings: Mutex<Settings>,
    pub haiku: Arc<Haiku>,
    pub listening: Arc<AtomicBool>,
    pub watching: Arc<AtomicBool>,
    pub token: String,
    idle_busy: AtomicBool,
    /// The monitor the overlay is on; None means the primary one.
    monitor: Mutex<Option<String>>,
    pub tray: OnceLock<tray::Tray>,
    pub update: Mutex<Option<tauri_plugin_updater::Update>>,
    pub hub: OnceLock<Arc<hooks::Hub>>,
}

impl AppState {
    /// Changes the saved preferences and writes them to disk.
    pub fn update(&self, change: impl FnOnce(&mut Settings)) {
        let mut settings = self.settings.lock().unwrap();
        change(&mut settings);
        settings.save(&self.settings_path);
    }

    fn monitor(&self, window: &WebviewWindow) -> Option<tauri::Monitor> {
        screens::current(window, self.monitor.lock().unwrap().as_deref())
    }
}

#[tauri::command]
fn set_click_through(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    window
        .set_ignore_cursor_events(enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_language(state: State<'_, Arc<AppState>>) -> Lang {
    state.haiku.lang()
}

/// A batch of idle lines from Haiku for the pet's chatter.
#[tauri::command]
async fn claude_phrases(state: State<'_, Arc<AppState>>, context: String, count: u32) -> Result<Vec<String>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if state.idle_busy.swap(true, Ordering::AcqRel) {
            return Err("busy".to_string());
        }
        let result = state.haiku.run(|lang| claude::idle_prompt(lang, &context, count.clamp(1, 10)));
        state.idle_busy.store(false, Ordering::Release);
        result.map(|out| claude::phrases(&out, 50))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Clawd's answer to something the user typed into the chat box.
#[tauri::command]
async fn claude_chat(
    state: State<'_, Arc<AppState>>,
    context: String,
    history: Vec<[String; 2]>,
    message: String,
    files: Option<Vec<files::FedFile>>,
) -> Result<claude::ChatReply, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let facts = state.settings.lock().unwrap().memory.facts.clone();
        let fed = files::prompt_section(&files.unwrap_or_default());
        let out = state
            .haiku
            .run_web(|lang| claude::chat_prompt(lang, &context, &facts, &fed, &history, &message))?;
        claude::parse_chat(&out, 220).ok_or_else(|| "empty reply".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Runs a command Clawd offered in the chat. The page calls this only after
/// the user clicked "Run" on the confirmation panel.
#[tauri::command]
async fn run_command(command: String) -> Result<shell::CommandResult, String> {
    tauri::async_runtime::spawn_blocking(move || shell::run(&command))
        .await
        .map_err(|e| e.to_string())?
}

/// The overlay is created unfocused; typing into the chat box needs keyboard focus.
#[tauri::command]
fn focus_window(window: WebviewWindow) -> Result<(), String> {
    window.set_focus().map_err(|e| e.to_string())
}

/// Which sides of the current monitor lead to another one.
#[tauri::command]
fn monitor_layout(window: WebviewWindow, state: State<'_, Arc<AppState>>) -> Option<screens::Layout> {
    state.monitor(&window).map(|m| screens::layout(&window, &m))
}

/// Moves the overlay to the monitor beside the current one, when Clawd walks off
/// that side at height `y` (CSS pixels). None when there's no monitor there.
#[tauri::command]
fn cross_edge(window: WebviewWindow, state: State<'_, Arc<AppState>>, side: String, y: f64) -> Option<screens::Layout> {
    let from = state.monitor(&window)?;
    let origin = window.inner_position().ok()?;
    let scale = window.scale_factor().ok()?;
    let next = screens::neighbor(&window, &from, side == "right", f64::from(origin.y) + y * scale)?;
    *state.monitor.lock().unwrap() = Some(screens::key(&next));
    screens::fit(&window, &next).ok()?;
    // Crossing into a monitor with another scale makes Windows resize the window
    // on its own a moment later; fit it once more after that.
    let (w, m) = (window.clone(), next.clone());
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        let _ = screens::fit(&w, &m);
    });
    Some(screens::layout(&window, &next))
}

/// Moves the overlay to the monitor under the mouse ("Call Clawd" in the tray).
pub fn move_to_cursor_monitor(window: &WebviewWindow, state: &AppState) {
    let Ok(cursor) = window.cursor_position() else {
        return;
    };
    if let Some(monitor) = screens::at(window, cursor.x, cursor.y) {
        *state.monitor.lock().unwrap() = Some(screens::key(&monitor));
        let _ = screens::fit(window, &monitor);
    }
}

#[tauri::command]
fn get_vitals(state: State<'_, Arc<AppState>>) -> Option<Vitals> {
    state.settings.lock().unwrap().vitals
}

#[tauri::command]
fn save_vitals(state: State<'_, Arc<AppState>>, mood: f64, food: f64) {
    let (mood, food) = (mood.clamp(0.0, 100.0), food.clamp(0.0, 100.0));
    state.update(|s| s.vitals = Some(Vitals { mood, food, at: reminders::now() }));
    tray::set_vitals(&state, mood, food);
}

/// Sets a reminder; returns when it goes off (unix seconds).
#[tauri::command]
fn add_reminder(state: State<'_, Arc<AppState>>, minutes: f64, text: String) -> Result<u64, String> {
    let at = reminders::add(&state, minutes, &text)?;
    tray::refresh(&state);
    Ok(at)
}

#[tauri::command]
fn clear_reminders(state: State<'_, Arc<AppState>>) -> usize {
    let count = reminders::clear(&state);
    tray::refresh(&state);
    count
}

#[tauri::command]
fn get_memory(state: State<'_, Arc<AppState>>) -> Memory {
    state.settings.lock().unwrap().memory.clone()
}

/// Keeps a short fact for future chats; the oldest go when there are too many.
#[tauri::command]
fn remember(state: State<'_, Arc<AppState>>, fact: String) {
    let fact = fact.trim().to_string();
    if fact.is_empty() {
        return;
    }
    state.update(|s| {
        let facts = &mut s.memory.facts;
        facts.retain(|f| !f.eq_ignore_ascii_case(&fact));
        facts.push(fact);
        let extra = facts.len().saturating_sub(MAX_FACTS);
        facts.drain(..extra);
    });
}

/// The last few chat turns, so a restart doesn't cut the conversation off.
#[tauri::command]
fn save_recent(state: State<'_, Arc<AppState>>, recent: Vec<[String; 2]>) {
    let skip = recent.len().saturating_sub(MAX_RECENT);
    state.update(|s| s.memory.recent = recent.into_iter().skip(skip).collect());
}

/// Asks Claude Code for the plan limits (`/usage`) and passes them on as a limits event.
fn fetch_usage(state: &AppState) -> Option<ClaudeEvent> {
    let cli = state.haiku.cli()?;
    let text = claude::usage(&cli, &state.haiku.workdir).map_err(|e| eprintln!("usage: {e}")).ok()?;
    let event = hooks::parse_usage(&text)?;
    if let Some(hub) = state.hub.get() {
        hub.limits(event.clone());
    }
    Some(event)
}

/// The chat asked about limits: fetch fresh numbers now.
#[tauri::command]
async fn refresh_limits(state: State<'_, Arc<AppState>>) -> Result<Option<ClaudeEvent>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || fetch_usage(&state)).await.map_err(|e| e.to_string())
}

/// A system notification (for warnings that shouldn't be missed behind other windows).
#[tauri::command]
fn notify(app: tauri::AppHandle, text: String) {
    let _ = app.notification().builder().title("Clawd").body(text).show();
}

/// The user clicked Allow or Deny on a Claude Code permission card.
#[tauri::command]
fn answer_permission(state: State<'_, Arc<AppState>>, id: u64, allow: bool) {
    if let Some(hub) = state.hub.get() {
        hub.answer_permission(id, allow);
    }
}

#[tauri::command]
fn forget_memory(state: State<'_, Arc<AppState>>) {
    state.update(|s| s.memory = Memory::default());
}

fn main() {
    #[cfg(target_os = "linux")]
    prepare_linux_env();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Starting Clawd again just calls the one that's already running.
            if let Some(window) = app.get_webview_window(WINDOW) {
                let _ = window.show();
            }
            let _ = app.emit_to(WINDOW, "recall", ());
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            set_click_through,
            get_language,
            claude_phrases,
            claude_chat,
            run_command,
            focus_window,
            monitor_layout,
            cross_edge,
            get_vitals,
            save_vitals,
            add_reminder,
            clear_reminders,
            get_memory,
            remember,
            save_recent,
            forget_memory,
            answer_permission,
            refresh_limits,
            notify
        ])
        .setup(|app| {
            let window = app
                .get_webview_window(WINDOW)
                .expect("pet window is declared in tauri.conf.json");

            let data_dir = app.path().app_data_dir()?;
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            let settings = Settings::load(&settings_path);
            let workdir = data_dir.join("haiku");
            fs::create_dir_all(&workdir)?;
            // Claude Code reports cwd with symlinks resolved.
            #[cfg(unix)]
            let workdir = fs::canonicalize(&workdir).unwrap_or(workdir);

            let state = Arc::new(AppState {
                settings_path,
                haiku: Arc::new(Haiku {
                    workdir,
                    cli: RwLock::new(None),
                    lang: RwLock::new(settings.language),
                    enabled: AtomicBool::new(settings.llm),
                }),
                listening: Arc::new(AtomicBool::new(settings.listen)),
                watching: Arc::new(AtomicBool::new(settings.watch)),
                token: hooks::load_token(&data_dir),
                idle_busy: AtomicBool::new(false),
                settings: Mutex::new(settings),
                monitor: Mutex::new(None),
                tray: OnceLock::new(),
                update: Mutex::new(None),
                hub: OnceLock::new(),
            });
            app.manage(state.clone());

            if let Some(monitor) = state.monitor(&window) {
                screens::fit(&window, &monitor)?;
            }
            // On Linux the native window must be realized (shown) before click-through can be set.
            window.show()?;
            window.set_ignore_cursor_events(true)?;
            spawn_cursor_poller(window.clone(), state.clone());

            // Looking through the login shell can take a few seconds; don't hold up startup.
            let haiku = state.haiku.clone();
            let saved = state.settings.lock().unwrap().claude_path.clone();
            thread::spawn(move || {
                let found = claude::find_cli(saved.as_deref());
                eprintln!("claude: {}", found.as_deref().map_or("not found".into(), |p| p.display().to_string()));
                *haiku.cli.write().unwrap() = found;
            });

            let w = window.clone();
            audio::spawn(state.listening.clone(), move |e| send(&w, "audio", e));
            let w = window.clone();
            context::spawn(state.watching.clone(), move |e| match e {
                ContextEvent::Frame { rect } => send(&w, "platform", localize(&w, rect)),
                other => send(&w, "context", other),
            });
            let (w, s) = (window.clone(), state.clone());
            let ask = state.settings.lock().unwrap().ask_permissions;
            let hub = hooks::Hub::new(state.haiku.clone(), ask, move |e| {
                if let ClaudeEvent::Limits { five_hour, seven_day } = &e {
                    tray::set_limits(&s, five_hour.clone(), seven_day.clone());
                }
                // A permission question needs Clawd on screen, even if he was hidden.
                if matches!(e, ClaudeEvent::Permission { .. }) {
                    let _ = w.show();
                }
                send(&w, "claude", e);
            });
            let _ = state.hub.set(hub.clone());
            if let Err(e) = hooks::serve(state.token.clone(), hub) {
                eprintln!("Claude Code hook endpoint unavailable: {e}");
            }
            // Already linked: bring the hooks up to date with this version (new events),
            // without taking over anything the user didn't link. An unchanged
            // settings.json isn't touched.
            if hooks::settings_path().is_some_and(|p| hooks::is_installed(&p)) {
                if let Err(e) = tray::link_claude_code(&state, true, false) {
                    eprintln!("Claude Code hooks: {e}");
                }
            }

            // Paths and metadata of files dropped on Clawd (the page only sees names and contents).
            let w = window.clone();
            droppaths::listen(&window, move |paths| {
                let metas: Vec<droppaths::FileMeta> = paths.iter().filter_map(|p| droppaths::meta(p)).collect();
                send(&w, "file_meta", metas);
            });

            let (w, s) = (window.clone(), state.clone());
            let app_handle = app.handle().clone();
            reminders::spawn(state.clone(), move |r| {
                // A hidden Clawd comes back for this, and Windows shows a notification
                // (with its sound) in case he's behind a fullscreen window.
                let _ = w.show();
                send(&w, "remind", Remind { text: r.text.clone() });
                let _ = app_handle
                    .notification()
                    .builder()
                    .title("⏰ Clawd")
                    .body(if r.text.is_empty() { "⏰" } else { r.text.as_str() })
                    .show();
                tray::refresh(&s);
            });

            // The Claude app doesn't run Claude Code's status line, so ask `/usage` now and then.
            let s = state.clone();
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(20));
                fetch_usage(&s);
                thread::sleep(USAGE_EVERY);
            });

            tray::build(app, state.clone())?;
            update::spawn(app.handle().clone(), state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Clawd");
}

/// Forwards an event to the page; with CLAWD_DEBUG set, also logs it to stderr.
fn send<T: Serialize + Clone + std::fmt::Debug>(window: &WebviewWindow, event: &str, payload: T) {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DEBUG.get_or_init(|| std::env::var_os("CLAWD_DEBUG").is_some()) {
        eprintln!("[{event}] {payload:?}");
    }
    let _ = window.emit_to(WINDOW, event, payload);
}

/// Converts a window frame in physical screen pixels into the overlay's CSS pixels.
fn localize(window: &WebviewWindow, rect: Option<[i32; 4]>) -> Option<Platform> {
    let [left, top, right, _] = rect?;
    let origin = window.inner_position().ok()?;
    let scale = window.scale_factor().ok()?;
    Some(Platform {
        x: f64::from(left - origin.x) / scale,
        y: f64::from(top - origin.y) / scale,
        w: f64::from(right - left) / scale,
    })
}

#[cfg(target_os = "linux")]
fn prepare_linux_env() {
    use std::env;
    // Wayland doesn't let clients position windows or read the global cursor,
    // so run through XWayland unless the user picked a backend explicitly.
    if env::var_os("WAYLAND_DISPLAY").is_some() && env::var_os("GDK_BACKEND").is_none() {
        env::set_var("GDK_BACKEND", "x11");
    }
    // Transparent WebKitGTK windows render black on some GPU drivers with DMA-BUF.
    if env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

/// The webview gets no mouse events while click-through is on, so feed it
/// the global cursor position (in window-local CSS pixels) instead.
fn spawn_cursor_poller(window: WebviewWindow, state: Arc<AppState>) {
    thread::spawn(move || {
        let mut last: Option<Cursor> = None;
        let mut ticks = 0u32;
        loop {
            thread::sleep(CURSOR_POLL);
            if !window.is_visible().unwrap_or(false) {
                continue;
            }
            ticks = ticks.wrapping_add(1);
            if ticks.is_multiple_of(REFIT_EVERY) {
                // Resolution or taskbar changes, or Clawd's monitor was unplugged.
                if let Some(monitor) = state.monitor(&window) {
                    let _ = screens::fit(&window, &monitor);
                }
            }
            let (Ok(cursor), Ok(origin), Ok(scale)) = (
                window.cursor_position(),
                window.inner_position(),
                window.scale_factor(),
            ) else {
                continue;
            };
            let pos = Cursor {
                x: (cursor.x - f64::from(origin.x)) / scale,
                y: (cursor.y - f64::from(origin.y)) / scale,
            };
            if last != Some(pos) {
                last = Some(pos);
                let _ = window.emit_to(WINDOW, "cursor", pos);
            }
        }
    });
}
