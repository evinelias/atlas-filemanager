//! Windows shell context-menu integration.
//!
//! The Windows shell exposes per-file context menus (Open with, Send to,
//! 7-Zip, WinRAR, Properties, …) through `IContextMenu`. That interface is
//! apartment-threaded and **not** `Send`: it must live on a single STA thread
//! for its whole lifetime, and invoking a verb (e.g. Properties) can spin up a
//! modeless property sheet that needs a running message pump.
//!
//! # Design
//! A dedicated STA worker thread ([`worker`]) owns every COM object. The UI
//! talks to it only through `Send` plain-data messages:
//! - [`ShellMenuRequest`] — build a menu for some paths, expand a submenu,
//!   invoke an item, or dismiss (free the COM objects).
//! - [`ShellMenuEvent`] — the enumerated items (with rasterised icons), a
//!   submenu's children, or an invoke result.
//!
//! The worker is woken by posting `WM_ATLAS_SHELLMENU` to its message-only
//! window (same trick as the OLE drag layer's `WM_ATLAS_START_DRAG`). The App
//! drains [`ShellMenuHandle::drain_events`] when a worker event wakes the UI.
//!
//! # `unsafe_code`
//! The workspace forbids `unsafe_code`; every file here uses
//! `#![allow(unsafe_code)]` because COM, PIDLs and HMENU walking require it.
#![allow(unsafe_code)]

mod enumerate;
mod worker;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

/// Custom Win32 message: a request was queued; the worker should drain it.
pub(crate) const WM_ATLAS_SHELLMENU: u32 = WM_USER + 0x0D1B;

/// Command ID range passed to `IContextMenu::QueryContextMenu`.
/// Shell extensions assign command IDs within [ID_FIRST, ID_LAST].
pub const ID_FIRST: u32 = 1;
pub const ID_LAST: u32 = 0x7FFF;

/// A rasterised menu-item icon: straight (non-premultiplied) RGBA, row-major,
/// `w * h * 4` bytes. Crosses the thread boundary as plain data; the UI wraps it
/// in an `Arc<RenderImage>` on arrival.
#[derive(Clone)]
pub struct RawIcon {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Shape of one enumerated menu entry.
#[derive(Clone)]
pub enum NativeItemKind {
    Normal,
    Separator,
    /// A submenu. `children` is `None` until expanded (lazy for depth ≥ 2).
    Submenu {
        children: Option<Vec<NativeMenuItem>>,
    },
}

/// One native shell menu entry, flattened to plain `Send` data.
#[derive(Clone)]
pub struct NativeMenuItem {
    /// Command id as returned by `QueryContextMenu` (already offset to be unique
    /// within this menu; the worker subtracts `id_first` when invoking).
    pub id: u32,
    pub title: String,
    /// Canonical verb (`GetCommandString(GCS_VERBW)`), used for de-duplication
    /// against Atlas's own items. `None` when unavailable.
    // reason: consumed by the worker's dedupe pass; retained for diagnostics.
    #[allow(dead_code)]
    pub verb: Option<String>,
    pub icon: Option<RawIcon>,
    pub kind: NativeItemKind,
    pub disabled: bool,
    /// Bold default item (the one Explorer would run on double-click).
    pub is_default: bool,
}

/// A request from the UI to the shell worker.
pub enum ShellMenuRequest {
    /// Build a context menu for `paths` (must share one parent directory).
    Build { id: u64, paths: Vec<PathBuf> },
    /// Lazily expand a submenu's children (depth ≥ 2).
    ExpandSubmenu { id: u64, item_id: u32 },
    /// Invoke an item by its command id at the given screen point.
    Invoke {
        id: u64,
        item_id: u32,
        screen_pt: (i32, i32),
    },
    /// Free the COM objects backing menu `id`.
    Dismiss { id: u64 },
    /// Build a throwaway menu for `path` only to load third-party handlers and
    /// seed the UI cache under `key`. Lowest priority; never blocks a real open.
    Prewarm { key: String, path: PathBuf },
}

/// An event from the shell worker back to the UI.
pub enum ShellMenuEvent {
    Built {
        id: u64,
        items: Vec<NativeMenuItem>,
    },
    SubmenuExpanded {
        id: u64,
        item_id: u32,
        items: Vec<NativeMenuItem>,
    },
    InvokeDone {
        id: u64,
        result: Result<(), String>,
    },
    /// Result of a [`ShellMenuRequest::Prewarm`] (no live menu is kept).
    Prewarmed {
        key: String,
        items: Vec<NativeMenuItem>,
    },
}

/// UI-side handle to the shell worker thread.
#[derive(Clone)]
pub struct ShellMenuHandle {
    requests: Arc<Mutex<VecDeque<ShellMenuRequest>>>,
    events: Arc<Mutex<Vec<ShellMenuEvent>>>,
    /// The worker's message-only window. `0` until the worker has created it.
    worker_hwnd: Arc<AtomicIsize>,
}

impl ShellMenuHandle {
    /// Queue a request and wake the worker. No-op if the worker window is not up
    /// yet (the request stays queued and is drained when `WM_ATLAS_SHELLMENU`
    /// next fires; a `Build` always posts so the first one is never lost).
    pub fn send(&self, req: ShellMenuRequest) {
        if let Ok(mut q) = self.requests.lock() {
            q.push_back(req);
        }
        let hwnd = self.worker_hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_ATLAS_SHELLMENU,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    /// Seed the menu cache and load shell handlers ahead of the first right-click.
    /// Representative entries live in a temp dir created off the UI thread.
    pub fn prewarm(&self) {
        let handle = self.clone();
        let _ = std::thread::Builder::new()
            .name("atlas-shell-menu-prewarm".into())
            .spawn(move || {
                let root = std::env::temp_dir().join("atlas-ctx-prewarm");
                let folder = root.join("folder");
                if std::fs::create_dir_all(&folder).is_err() {
                    return;
                }
                handle.send(ShellMenuRequest::Prewarm { key: "dir".into(), path: folder });
                for ext in ["txt", "zip", "png"] {
                    let file = root.join(format!("a.{ext}"));
                    if file.exists() || std::fs::write(&file, b"").is_ok() {
                        handle.send(ShellMenuRequest::Prewarm { key: format!(".{ext}"), path: file });
                    }
                }
            });
    }

    /// Take all pending events. Called from the App's drain loop.
    pub fn drain_events(&self) -> Vec<ShellMenuEvent> {
        self.events
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

/// Spawn the STA worker thread and return a handle. Call once in `App::new`.
pub fn spawn() -> ShellMenuHandle {
    let requests = Arc::new(Mutex::new(VecDeque::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let worker_hwnd = Arc::new(AtomicIsize::new(0));

    let handle = ShellMenuHandle {
        requests: requests.clone(),
        events: events.clone(),
        worker_hwnd: worker_hwnd.clone(),
    };

    std::thread::Builder::new()
        .name("atlas-shell-menu".into())
        .spawn(move || worker::run(requests, events, worker_hwnd))
        .expect("spawn shell-menu worker");

    handle
}
