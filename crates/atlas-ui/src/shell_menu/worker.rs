//! The STA worker thread: owns every COM object for the lifetime of a menu.
//!
//! Reentrancy note: invoking a verb runs a nested message loop that can deliver
//! `WM_INITMENUPOPUP`/`WM_DRAWITEM` back to [`wndproc`], which reads the active
//! menu. We therefore never hold a `RefCell` borrow across an invoke — the COM
//! interface is cloned (refcount bump) out under a short borrow first.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, IBindCtx, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToObject, SHParseDisplayName,
    CMF_NORMAL, CMINVOKECOMMANDINFOEX,
};
use windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, CW_USEDEFAULT, HMENU, MSG, SW_SHOWNORMAL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
};

// CMIC_MASK_* are plain bit flags; declared locally to avoid version-skewed
// constant paths in the `windows` crate.
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
const CMIC_MASK_PTINVOKE: u32 = 0x2000_0000;

use super::enumerate::walk_menu;
use super::{
    NativeMenuItem, ShellMenuEvent, ShellMenuRequest, ID_FIRST, ID_LAST, WM_ATLAS_SHELLMENU,
};

/// The live shell context menu and its backing COM objects. `Drop` frees the
/// HMENU; the `IContextMenu*` are released by their own refcounts.
pub(crate) struct ActiveMenu {
    pub id: u64,
    pub icm: IContextMenu,
    pub icm2: Option<IContextMenu2>,
    pub icm3: Option<IContextMenu3>,
    pub hmenu: HMENU,
    pub id_first: u32,
}

impl Drop for ActiveMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

struct WorkerState {
    requests: Arc<Mutex<VecDeque<ShellMenuRequest>>>,
    events: Arc<Mutex<Vec<ShellMenuEvent>>>,
    active: Option<ActiveMenu>,
}

thread_local! {
    static WORKER: RefCell<Option<WorkerState>> = const { RefCell::new(None) };
}

fn push_event(ev: ShellMenuEvent) {
    WORKER.with(|w| {
        if let Some(ws) = w.borrow().as_ref() {
            if let Ok(mut q) = ws.events.lock() {
                q.push(ev);
                crate::ui_wake::wake();
            }
        }
    });
}

/// Entry point for the worker thread.
pub(crate) fn run(
    requests: Arc<Mutex<VecDeque<ShellMenuRequest>>>,
    events: Arc<Mutex<Vec<ShellMenuEvent>>>,
    worker_hwnd: Arc<AtomicIsize>,
) {
    unsafe {
        // STA: IContextMenu and any property sheet it spawns require it.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("AtlasShellMenuWorker");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("atlas-shell-menu"),
            WINDOW_STYLE(0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        );
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(?e, "shell-menu worker window creation failed");
                return;
            }
        };

        WORKER.with(|w| {
            *w.borrow_mut() = Some(WorkerState {
                requests,
                events,
                active: None,
            });
        });
        worker_hwnd.store(hwnd.0 as isize, Ordering::Release);

        // Any requests queued before the window existed are drained now.
        drain_requests(hwnd);

        // Perpetual pump: keeps invoked property sheets alive on this thread.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ATLAS_SHELLMENU => {
            drain_requests(hwnd);
            return LRESULT(0);
        }
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            // Forward to dynamic-menu handlers (Open with, Send to, …). Read-only
            // borrow: safe even during a nested invoke loop.
            let handled = WORKER.with(|w| {
                let ws = w.borrow();
                let am = ws.as_ref().and_then(|s| s.active.as_ref())?;
                if let Some(icm3) = &am.icm3 {
                    let mut res = LRESULT(0);
                    if icm3
                        .HandleMenuMsg2(msg, wparam, lparam, Some(&mut res))
                        .is_ok()
                    {
                        return Some(res);
                    }
                }
                if let Some(icm2) = &am.icm2 {
                    if icm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                        return Some(LRESULT(0));
                    }
                }
                None
            });
            if let Some(res) = handled {
                return res;
            }
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Pull every queued request and process it. Requests are copied out under a
/// short lock so the worker state is free during processing.
unsafe fn drain_requests(hwnd: HWND) {
    loop {
        let req = WORKER.with(|w| {
            w.borrow()
                .as_ref()
                .and_then(|ws| {
                    ws.requests.lock().ok().and_then(|mut q| {
                        // Real requests jump ahead of background prewarming.
                        let pos = q
                            .iter()
                            .position(|r| !matches!(r, ShellMenuRequest::Prewarm { .. }))
                            .unwrap_or(0);
                        q.remove(pos)
                    })
                })
        });
        let Some(req) = req else { break };
        process(hwnd, req);
    }
}

unsafe fn process(hwnd: HWND, req: ShellMenuRequest) {
    match req {
        ShellMenuRequest::Build { id, paths } => match build_menu(hwnd, id, &paths) {
            Some((am, items)) => {
                set_active(am);
                push_event(ShellMenuEvent::Built { id, items });
            }
            None => push_event(ShellMenuEvent::Built { id, items: vec![] }),
        },
        ShellMenuRequest::ExpandSubmenu { id, item_id } => {
            let items = expand_submenu(id, item_id);
            push_event(ShellMenuEvent::SubmenuExpanded { id, item_id, items });
        }
        ShellMenuRequest::Invoke {
            id,
            item_id,
            screen_pt,
        } => {
            let result = invoke(hwnd, id, item_id, screen_pt);
            push_event(ShellMenuEvent::InvokeDone { id, result });
        }
        ShellMenuRequest::Prewarm { key, path } => {
            if let Some((_menu, items)) = build_menu(hwnd, 0, &[path]) {
                if !items.is_empty() {
                    push_event(ShellMenuEvent::Prewarmed { key, items });
                }
            }
        }
        ShellMenuRequest::Dismiss { id } => {
            WORKER.with(|w| {
                let mut ws = w.borrow_mut();
                if let Some(s) = ws.as_mut() {
                    if s.active.as_ref().is_some_and(|a| a.id == id) {
                        s.active = None; // Drop → DestroyMenu
                    }
                }
            });
        }
    }
}

/// Replace the live menu, dropping (and destroying) any previous one.
fn set_active(am: ActiveMenu) {
    WORKER.with(|w| {
        if let Some(s) = w.borrow_mut().as_mut() {
            s.active = Some(am);
        }
    });
}

/// Acquire the `IContextMenu` for `paths`, query it into a fresh popup HMENU,
/// and enumerate the result. Returns `None` for cross-parent or virtual paths.
unsafe fn build_menu(
    hwnd: HWND,
    id: u64,
    paths: &[PathBuf],
) -> Option<(ActiveMenu, Vec<NativeMenuItem>)> {
    if paths.is_empty() {
        return None;
    }
    let parent = paths[0].parent()?;
    // All paths must share this parent (the caller groups them).
    if paths.iter().any(|p| p.parent() != Some(parent)) {
        return None;
    }

    let icm = acquire_context_menu(hwnd, parent, paths).ok()?;
    let icm2 = icm.cast::<IContextMenu2>().ok();
    let icm3 = icm.cast::<IContextMenu3>().ok();

    use windows::Win32::UI::WindowsAndMessaging::CreatePopupMenu;
    let hmenu = CreatePopupMenu().ok()?;
    let hr = icm.QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, CMF_NORMAL);
    if hr.is_err() {
        let _ = DestroyMenu(hmenu);
        return None;
    }

    let items = walk_menu(hmenu, &icm, ID_FIRST, ID_LAST);

    Some((
        ActiveMenu {
            id,
            icm,
            icm2,
            icm3,
            hmenu,
            id_first: ID_FIRST,
        },
        items,
    ))
}

/// `SHParseDisplayName(parent)` → `IShellFolder`, parse each child name relative
/// to it, then `GetUIObjectOf(IContextMenu)` over the child PIDL array.
unsafe fn acquire_context_menu(
    hwnd: HWND,
    parent: &Path,
    paths: &[PathBuf],
) -> windows::core::Result<IContextMenu> {
    let parent_wide = to_wide(parent.as_os_str());
    let mut parent_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    SHParseDisplayName(
        PCWSTR(parent_wide.as_ptr()),
        None,
        &mut parent_pidl,
        0,
        None,
    )?;

    let folder_res: windows::core::Result<IShellFolder> =
        SHBindToObject(None, parent_pidl, None::<&IBindCtx>);
    CoTaskMemFree(Some(parent_pidl as *const c_void));
    let folder = folder_res?;

    // Parse each file name into a child PIDL relative to the parent folder.
    let mut child_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    for p in paths {
        let Some(name) = p.file_name() else { continue };
        let name_wide = to_wide(name);
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let r = folder.ParseDisplayName(
            hwnd,
            None,
            PCWSTR(name_wide.as_ptr()),
            None,
            &mut child,
            std::ptr::null_mut(),
        );
        if r.is_ok() && !child.is_null() {
            child_pidls.push(child);
        }
    }
    if child_pidls.is_empty() {
        return Err(windows::core::Error::from_win32());
    }

    let apidl: Vec<*const ITEMIDLIST> = child_pidls
        .iter()
        .map(|p| *p as *const ITEMIDLIST)
        .collect();
    let icm: windows::core::Result<IContextMenu> = folder.GetUIObjectOf(hwnd, &apidl, None);
    for c in child_pidls {
        CoTaskMemFree(Some(c as *const c_void));
    }
    icm
}

/// Lazily enumerate a submenu's children (depth ≥ 2). Returns empty if the menu
/// id is stale or the item has no submenu.
unsafe fn expand_submenu(id: u64, item_id: u32) -> Vec<NativeMenuItem> {
    WORKER.with(|w| {
        let ws = w.borrow();
        let Some(am) = ws.as_ref().and_then(|s| s.active.as_ref()) else {
            return vec![];
        };
        if am.id != id {
            return vec![];
        }
        // Find the submenu HMENU for `item_id` by walking the tree.
        let Some(hsub) = find_submenu(am.hmenu, am.id_first + item_id) else {
            return vec![];
        };
        super::enumerate::walk_submenu(
            hsub,
            &am.icm,
            am.icm2.as_ref(),
            am.icm3.as_ref(),
            am.id_first,
            ID_LAST,
        )
    })
}

/// Depth-first search for the submenu HMENU owned by command `id`.
unsafe fn find_submenu(hmenu: HMENU, target_id: u32) -> Option<HMENU> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuItemInfoW, MENUITEMINFOW, MIIM_ID, MIIM_SUBMENU,
    };
    let count = GetMenuItemCount(Some(hmenu));
    for i in 0..count {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_SUBMENU,
            ..Default::default()
        };
        if GetMenuItemInfoW(hmenu, i as u32, true, &mut info).is_err() {
            continue;
        }
        if !info.hSubMenu.is_invalid() {
            let submenu_handle = info.hSubMenu.0 as usize as u32;
            if info.wID == target_id || submenu_handle == target_id {
                return Some(info.hSubMenu);
            }
            if let Some(found) = find_submenu(info.hSubMenu, target_id) {
                return Some(found);
            }
        }
    }
    None
}

/// Invoke command `item_id` (offset by `id_first`) at `screen_pt`.
unsafe fn invoke(_hwnd: HWND, id: u64, item_id: u32, screen_pt: (i32, i32)) -> Result<(), String> {
    // Clone the COM handle out under a short borrow; do NOT hold it across the
    // invoke (which re-enters wndproc via the nested loop).
    let cloned = WORKER.with(|w| {
        let ws = w.borrow();
        let am = ws.as_ref().and_then(|s| s.active.as_ref())?;
        if am.id != id {
            return None;
        }
        Some((am.icm.clone(), am.id_first))
    });
    let Some((icm, id_first)) = cloned else {
        return Err("menú expirado".into());
    };

    let verb = make_int_resource(item_id); // command id, not a string verb
    let pt = POINT {
        x: screen_pt.0,
        y: screen_pt.1,
    };
    let mut info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        lpVerb: windows::core::PCSTR(verb as *const u8),
        lpVerbW: windows::core::PCWSTR(verb as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    let _ = id_first; // item_id is already the 0-based offset into id_first..
    icm.InvokeCommand(&mut info as *mut _ as *const _)
        .map_err(|e| e.message().to_string())
}

/// `MAKEINTRESOURCE(id)`: the low word of a pointer is the command offset.
fn make_int_resource(id: u32) -> usize {
    (id & 0xFFFF) as usize
}

/// UTF-16, NUL-terminated, for Win32 wide-string args.
fn to_wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}
