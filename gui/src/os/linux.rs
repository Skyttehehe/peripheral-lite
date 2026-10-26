//! Linux helpers: reaching an already running instance, file dialogs via zenity/kdialog,
//! window icons through winit, icon sizes.
//! [INF] Written without a Linux desktop to try it on; built and type-checked only.

use std::path::PathBuf;
use std::process::{Command, Stdio};

pub use super::unix::{focus_existing, serve_show_requests};

/// Window icons are set through winit (`ViewportCommand::Icon`) on Linux; Wayland
/// compositors take the icon from the .desktop file instead.
pub fn set_window_icons(_hwnd: isize, _small: &[u8], _ss: u32, _big: &[u8], _bs: u32) {}

/// Windows-only fix for a winit quirk; nothing to do here.
pub fn full_client_area(_hwnd: isize) {}

/// Pick a skin file to open, or a place to save one, with zenity (GNOME, most desktops) or
/// kdialog (KDE). None if cancelled or neither is installed; dropping a file on the window
/// works everywhere.
pub fn skin_file_dialog(save: bool, name: &str) -> Option<PathBuf> {
    let zenity = {
        let mut c = Command::new("zenity");
        c.args([
            "--file-selection",
            "--title=Skin file",
            "--file-filter=Skin files | *.json",
        ]);
        if save {
            c.args(["--save", "--confirm-overwrite", &format!("--filename={name}")]);
        }
        c
    };
    let kdialog = {
        let mut c = Command::new("kdialog");
        let start = if save { name } else { "." };
        c.args([
            if save { "--getsavefilename" } else { "--getopenfilename" },
            start,
            "*.json",
        ]);
        c
    };
    for mut c in [zenity, kdialog] {
        let Ok(out) = c.stderr(Stdio::null()).output() else {
            continue; // not installed: try the next one
        };
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path));
    }
    None
}

/// What the widget says when the OS will not let us open the devices.
pub const NO_ACCESS: [&str; 2] = ["No access to the devices.", "Install the udev rule (see the README)."];
pub const ACCESS_SETTINGS: Option<&str> = None;

/// (tray, window) icon sizes. The tray gets several sizes (see tray/linux.rs); the window
/// icon is scaled by the desktop.
pub fn icon_sizes() -> (u32, u32) {
    (32, 64)
}
