//! Windows helpers: bringing an existing window forward, file dialogs, window icons,
//! the borderless-window fix, icon sizes.

use std::path::PathBuf;

use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_SHOW, SetForegroundWindow, ShowWindow};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Windows finds the running window by title instead (`focus_existing`); nothing to serve.
pub fn serve_show_requests(_show: Box<dyn Fn() + Send>) {}

pub fn focus_existing(title: &str) {
    let t = wide(title);
    // SAFETY: NUL-terminated title; the window belongs to our other instance.
    unsafe {
        let w = FindWindowW(std::ptr::null(), t.as_ptr());
        if !w.is_null() {
            ShowWindow(w, SW_SHOW);
            SetForegroundWindow(w);
        }
    }
}

/// winit gives borderless windows a 1 px top strip for the system drop shadow, which
/// shifts the content down by a pixel and draws a dark line. Take the whole window as
/// client area instead (the widget paints its own edge).
/// Set the window's small (title/alt-tab) and big (taskbar) icons from RGBA images.
pub fn set_window_icons(hwnd: windows_sys::Win32::Foundation::HWND, small: &[u8], ss: u32, big: &[u8], bs: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateIcon, DestroyIcon, HICON, ICON_BIG, ICON_SMALL, SendMessageW, WM_SETICON,
    };
    fn make(rgba: &[u8], size: u32) -> HICON {
        // 32-bit colour bits are BGRA; the AND mask is all zero (the alpha channel rules)
        let bgra: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let mask_row = (size as usize).div_ceil(16) * 2;
        let mask = vec![0u8; mask_row * size as usize];
        // SAFETY: buffers match the stated size and bit depth; null instance is allowed.
        unsafe {
            CreateIcon(
                std::ptr::null_mut(),
                size as i32,
                size as i32,
                1,
                32,
                mask.as_ptr(),
                bgra.as_ptr(),
            )
        }
    }
    for (kind, rgba, size) in [(ICON_SMALL, small, ss), (ICON_BIG, big, bs)] {
        let icon = make(rgba, size);
        if icon.is_null() {
            continue;
        }
        // SAFETY: our own window; WM_SETICON returns the previous icon, which we created.
        unsafe {
            let old = SendMessageW(hwnd, WM_SETICON, kind as usize, icon as isize);
            if old != 0 {
                DestroyIcon(old as HICON);
            }
        }
    }
}

pub fn full_client_area(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, WM_NCCALCSIZE,
    };

    unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM, _: usize, _: usize) -> LRESULT {
        if msg == WM_NCCALCSIZE && w != 0 {
            return 0; // client rect = window rect
        }
        // SAFETY: forwarding the message unchanged to the next handler.
        unsafe { DefSubclassProc(h, msg, w, l) }
    }
    // SAFETY: hwnd is our live top-level window; the subclass lives as long as it does.
    unsafe {
        SetWindowSubclass(hwnd, Some(proc), 1, 0);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
}

/// The standard Windows open/save dialog for skin files. `save` proposes `name`.
pub fn skin_file_dialog(save: bool, name: &str) -> Option<PathBuf> {
    let filter: Vec<u16> = "Skin files (*.json)\0*.json\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut buf = [0u16; 1024];
    for (i, c) in name.encode_utf16().take(buf.len() - 1).enumerate() {
        buf[i] = c;
    }
    let ext = wide("json");
    // SAFETY: zeroed is a valid OPENFILENAMEW; the pointers below outlive the call.
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_NOCHANGEDIR | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST };
    // SAFETY: ofn is fully initialised for the call.
    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// What the widget says when the OS will not let us open the devices.
pub const NO_ACCESS: [&str; 2] = [
    "Windows blocks access to the devices.",
    "Another program may hold them.",
];
pub const ACCESS_SETTINGS: Option<&str> = None;

/// Icon sizes Windows uses at the system DPI: (tray / small, window / taskbar).
pub fn icon_sizes() -> (u32, u32) {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SM_CXICON, SM_CXSMICON};
    // SAFETY: plain queries without pointers.
    unsafe {
        let dpi = GetDpiForSystem();
        let small = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16) as u32;
        let big = GetSystemMetricsForDpi(SM_CXICON, dpi).max(32) as u32;
        (small, big)
    }
}
