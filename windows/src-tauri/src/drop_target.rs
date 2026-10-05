// Files dragged onto the island, received by Coucou itself.
//
// wry installs its drop target by walking the webview's child windows once,
// when the webview is created; WebView2 builds its own windows afterwards and
// keeps one of them registered with a target that refuses every file (the page
// has no HTML5 drop handler). Revoking just that one wasn't enough: whatever
// OLE fell back to never fed Tauri's drag events, so nothing dropped on the
// island ever arrived. So Coucou registers its own target on every window of
// the island, re-asserted whenever a drag might be starting, and sends the
// island a `file-drag` event shaped like Tauri's own drag-drop payload.

use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::core::{implement, Ref, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, POINTL, RECT};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowRect};

use crate::island;

const EVENT: &str = "file-drag";

#[derive(Serialize, Clone)]
struct Position {
    x: i32,
    y: i32,
}

/// Same shape as Tauri's drag-drop event payload.
#[derive(Serialize, Clone)]
struct Payload {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<Position>,
}

#[implement(IDropTarget)]
struct IslandDropTarget {
    app: AppHandle,
    /// The island window, whose top left the positions are measured from.
    top: HWND,
    /// Whether the drag in progress carries files; anything else is refused.
    files: Cell<bool>,
}

impl IslandDropTarget {
    fn send(&self, kind: &'static str, paths: Option<Vec<String>>, pt: Option<&POINTL>) {
        let position = pt.map(|pt| {
            let mut r = RECT::default();
            let _ = unsafe { GetWindowRect(self.top, &mut r) };
            Position { x: pt.x - r.left, y: pt.y - r.top }
        });
        let _ = self.app.emit_to(island::WINDOW_LABEL, EVENT, Payload { kind, paths, position });
    }
}

/// The files a drag carries, or None when it carries no files.
fn paths_of(data: Ref<'_, IDataObject>) -> Option<Vec<String>> {
    let data = data.as_ref()?;
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let mut medium = data.GetData(&format).ok()?;
        let hdrop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(OsString::from_wide(&buf[..len]).to_string_lossy().to_string());
        }
        ReleaseStgMedium(&mut medium);
        Some(paths)
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for IslandDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = paths_of(data);
        self.files.set(paths.is_some());
        unsafe { *effect = if paths.is_some() { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if paths.is_some() {
            self.send("enter", paths, Some(pt));
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let files = self.files.get();
        unsafe { *effect = if files { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if files {
            self.send("over", None, Some(pt));
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if self.files.replace(false) {
            self.send("leave", None, None);
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = paths_of(data);
        unsafe { *effect = if paths.is_some() { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if self.files.replace(false) {
            self.send("drop", paths, Some(pt));
        }
        Ok(())
    }
}

/// Puts Coucou's own drop target on every window inside the island, replacing
/// whatever wry or WebView2 registered there. Main thread only (OLE needs the
/// thread that owns the windows). Cheap and idempotent, so it simply runs
/// again whenever a drag might be starting.
pub fn install(app: &AppHandle) {
    let Some(win) = island::window(app) else { return };
    let Some(top) = island::hwnd_of(&win) else { return };
    let target: IDropTarget = IslandDropTarget { app: app.clone(), top, files: Cell::new(false) }.into();
    unsafe {
        let _ = EnumChildWindows(Some(top), Some(register), LPARAM(&target as *const IDropTarget as isize));
    }
}

unsafe extern "system" fn register(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let target = unsafe { &*(lparam.0 as *const IDropTarget) };
    unsafe {
        let _ = RevokeDragDrop(hwnd);
        // OLE keeps its own reference until the next revoke.
        let _ = RegisterDragDrop(hwnd, target);
    }
    true.into()
}
