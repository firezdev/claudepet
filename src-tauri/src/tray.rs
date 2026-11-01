//! The tray menu: switches, language, Clawd's needs, reminders, Claude Code
//! limits, updates and finding `claude` for Haiku lines.

use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
};

use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    App, AppHandle, Emitter, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;

use crate::{
    claude,
    context::ContextEvent,
    hooks::{self, Window},
    reminders,
    settings::{Lang, Memory},
    update, AppState, WINDOW,
};

#[derive(Clone, Serialize)]
struct Notice {
    phrase: &'static str,
}

pub fn notice(app: &AppHandle, phrase: &'static str) {
    let _ = app.emit_to(WINDOW, "notice", Notice { phrase });
}

fn text(lang: Lang, key: &str) -> &'static str {
    let ru = lang == Lang::Ru;
    match key {
        "recall" => if ru { "Позвать Clawd" } else { "Call Clawd" },
        "chat" => if ru { "Поговорить с Clawd…" } else { "Talk to Clawd…" },
        "hide" => if ru { "Спрятать" } else { "Hide" },
        "show" => if ru { "Показать" } else { "Show" },
        "feed" => if ru { "Покормить Clawd" } else { "Feed Clawd" },
        "forget" => if ru { "Забыть разговоры" } else { "Forget our chats" },
        "no_reminders" => if ru { "Напоминаний нет" } else { "No reminders" },
        "listen" => if ru { "Слушать звук" } else { "Listen to sound" },
        "watch" => if ru { "Видеть окна и печать" } else { "See windows and typing" },
        "llm" => if ru { "Реплики через Claude (Haiku)" } else { "Lines from Claude (Haiku)" },
        "pick" => if ru { "Указать путь к claude…" } else { "Locate claude…" },
        "hooks" => if ru { "Связать с Claude Code" } else { "Connect to Claude Code" },
        "autostart" => if ru { "Запускать вместе с системой" } else { "Start with the system" },
        "ask" => if ru { "Разрешения Claude Code через Clawd" } else { "Claude Code permissions via Clawd" },
        "check_update" => if ru { "Проверить обновления" } else { "Check for updates" },
        "quit" => if ru { "Выход" } else { "Quit" },
        "pick_title" => if ru { "Где находится claude?" } else { "Where is claude?" },
        _ => "",
    }
}

/// Menu lines that show live numbers; kept so they can be redrawn in another language.
#[derive(Default)]
struct Labels {
    vitals: Option<(f64, f64)>,
    limits: Option<(Option<Window>, Option<Window>)>,
    update: Option<String>,
}

fn vitals_text(lang: Lang, vitals: Option<(f64, f64)>) -> String {
    let (mood, food) = vitals.unwrap_or((70.0, 70.0));
    let face = if mood >= 70.0 { "😊" } else if mood >= 35.0 { "🙂" } else { "😟" };
    match lang {
        Lang::Ru => format!("{face} Настроение {mood:.0}% · 🍕 сытость {food:.0}%"),
        Lang::En => format!("{face} Mood {mood:.0}% · 🍕 fullness {food:.0}%"),
    }
}

fn limits_text(lang: Lang, limits: Option<&(Option<Window>, Option<Window>)>) -> String {
    let pct = |w: &Option<Window>| w.as_ref().map_or("—".to_string(), |w| format!("{:.0}%", w.used));
    match (lang, limits) {
        (Lang::Ru, None) => "Лимит Claude: узнаю через минуту…".into(),
        (Lang::En, None) => "Claude limit: checking in a minute…".into(),
        (Lang::Ru, Some((five, week))) => format!("Лимит Claude: 5 ч — {}, неделя — {}", pct(five), pct(week)),
        (Lang::En, Some((five, week))) => format!("Claude limit: 5h {}, week {}", pct(five), pct(week)),
    }
}

fn reminders_text(lang: Lang, count: usize) -> String {
    if count == 0 {
        return text(lang, "no_reminders").into();
    }
    match lang {
        Lang::Ru => format!("Отменить напоминания ({count})"),
        Lang::En => format!("Cancel reminders ({count})"),
    }
}

fn update_text(lang: Lang, version: Option<&str>) -> String {
    match (lang, version) {
        (_, None) => text(lang, "check_update").into(),
        (Lang::Ru, Some(v)) => format!("⬆ Обновить до v{v}"),
        (Lang::En, Some(v)) => format!("⬆ Update to v{v}"),
    }
}

#[derive(Clone)]
struct Items {
    recall: MenuItem<Wry>,
    chat: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    vitals: MenuItem<Wry>,
    feed: MenuItem<Wry>,
    reminders: MenuItem<Wry>,
    forget: MenuItem<Wry>,
    limits: MenuItem<Wry>,
    link: CheckMenuItem<Wry>,
    ask: CheckMenuItem<Wry>,
    listen: CheckMenuItem<Wry>,
    watch: CheckMenuItem<Wry>,
    llm: CheckMenuItem<Wry>,
    pick: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    ru: CheckMenuItem<Wry>,
    en: CheckMenuItem<Wry>,
    update: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

/// The menu, kept in AppState so other parts of the app can update its live lines.
pub struct Tray {
    items: Items,
    labels: Mutex<Labels>,
}

impl Items {
    fn relabel(&self, lang: Lang, visible: bool) {
        let _ = self.recall.set_text(text(lang, "recall"));
        let _ = self.chat.set_text(text(lang, "chat"));
        let _ = self.toggle.set_text(text(lang, if visible { "hide" } else { "show" }));
        let _ = self.feed.set_text(text(lang, "feed"));
        let _ = self.forget.set_text(text(lang, "forget"));
        let _ = self.link.set_text(text(lang, "hooks"));
        let _ = self.ask.set_text(text(lang, "ask"));
        let _ = self.listen.set_text(text(lang, "listen"));
        let _ = self.watch.set_text(text(lang, "watch"));
        let _ = self.llm.set_text(text(lang, "llm"));
        let _ = self.pick.set_text(text(lang, "pick"));
        let _ = self.autostart.set_text(text(lang, "autostart"));
        let _ = self.quit.set_text(text(lang, "quit"));
        let _ = self.ru.set_checked(lang == Lang::Ru);
        let _ = self.en.set_checked(lang == Lang::En);
    }
}

/// Redraws the lines with live numbers.
pub fn refresh(state: &AppState) {
    let Some(tray) = state.tray.get() else {
        return;
    };
    let lang = state.haiku.lang();
    let labels = tray.labels.lock().unwrap();
    let count = reminders::count(state);
    let _ = tray.items.vitals.set_text(vitals_text(lang, labels.vitals));
    let _ = tray.items.limits.set_text(limits_text(lang, labels.limits.as_ref()));
    let _ = tray.items.reminders.set_text(reminders_text(lang, count));
    let _ = tray.items.reminders.set_enabled(count > 0);
    let _ = tray.items.update.set_text(update_text(lang, labels.update.as_deref()));
}

fn with_labels(state: &AppState, change: impl FnOnce(&mut Labels)) {
    if let Some(tray) = state.tray.get() {
        change(&mut tray.labels.lock().unwrap());
    }
    refresh(state);
}

pub fn set_vitals(state: &AppState, mood: f64, food: f64) {
    with_labels(state, |l| l.vitals = Some((mood, food)));
}

pub fn set_limits(state: &AppState, five_hour: Option<Window>, seven_day: Option<Window>) {
    with_labels(state, |l| l.limits = Some((five_hour, seven_day)));
}

pub fn set_update(state: &AppState, version: Option<String>) {
    with_labels(state, |l| l.update = version);
}

fn window_visible(app: &AppHandle) -> bool {
    app.get_webview_window(WINDOW).and_then(|w| w.is_visible().ok()).unwrap_or(false)
}

pub fn build(app: &App, state: Arc<AppState>) -> tauri::Result<()> {
    let settings = state.settings.lock().unwrap().clone();
    let lang = settings.language;
    let hooks_installed = hooks::settings_path().is_some_and(|p| hooks::is_installed(&p));
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let item = |id: &str, label: &str, enabled: bool| MenuItem::with_id(app, id, label, enabled, None::<&str>);
    let check = |id: &str, on: bool| CheckMenuItem::with_id(app, id, text(lang, id), true, on, None::<&str>);
    let items = Items {
        recall: item("recall", text(lang, "recall"), true)?,
        chat: item("chat", text(lang, "chat"), true)?,
        toggle: item("toggle", text(lang, "hide"), true)?,
        vitals: item("vitals", &vitals_text(lang, None), false)?,
        feed: item("feed", text(lang, "feed"), true)?,
        reminders: item("reminders", &reminders_text(lang, settings.reminders.len()), !settings.reminders.is_empty())?,
        forget: item("forget", text(lang, "forget"), true)?,
        limits: item("limits", &limits_text(lang, None), false)?,
        link: CheckMenuItem::with_id(app, "hooks", text(lang, "hooks"), true, hooks_installed, None::<&str>)?,
        ask: check("ask", settings.ask_permissions)?,
        listen: check("listen", settings.listen)?,
        watch: check("watch", settings.watch)?,
        llm: check("llm", settings.llm)?,
        pick: item("pick", text(lang, "pick"), true)?,
        autostart: check("autostart", autostart_on)?,
        ru: CheckMenuItem::with_id(app, "lang_ru", "Русский", true, lang == Lang::Ru, None::<&str>)?,
        en: CheckMenuItem::with_id(app, "lang_en", "English", true, lang == Lang::En, None::<&str>)?,
        update: item("update", text(lang, "check_update"), true)?,
        quit: item("quit", text(lang, "quit"), true)?,
    };
    let language = Submenu::with_items(app, "Язык / Language", true, &[&items.ru, &items.en])?;
    let separator = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(
        app,
        &[
            &items.recall,
            &items.chat,
            &items.toggle,
            &separator()?,
            &items.vitals,
            &items.feed,
            &items.reminders,
            &items.forget,
            &separator()?,
            &items.limits,
            &items.link,
            &items.ask,
            &separator()?,
            &items.listen,
            &items.watch,
            &items.llm,
            &items.pick,
            &items.autostart,
            &language,
            &separator()?,
            &items.update,
            &items.quit,
        ],
    )?;
    let _ = state.tray.set(Tray { items: items.clone(), labels: Mutex::new(Labels::default()) });

    let mut tray = TrayIconBuilder::with_id("clawd").tooltip("Clawd").menu(&menu).show_menu_on_left_click(true);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.on_menu_event(move |app, event| {
        let lang = state.haiku.lang();
        match event.id.as_ref() {
            "recall" => {
                set_visible(app, &items, lang, true);
                if let Some(window) = app.get_webview_window(WINDOW) {
                    crate::move_to_cursor_monitor(&window, &state);
                }
                let _ = app.emit_to(WINDOW, "recall", ());
            }
            "chat" => {
                set_visible(app, &items, lang, true);
                let _ = app.emit_to(WINDOW, "chat_open", ());
            }
            "toggle" => set_visible(app, &items, lang, !window_visible(app)),
            "feed" => {
                set_visible(app, &items, lang, true);
                let _ = app.emit_to(WINDOW, "feed", ());
            }
            "reminders" => {
                reminders::clear(&state);
                refresh(&state);
                notice(app, "reminders_cleared");
            }
            "forget" => {
                state.update(|s| s.memory = Memory::default());
                let _ = app.emit_to(WINDOW, "memory_cleared", ());
                notice(app, "forgot");
            }
            "listen" => {
                let on = items.listen.is_checked().unwrap_or(true);
                state.listening.store(on, Ordering::Relaxed);
                state.update(|s| s.listen = on);
            }
            "watch" => {
                let on = items.watch.is_checked().unwrap_or(true);
                state.watching.store(on, Ordering::Relaxed);
                state.update(|s| s.watch = on);
                if !on {
                    // Make the pet forget what it saw.
                    let _ = app.emit_to(WINDOW, "context", ContextEvent::Window { app: String::new(), title: String::new() });
                    let _ = app.emit_to(WINDOW, "context", ContextEvent::Typing { kpm: 0 });
                    let _ = app.emit_to(WINDOW, "platform", None::<()>);
                }
            }
            "llm" => {
                let on = items.llm.is_checked().unwrap_or(false);
                state.update(|s| s.llm = on);
                if !on {
                    state.haiku.enabled.store(false, Ordering::Relaxed);
                } else if state.haiku.cli().is_some() {
                    state.haiku.enabled.store(true, Ordering::Relaxed);
                } else {
                    find_or_ask(app.clone(), state.clone(), items.clone());
                }
            }
            "pick" => pick_claude(app.clone(), state.clone(), items.clone()),
            "hooks" => {
                let on = items.link.is_checked().unwrap_or(false);
                let phrase = match link_claude_code(&state, on, true) {
                    Ok(()) if on => "hooks_on",
                    Ok(()) => "hooks_off",
                    Err(e) => {
                        eprintln!("Claude Code hooks: {e}");
                        let _ = items.link.set_checked(!on);
                        "hooks_error"
                    }
                };
                notice(app, phrase);
            }
            "ask" => {
                let on = items.ask.is_checked().unwrap_or(true);
                state.update(|s| s.ask_permissions = on);
                if let Some(hub) = state.hub.get() {
                    hub.ask_permissions.store(on, Ordering::Relaxed);
                }
            }
            "autostart" => {
                let on = items.autostart.is_checked().unwrap_or(false);
                let manager = app.autolaunch();
                let result = if on { manager.enable() } else { manager.disable() };
                if let Err(e) = result {
                    eprintln!("autostart: {e}");
                    let _ = items.autostart.set_checked(!on);
                }
            }
            id @ ("lang_ru" | "lang_en") => {
                let lang = if id == "lang_ru" { Lang::Ru } else { Lang::En };
                *state.haiku.lang.write().unwrap() = lang;
                state.update(|s| s.language = lang);
                items.relabel(lang, window_visible(app));
                refresh(&state);
                let _ = app.emit_to(WINDOW, "language", lang);
            }
            "update" => {
                let (app, state) = (app.clone(), state.clone());
                let found = state.update.lock().unwrap().is_some();
                tauri::async_runtime::spawn(async move {
                    if found {
                        update::install(&app, &state).await;
                    } else {
                        update::check(&app, &state, true).await;
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        }
    })
    .build(app)?;
    refresh(&state_ref(app));
    Ok(())
}

fn state_ref(app: &App) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

/// Adds or removes Clawd's hooks and status line in Claude Code's settings.
/// `fresh` is the user clicking "Connect": event hooks, the permission card and the
/// status line wrapper. Otherwise (at startup) only what an earlier version set up is
/// brought up to date, so an upgrade doesn't take over the user's status line or
/// start answering Claude Code's permission prompts without being asked.
pub fn link_claude_code(state: &AppState, on: bool, fresh: bool) -> Result<(), String> {
    let path = hooks::settings_path().ok_or_else(|| "home directory not found".to_string())?;
    if on && !fresh && !hooks::has_permission_hook(&path) {
        // Linked before permission cards existed: Claude Code keeps asking as before
        // until the user turns the card on in the tray.
        state.update(|s| s.ask_permissions = false);
        if let Some(hub) = state.hub.get() {
            hub.ask_permissions.store(false, Ordering::Relaxed);
        }
    }
    let backup = state.settings.lock().unwrap().statusline_backup.clone();
    let backup = hooks::set_installed(&path, on.then_some(state.token.as_str()), backup, fresh)?;
    state.update(|s| s.statusline_backup = backup);
    Ok(())
}

fn set_visible(app: &AppHandle, items: &Items, lang: Lang, visible: bool) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = if visible { window.show() } else { window.hide() };
    }
    let _ = items.toggle.set_text(text(lang, if visible { "hide" } else { "show" }));
}

/// Uses `path` for Haiku if it really is Claude Code.
fn adopt(app: &AppHandle, state: &AppState, items: &Items, path: PathBuf) -> bool {
    match claude::verify(&path) {
        Ok(version) => {
            eprintln!("claude: using {} ({version})", path.display());
            *state.haiku.cli.write().unwrap() = Some(path.clone());
            state.haiku.enabled.store(true, Ordering::Relaxed);
            state.update(|s| {
                s.claude_path = Some(path);
                s.llm = true;
            });
            let _ = items.llm.set_checked(true);
            true
        }
        Err(e) => {
            eprintln!("claude: {} rejected: {e}", path.display());
            notice(app, "claude_invalid");
            false
        }
    }
}

/// Searches for `claude` off the UI thread; if it's nowhere to be found, asks the user.
fn find_or_ask(app: AppHandle, state: Arc<AppState>, items: Items) {
    thread::spawn(move || {
        let saved = state.settings.lock().unwrap().claude_path.clone();
        if let Some(path) = claude::find_cli(saved.as_deref()) {
            if adopt(&app, &state, &items, path) {
                notice(&app, "claude_found");
                return;
            }
        }
        notice(&app, "claude_missing");
        pick_claude(app, state, items);
    });
}

fn pick_claude(app: AppHandle, state: Arc<AppState>, items: Items) {
    let mut dialog = app.dialog().file().set_title(text(state.haiku.lang(), "pick_title"));
    if cfg!(windows) {
        dialog = dialog.add_filter("claude", &["exe", "cmd"]);
    }
    if let Some(home) = claude::home_dir() {
        dialog = dialog.set_directory(home);
    }
    dialog.pick_file(move |file| {
        let picked = file.and_then(|f| f.into_path().ok());
        thread::spawn(move || {
            let adopted = picked.is_some_and(|path| adopt(&app, &state, &items, path));
            if adopted {
                notice(&app, "claude_found");
            } else if state.haiku.cli().is_none() {
                // Still nothing to talk through: leave Haiku off.
                state.haiku.enabled.store(false, Ordering::Relaxed);
                state.update(|s| s.llm = false);
                let _ = items.llm.set_checked(false);
            }
        });
    });
}
