//! Self-update from GitHub Releases (tauri-plugin-updater). Releases are signed:
//! the public key lets only builds signed with the maintainer's key in. It isn't in
//! tauri.conf.json: the release workflow puts it in, so local builds don't update.

use std::{sync::Arc, thread, time::Duration};

use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

use crate::{tray, AppState};

const FIRST_CHECK: Duration = Duration::from_secs(90);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Whether this build knows the key releases are signed with.
fn configured(app: &AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.trim().is_empty())
}

/// Checks shortly after start and then every few hours, quietly.
pub fn spawn(app: AppHandle, state: Arc<AppState>) {
    if !configured(&app) {
        return;
    }
    thread::spawn(move || {
        thread::sleep(FIRST_CHECK);
        loop {
            tauri::async_runtime::block_on(check(&app, &state, false));
            thread::sleep(CHECK_EVERY);
        }
    });
}

/// Looks for a newer release; `manual` checks also report "you're up to date" and errors.
pub async fn check(app: &AppHandle, state: &AppState, manual: bool) {
    if !configured(app) {
        if manual {
            tray::notice(app, "update_unconfigured");
        }
        return;
    }
    let found = match app.updater() {
        Ok(updater) => updater.check().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match found {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let known = state.update.lock().unwrap().as_ref().is_some_and(|u| u.version == version);
            *state.update.lock().unwrap() = Some(update);
            tray::set_update(state, Some(version));
            if manual || !known {
                tray::notice(app, "update_available");
            }
        }
        Ok(None) => {
            if manual {
                tray::notice(app, "update_none");
            }
        }
        Err(e) => {
            eprintln!("update check: {e}");
            if manual {
                tray::notice(app, "update_error");
            }
        }
    }
}

/// Downloads and installs the update found earlier, then restarts Clawd.
pub async fn install(app: &AppHandle, state: &AppState) {
    let Some(update) = state.update.lock().unwrap().take() else {
        return;
    };
    tray::notice(app, "update_installing");
    match update.download_and_install(|_, _| {}, || {}).await {
        Ok(()) => app.restart(),
        Err(e) => {
            eprintln!("update install: {e}");
            tray::set_update(state, None);
            tray::notice(app, "update_error");
        }
    }
}
