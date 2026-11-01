//! What the user is up to: the focused window and how fast they type.
//! Only the number of key presses is counted; which keys were pressed is never kept.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

const POLL: Duration = Duration::from_millis(30);
const WINDOW_EVERY: u32 = 33; // ~1 s
const TYPING_EVERY: Duration = Duration::from_secs(2);
const TYPING_WINDOW: Duration = Duration::from_secs(10);
const MAX_TITLE: usize = 120;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ContextEvent {
    Window { app: String, title: String },
    /// Key presses per minute over the last few seconds.
    Typing { kpm: u32 },
    /// The focused window's frame in physical screen pixels (left, top, right,
    /// bottom), or None when there's nothing to stand on (maximized, desktop...).
    Frame { rect: Option<[i32; 4]> },
}

type KeyBits = [u8; 32];

fn count_new_presses(prev: &KeyBits, now: &KeyBits) -> u32 {
    prev.iter().zip(now).map(|(p, n)| (n & !p).count_ones()).sum()
}

fn shorten(title: &str) -> String {
    let title = title.trim();
    match title.char_indices().nth(MAX_TITLE) {
        Some((i, _)) => format!("{}…", &title[..i]),
        None => title.to_string(),
    }
}

pub fn spawn(enabled: Arc<AtomicBool>, mut emit: impl FnMut(ContextEvent) + Send + 'static) {
    thread::spawn(move || loop {
        if !enabled.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let Some(probe) = platform::Probe::new() else {
            eprintln!("context: no desktop session to watch");
            thread::sleep(Duration::from_secs(30));
            continue;
        };
        let mut last_window: Option<(String, String)> = None;
        let mut prev_keys: KeyBits = [0; 32];
        let mut presses: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut last_typing = Instant::now();
        let mut last_kpm = 0;
        let mut last_frame: Option<Option<[i32; 4]>> = None;
        let mut tick = 0u32;
        while enabled.load(Ordering::Relaxed) {
            thread::sleep(POLL);
            tick = tick.wrapping_add(1);

            let keys = probe.keys();
            let n = count_new_presses(&prev_keys, &keys);
            prev_keys = keys;
            if n > 0 {
                presses.push_back((Instant::now(), n));
            }
            while presses.front().is_some_and(|(t, _)| t.elapsed() > TYPING_WINDOW) {
                presses.pop_front();
            }
            if last_typing.elapsed() >= TYPING_EVERY {
                last_typing = Instant::now();
                let total: u32 = presses.iter().map(|(_, n)| n).sum();
                let kpm = total * 60 / TYPING_WINDOW.as_secs() as u32;
                if kpm != last_kpm {
                    last_kpm = kpm;
                    emit(ContextEvent::Typing { kpm });
                }
            }

            // The frame is polled every tick so Clawd rides along when the window is dragged.
            if let Some(frame) = probe.frame() {
                if last_frame != Some(frame) {
                    last_frame = Some(frame);
                    emit(ContextEvent::Frame { rect: frame });
                }
            }

            if tick.is_multiple_of(WINDOW_EVERY) {
                if let Some((app, title)) = probe.active_window() {
                    // Clicking Clawd focuses his own overlay; that's not interesting.
                    let current = (app, shorten(&title));
                    if current.1 != "Clawd" && last_window.as_ref() != Some(&current) {
                        last_window = Some(current.clone());
                        emit(ContextEvent::Window {
                            app: current.0,
                            title: current.1,
                        });
                    }
                }
            }
        }
    });
}

#[cfg(target_os = "linux")]
mod platform {
    use super::KeyBits;
    use std::{
        cell::RefCell,
        time::{Duration, Instant},
    };
    use x11rb::{
        connection::Connection,
        protocol::xproto::{Atom, AtomEnum, ConnectionExt, Window},
        rust_connection::RustConnection,
    };

    /// How long what we know about the focused window (owner, state, frame margins)
    /// is trusted before it's read again; its position is read every poll.
    const STANDING_TTL: Duration = Duration::from_millis(500);

    pub struct Probe {
        conn: RustConnection,
        root: Window,
        net_active_window: Atom,
        net_wm_name: Atom,
        utf8_string: Atom,
        frame: FrameAtoms,
        standing: RefCell<Option<(Window, Instant, Standing)>>,
    }

    /// Whether Clawd can stand on a window, and the margins between its X geometry and
    /// the visible frame: [left, top, right, bottom] to subtract from x, y and add to w, h.
    #[derive(Clone, Copy)]
    enum Standing {
        Own,
        No,
        Yes([i32; 4]),
    }

    /// EWMH properties that say where a window's frame is and whether it's one to stand on.
    struct FrameAtoms {
        pid: Atom,
        state: Atom,
        maximized_vert: Atom,
        maximized_horz: Atom,
        hidden: Atom,
        fullscreen: Atom,
        window_type: Atom,
        type_desktop: Atom,
        type_dock: Atom,
        frame_extents: Atom,
        gtk_frame_extents: Atom,
    }

    /// A property made of 32-bit values (CARDINAL, ATOM).
    fn words(raw: &[u8]) -> Vec<u32> {
        raw.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    impl Probe {
        pub fn new() -> Option<Self> {
            let (conn, screen) = x11rb::connect(None).ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            let atom = |name: &[u8]| Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom);
            let net_active_window = atom(b"_NET_ACTIVE_WINDOW")?;
            let net_wm_name = atom(b"_NET_WM_NAME")?;
            let utf8_string = atom(b"UTF8_STRING")?;
            let frame = FrameAtoms {
                pid: atom(b"_NET_WM_PID")?,
                state: atom(b"_NET_WM_STATE")?,
                maximized_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
                maximized_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
                hidden: atom(b"_NET_WM_STATE_HIDDEN")?,
                fullscreen: atom(b"_NET_WM_STATE_FULLSCREEN")?,
                window_type: atom(b"_NET_WM_WINDOW_TYPE")?,
                type_desktop: atom(b"_NET_WM_WINDOW_TYPE_DESKTOP")?,
                type_dock: atom(b"_NET_WM_WINDOW_TYPE_DOCK")?,
                frame_extents: atom(b"_NET_FRAME_EXTENTS")?,
                gtk_frame_extents: atom(b"_GTK_FRAME_EXTENTS")?,
            };
            Some(Self {
                conn,
                root,
                net_active_window,
                net_wm_name,
                utf8_string,
                frame,
                standing: RefCell::new(None),
            })
        }

        fn property(&self, window: Window, property: Atom, type_: Atom) -> Option<Vec<u8>> {
            let reply = self
                .conn
                .get_property(false, window, property, type_, 0, 1024)
                .ok()?
                .reply()
                .ok()?;
            Some(reply.value)
        }

        fn focused(&self) -> Option<Window> {
            let raw = self.property(self.root, self.net_active_window, AtomEnum::WINDOW.into())?;
            let window = u32::from_ne_bytes(raw.get(..4)?.try_into().ok()?);
            (window != 0).then_some(window)
        }

        pub fn active_window(&self) -> Option<(String, String)> {
            let window = self.focused()?;
            let title = self
                .property(window, self.net_wm_name, self.utf8_string)
                .filter(|t| !t.is_empty())
                .or_else(|| self.property(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
                .unwrap_or_default();
            // WM_CLASS is "instance\0Class\0"; the class reads like an app name.
            let class = self
                .property(window, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into())
                .unwrap_or_default();
            let app = class
                .split(|&b| b == 0)
                .rfind(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            Some((app, String::from_utf8_lossy(&title).into_owned()))
        }

        /// Reads what rarely changes about `window`: five property requests, so it's cached.
        fn standing(&self, window: Window) -> Standing {
            let cached = *self.standing.borrow();
            if let Some((w, at, standing)) = cached {
                if w == window && at.elapsed() < STANDING_TTL {
                    return standing;
                }
            }
            let standing = self.read_standing(window);
            *self.standing.borrow_mut() = Some((window, Instant::now(), standing));
            standing
        }

        fn read_standing(&self, window: Window) -> Standing {
            let a = &self.frame;
            let cardinal: Atom = AtomEnum::CARDINAL.into();
            let atom_type: Atom = AtomEnum::ATOM.into();
            let pid = self.property(window, a.pid, cardinal).map(|r| words(&r));
            if pid.as_ref().and_then(|p| p.first()) == Some(&std::process::id()) {
                return Standing::Own;
            }
            let state = self.property(window, a.state, atom_type).map(|r| words(&r)).unwrap_or_default();
            let maximized = state.contains(&a.maximized_vert) && state.contains(&a.maximized_horz);
            if maximized || state.contains(&a.hidden) || state.contains(&a.fullscreen) {
                return Standing::No;
            }
            let kinds = self.property(window, a.window_type, atom_type).map(|r| words(&r)).unwrap_or_default();
            if kinds.contains(&a.type_desktop) || kinds.contains(&a.type_dock) {
                return Standing::No;
            }
            // The window manager's title bar sits outside the window (_NET_FRAME_EXTENTS);
            // GTK's own title bars sit inside, under a shadow margin (_GTK_FRAME_EXTENTS).
            let extents = |atom| {
                let v = self.property(window, atom, cardinal).map(|r| words(&r)).unwrap_or_default();
                if v.len() == 4 { [v[0], v[1], v[2], v[3]].map(|e| e as i32) } else { [0; 4] }
            };
            let [l, r, t, b] = extents(a.frame_extents);
            let [gl, gr, gt, gb] = extents(a.gtk_frame_extents);
            Standing::Yes([l - gl, t - gt, r - gr, b - gb])
        }

        /// None when the focused window is Clawd's own overlay: clicking him
        /// shouldn't knock him off the window he's sitting on.
        pub fn frame(&self) -> Option<Option<[i32; 4]>> {
            let Some(window) = self.focused() else {
                return Some(None);
            };
            let [l, t, r, b] = match self.standing(window) {
                Standing::Own => return None,
                Standing::No => return Some(None),
                Standing::Yes(margins) => margins,
            };
            let Some(geometry) = self.conn.get_geometry(window).ok().and_then(|c| c.reply().ok()) else {
                return Some(None);
            };
            let Some(origin) = self.conn.translate_coordinates(window, self.root, 0, 0).ok().and_then(|c| c.reply().ok())
            else {
                return Some(None);
            };
            let (x, y) = (i32::from(origin.dst_x), i32::from(origin.dst_y));
            let (w, h) = (i32::from(geometry.width), i32::from(geometry.height));
            let rect = [x - l, y - t, x + w + r, y + h + b];
            if rect[2] - rect[0] < 160 || rect[3] - rect[1] < 80 {
                return Some(None);
            }
            Some(Some(rect))
        }

        pub fn keys(&self) -> KeyBits {
            self.conn
                .query_keymap()
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.keys)
                .unwrap_or([0; 32])
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::KeyBits;
    use std::{ffi::c_void, path::Path};
    use windows::{
        core::PWSTR,
        Win32::{
            Foundation::{CloseHandle, RECT},
            Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION},
            UI::{
                Input::KeyboardAndMouse::GetAsyncKeyState,
                WindowsAndMessaging::{
                    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
                    IsIconic, IsWindowVisible, IsZoomed,
                },
            },
        },
    };

    /// The desktop and the taskbar are focusable but nothing to stand on.
    const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

    pub struct Probe;

    impl Probe {
        pub fn new() -> Option<Self> {
            Some(Self)
        }

        pub fn active_window(&self) -> Option<(String, String)> {
            unsafe {
                let hwnd = GetForegroundWindow();
                if hwnd.is_invalid() {
                    return None;
                }
                let mut buf = [0u16; 512];
                let len = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
                let title = String::from_utf16_lossy(&buf[..len]);

                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let mut app = String::new();
                if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    let mut path = [0u16; 1024];
                    let mut size = path.len() as u32;
                    if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut size).is_ok() {
                        let full = String::from_utf16_lossy(&path[..size as usize]);
                        if let Some(stem) = Path::new(&full).file_stem() {
                            app = stem.to_string_lossy().into_owned();
                        }
                    }
                    let _ = CloseHandle(process);
                }
                Some((app, title))
            }
        }

        /// None when the focused window is Clawd's own overlay: clicking him
        /// shouldn't knock him off the window he's sitting on.
        pub fn frame(&self) -> Option<Option<[i32; 4]>> {
            unsafe {
                let hwnd = GetForegroundWindow();
                if hwnd.is_invalid() {
                    return Some(None);
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid == std::process::id() {
                    return None;
                }
                if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() {
                    return Some(None);
                }
                let mut class = [0u16; 64];
                let len = GetClassNameW(hwnd, &mut class).max(0) as usize;
                if SHELL_CLASSES.contains(&String::from_utf16_lossy(&class[..len]).as_str()) {
                    return Some(None);
                }
                // The DWM frame excludes the invisible resize borders GetWindowRect includes.
                let mut r = RECT::default();
                let dwm = DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    &mut r as *mut RECT as *mut c_void,
                    std::mem::size_of::<RECT>() as u32,
                );
                if dwm.is_err() && GetWindowRect(hwnd, &mut r).is_err() {
                    return Some(None);
                }
                if r.right - r.left < 160 || r.bottom - r.top < 80 {
                    return Some(None);
                }
                Some(Some([r.left, r.top, r.right, r.bottom]))
            }
        }

        pub fn keys(&self) -> KeyBits {
            let mut bits = [0u8; 32];
            // 0x08.. skips the mouse buttons.
            for vk in 0x08..=0xFE_i32 {
                if unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0 {
                    bits[vk as usize / 8] |= 1 << (vk % 8);
                }
            }
            bits
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::KeyBits;

    pub struct Probe;

    impl Probe {
        pub fn new() -> Option<Self> {
            None
        }
        pub fn active_window(&self) -> Option<(String, String)> {
            None
        }
        pub fn frame(&self) -> Option<Option<[i32; 4]>> {
            None
        }
        pub fn keys(&self) -> KeyBits {
            [0; 32]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_new_presses() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        b[1] = 0b0000_0110;
        assert_eq!(count_new_presses(&a, &b), 2);
        a[1] = 0b0000_0010;
        assert_eq!(count_new_presses(&a, &b), 1);
        assert_eq!(count_new_presses(&b, &a), 0);
    }

    #[test]
    fn long_titles_are_shortened() {
        let long = "я".repeat(200);
        let short = shorten(&long);
        assert_eq!(short.chars().count(), MAX_TITLE + 1);
        assert_eq!(shorten("  Firefox  "), "Firefox");
    }
}
