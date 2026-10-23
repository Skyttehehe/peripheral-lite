//! peripheral-lite-gui: a small skinnable battery widget + tray front end over the peripheral-lite library.
//! Not affiliated with or endorsed by Logitech.
//!
//! Threads: the UI thread runs eframe; one worker thread owns the `watch::Monitor` and does all
//! HID I/O (requests can block for seconds while a wireless device wakes up), so the UI never
//! blocks. Commands go UI -> worker over a channel plus `hid::Waker`; updates come back over a
//! channel plus `request_repaint`. While the window is hidden nothing is repainted and the
//! worker sleeps in the same WaitForMultipleObjects as `peripheral-lite watch`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod demo;
mod fonts;
mod glow;
mod history;
mod icons;
mod os;
mod skin;
mod tray;
mod update;
mod view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use history::History;
use peripheral_lite::device::{Event, MODE_ONBOARD};
use peripheral_lite::hid::Waker;
use peripheral_lite::watch::{Cmd, Monitor, Snapshot, Update};
use peripheral_lite::{autostart, battery, config, hidpp, migrate, platform};
use skin::{Settings, Show, Skin};

const TITLE: &str = "peripheral-lite";
/// Messages under the controls disappear after this long.
const MESSAGE_TIME: Duration = Duration::from_secs(8);

enum ToWorker {
    Exec(String, Cmd),
    ApplyPresets(bool),
}

enum FromWorker {
    Update(Update),
    // boxed: a Snapshot is several times larger than the other messages
    Snapshot(String, Result<Box<Snapshot>, String>),
    Fatal(String),
}

/// The update bar under the title: hidden (Idle), offering a release, or installing it.
pub enum UpdateState {
    Idle,
    Available(update::Release),
    Installing(update::Release),
}

enum UpdateMsg {
    Checked(Result<Option<update::Release>, String>),
    Installed(update::Release, Result<(), String>),
}

type IconState = (Option<icons::Level>, Option<icons::Level>, Option<icons::Level>);

struct Row {
    sel: String,
    name: String,
    battery: Option<String>,
    percent: Option<u8>,
    online: bool,
    protocol: Option<(u8, u8)>,
    history: History,
    /// The low-battery alert fired and the level has not risen above the threshold since.
    alerted: bool,
    /// Demo mode: fixed (time left, drain, status) texts.
    demo: Option<[&'static str; 3]>,
}

/// The charge state word of a battery line like `95% level=full discharging external=none`:
/// "discharging", "charging" (also "slow charging"), "full" or "error". Matched as a whole word,
/// so `level=full` (the level range) is not taken for a full charge.
fn charge_state(battery: &str) -> Option<&'static str> {
    battery.split_whitespace().find_map(|w| match w {
        "discharging" => Some("Discharging"),
        "charging" => Some("Charging"),
        "full" => Some("Full"),
        "error" => Some("Charge error"),
        _ => None,
    })
}

impl Row {
    /// On external power: charging or charged.
    fn charging(&self) -> bool {
        self.battery
            .as_deref()
            .and_then(charge_state)
            .is_some_and(|s| s == "Charging" || s == "Full")
    }
}

/// Flags set by tray callbacks (which run outside `logic`) and consumed in `logic`.
#[derive(Default)]
struct TrayFlags {
    show: AtomicBool,
    skins: AtomicBool,
    quit: AtomicBool,
}

struct App {
    tx: Sender<ToWorker>,
    rx: Receiver<FromWorker>,
    waker: Arc<Waker>,
    rows: Vec<Row>,
    selected: Option<String>,
    snap: Option<Snapshot>,
    pending: usize,
    dpi_input: String,
    /// (is_error, text, when)
    message: Option<(bool, String, Instant)>,
    fatal: Option<String>,
    tray: Option<tray::Tray>,
    tray_tip: String,
    /// What the window and tray icons show now: (lowest, first device, second device).
    icon_state: Option<IconState>,
    /// Our window, for setting its icons directly (0 if unknown).
    hwnd: isize,
    /// False on Wayland, where a window cannot be hidden (winit ignores `Visible(false)`):
    /// closing then minimises the window instead.
    can_hide: bool,
    /// Started with `--tray` where the window cannot start hidden (Wayland): minimise it on
    /// the first frame, once the window exists.
    minimise_at_start: bool,
    flags: Arc<TrayFlags>,
    quitting: bool,
    autostart_gui: bool,
    bg_running: bool,
    /// Folder of the preset file: battery curves, history, skins and app settings live here.
    dir: PathBuf,
    /// For a voltage-only device on the estimate: (model, G HUB curve file if found).
    ghub_curve: Option<(u16, Option<PathBuf>)>,
    /// After a DPI/report-rate change succeeds, save the values as the model's preset.
    save_preset: bool,
    skins: Vec<Skin>,
    settings: Settings,
    drawer: bool,
    show_skins: bool,
    skin_msg: Option<(bool, String)>,
    /// A custom skin was edited; saved once editing pauses.
    skin_dirty: Option<Instant>,
    /// `--demo`: the design's sample devices, no hardware access.
    demo: Option<Vec<Snapshot>>,
    /// Height of the skin manager panel at the last frame.
    manager_h: f32,
    update: UpdateState,
    update_rx: Option<Receiver<UpdateMsg>>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let start_hidden = args.iter().any(|a| a == "--tray");
    let cfg = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .or_else(config::default_path)
        .unwrap_or_else(|| PathBuf::from("presets.txt"));

    let demo = demo::Options::parse(&args);
    // before the instance lock: an old lghub-lite app still running is stopped here
    let migrated = if demo.is_none() {
        migrate::run("gui")
    } else {
        Vec::new()
    };
    // One app per session; the lock is held until the process exits.
    let _instance = match demo {
        Some(_) => None,
        None => match instance_lock(args.iter().any(|a| a == "--updated")) {
            Some(lock) => Some(lock),
            None => {
                os::focus_existing(TITLE);
                return;
            }
        },
    };
    if demo.is_none() {
        update::clean_up();
    }
    let bg_running = demo.is_none() && platform::instance_running("bg");

    let big = os::icon_sizes().1;
    let (w, h, rgba) = (big, big, icons::app(big, None));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(if demo.is_some() { "peripheral-lite demo" } else { TITLE })
            .with_app_id("peripheral-lite") // matches packaging/linux/peripheral-lite.desktop
            .with_inner_size([view::WIDGET_W, 420.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_drag_and_drop(true)
            .with_icon(egui::IconData {
                rgba,
                width: w,
                height: h,
            })
            .with_visible(!start_hidden),
        ..Default::default()
    };
    let _ = eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc, cfg, bg_running, demo.is_some());
            app.minimise_at_start = start_hidden && !app.can_hide;
            if !migrated.is_empty() {
                app.say(false, migrated.join("; "));
            }
            match demo {
                Some(d) => d.apply(&mut app),
                None => app.check_for_update(&cc.egui_ctx),
            }
            Ok(Box::new(app))
        }),
    );
}

/// An earlier version of the app ran with this folder: it left app settings or battery history.
fn used_before(dir: &Path) -> bool {
    dir.join("app.txt").exists()
        || std::fs::read_dir(dir).is_ok_and(|d| {
            d.flatten().any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("history-") && name.ends_with(".csv")
            })
        })
}

/// The single-instance lock. Right after an update the old copy is still exiting: wait for it.
fn instance_lock(updated: bool) -> Option<platform::InstanceLock> {
    let until = Instant::now() + Duration::from_secs(if updated { 10 } else { 0 });
    loop {
        if let Some(lock) = platform::instance_lock("gui") {
            return Some(lock);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: PathBuf, bg_running: bool, demo: bool) -> App {
        let ctx = cc.egui_ctx.clone();
        fonts::install(&ctx);
        let (mut hwnd, mut can_hide) = (0isize, true);
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => {
                    hwnd = w.hwnd.get();
                    os::full_client_area(hwnd as _);
                }
                Ok(RawWindowHandle::Wayland(_)) => can_hide = false,
                _ => {}
            }
        }
        // demo mode keeps its settings and skins away from the real ones
        let dir = if demo {
            std::env::temp_dir().join("peripheral-lite-demo")
        } else {
            cfg.parent().map(PathBuf::from).unwrap_or_default()
        };
        let (tx, worker_rx) = channel();
        let (worker_tx, rx) = channel();
        let waker = Arc::new(Waker::new().expect("cannot create event"));
        if !demo {
            let (waker, ctx, cfg) = (waker.clone(), ctx.clone(), cfg.clone());
            std::thread::spawn(move || worker(cfg, worker_rx, worker_tx, waker, ctx, !bg_running));
        }
        let flags = Arc::new(TrayFlags::default());
        if !demo {
            let (flags, ctx) = (flags.clone(), ctx.clone());
            os::serve_show_requests(Box::new(move || {
                flags.show.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }));
        }
        let tray = tray::build(&ctx, flags.clone());
        let autostart_gui = autostart::status()
            .ok()
            .flatten()
            .is_some_and(|c| c.contains("peripheral-lite-gui"));
        let mut skins = skin::builtins();
        skins.extend(skin::load_custom(&dir.join("skins")));
        let mut settings = Settings::load(&dir.join("app.txt"));
        // First start of a version with optional parts: an update keeps them, a new install
        // starts without. Saved right away, so the choice sticks.
        let decide_show = !demo && settings.show.is_none();
        if decide_show {
            settings.show = Some(if used_before(&dir) { Show::ALL } else { Show::default() });
        }
        let mut app = App {
            tx,
            rx,
            waker,
            rows: Vec::new(),
            selected: None,
            snap: None,
            pending: 0,
            dpi_input: String::new(),
            message: None,
            fatal: None,
            tray,
            tray_tip: String::new(),
            icon_state: None,
            hwnd,
            can_hide,
            minimise_at_start: false,
            flags,
            quitting: false,
            autostart_gui,
            bg_running,
            dir,
            ghub_curve: None,
            save_preset: false,
            skins,
            settings,
            drawer: false,
            show_skins: false,
            skin_msg: None,
            skin_dirty: None,
            demo: None,
            manager_h: 720.0,
            update: UpdateState::Idle,
            update_rx: None,
        };
        if decide_show {
            app.save_settings();
        }
        app
    }

    /// The optional parts of the widget that are shown.
    fn show(&self) -> Show {
        self.settings.show.unwrap_or_default()
    }

    fn skin(&self) -> &Skin {
        self.skins
            .iter()
            .find(|s| s.id == self.settings.skin)
            .unwrap_or(&self.skins[0])
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.dir.join("app.txt")) {
            self.say(true, format!("settings not saved: {e}"));
        }
    }

    fn say(&mut self, err: bool, text: String) {
        self.message = Some((err, text, Instant::now()));
    }

    /// Alert level for a device: saved, else 15 % for voltage-only headsets and 20 % otherwise.
    fn alert_level(&self, sel: &str) -> u8 {
        self.settings.alert(sel).unwrap_or_else(|| {
            let voltage_only = self
                .snap
                .as_ref()
                .is_some_and(|s| s.sel == sel && s.voltage_mv.is_some())
                || self
                    .rows
                    .iter()
                    .any(|r| r.sel == sel && r.battery.as_deref().is_some_and(|b| b.contains(" mV")));
            if voltage_only { 15 } else { 20 }
        })
    }

    fn send(&mut self, sel: &str, cmd: Cmd) {
        if let Some(snaps) = &self.demo {
            self.snap = snaps.iter().find(|s| s.sel == sel).cloned();
            return;
        }
        // A Corsair headset takes ~100 ms per request: show the new setting at once (a failure
        // refreshes it from the headset again).
        if let Some(h) = self
            .snap
            .as_mut()
            .filter(|s| s.sel == sel)
            .and_then(|s| s.headset.as_mut())
        {
            use peripheral_lite::corsair::prop;
            *h = match cmd {
                Cmd::SetLights(on) => h.with(prop::BRIGHTNESS as u16, if on { 1000 } else { 0 }),
                Cmd::SetAutoOff(0) => h.with(prop::SLEEP_ENABLED as u16, 0),
                Cmd::SetAutoOff(m) => h
                    .with(prop::SLEEP_ENABLED as u16, 1)
                    .with(prop::SLEEP_MS as u16, m * 60_000),
                _ => *h,
            };
        }
        self.pending += 1;
        let _ = self.tx.send(ToWorker::Exec(sel.to_string(), cmd));
        self.waker.wake();
    }

    fn refresh_selected(&mut self) {
        if let Some(sel) = self.selected.clone() {
            self.send(&sel, Cmd::Refresh);
        }
    }

    fn select(&mut self, sel: String) {
        if self.selected.as_deref() != Some(sel.as_str()) {
            self.selected = Some(sel);
            self.snap = None;
            self.message = None;
            self.refresh_selected();
        }
    }

    /// New battery reading for a row: remember it, add it to the history, check the alert.
    fn set_battery(&mut self, sel: &str, text: Option<String>, percent: Option<u8>) {
        let level = self.alert_level(sel);
        let Some(r) = self.rows.iter_mut().find(|r| r.sel == sel) else {
            return;
        };
        r.battery = text;
        r.percent = percent;
        let Some(p) = percent else { return };
        r.online = true; // a battery reading means the device answers (e.g. a headset back on)
        r.history.add(history::now(), p);
        if p > level {
            r.alerted = false;
        } else if !r.alerted && !r.charging() {
            r.alerted = true;
            let msg = format!("{} battery low: {p}%", r.name);
            self.say(true, msg);
            self.flags.show.store(true, Ordering::Relaxed); // bring the widget up once
        }
    }

    fn on_update(&mut self, u: Update) {
        let mut refresh = false;
        match u {
            Update::Added {
                sel,
                name,
                protocol,
                battery,
                battery_percent,
            } => {
                refresh = self.selected.as_deref() == Some(sel.as_str());
                match self.rows.iter_mut().find(|r| r.sel == sel) {
                    Some(r) => {
                        r.name = name;
                        r.online = true;
                        r.protocol = protocol.hidpp();
                    }
                    None => {
                        let history = History::load(&self.dir, &sel);
                        self.rows.push(Row {
                            sel: sel.clone(),
                            name,
                            battery: None,
                            percent: None,
                            online: true,
                            protocol: protocol.hidpp(),
                            history,
                            alerted: false,
                            demo: None,
                        })
                    }
                }
                self.set_battery(&sel, battery, battery_percent);
            }
            Update::Offline { sel, .. } => {
                if let Some(r) = self.rows.iter_mut().find(|r| r.sel == sel) {
                    r.online = false;
                }
            }
            Update::Removed { sels, .. } => {
                self.rows.retain(|r| !sels.contains(&r.sel));
                if self.selected.as_ref().is_some_and(|s| sels.contains(s)) {
                    self.selected = None;
                    self.snap = None;
                }
            }
            Update::Battery { sel, text, percent } => {
                refresh = self.selected.as_deref() == Some(sel.as_str());
                self.set_battery(&sel, Some(text), percent);
            }
            Update::Event { sel, event } => {
                match &event {
                    Event::Connection { online: false, .. } => {
                        if let Some(r) = self.rows.iter_mut().find(|r| r.sel == sel) {
                            r.online = false;
                        }
                    }
                    Event::Battery(b) => self.set_battery(&sel, Some(b.to_string()), b.percent()),
                    _ => {}
                }
                refresh = self.selected.as_deref() == Some(sel.as_str())
                    && matches!(event, Event::Dpi { .. } | Event::ReportRate { .. });
            }
            Update::Preset { sel, .. } => refresh = self.selected.as_deref() == Some(sel.as_str()),
            // a Corsair headset announced a changed setting (mic boom, ...): show it at once
            Update::Headset { sel, settings } => {
                if let Some(s) = self.snap.as_mut().filter(|s| s.sel == sel) {
                    s.headset = Some(settings);
                }
            }
            Update::Line(_) | Update::Receiver { .. } => {}
        }
        if self.selected.is_none()
            && let Some(r) = self.rows.iter().find(|r| r.online)
        {
            self.selected = Some(r.sel.clone());
            refresh = true;
        }
        if refresh {
            self.refresh_selected();
        }
    }

    fn on_snapshot(&mut self, sel: String, r: Result<Snapshot, String>) {
        self.pending = self.pending.saturating_sub(1);
        match r {
            Ok(s) => {
                // A DPI/rate change went through: keep it as the model's preset, so it comes
                // back whenever the mouse connects (host-mode settings do not survive on it).
                if std::mem::take(&mut self.save_preset) && s.mode != Some(MODE_ONBOARD) {
                    let (dpi, rate) = (s.dpi.as_ref().map(|d| d.current), s.rate.as_ref().map(|r| r.0));
                    if s.preset.as_ref().is_none_or(|p| p.dpi != dpi || p.rate != rate) {
                        self.send(&sel, Cmd::SavePreset { dpi, rate });
                    }
                }
                if self.selected.as_deref() != Some(sel.as_str()) {
                    return;
                }
                if let Some(d) = &s.dpi {
                    self.dpi_input = d.current.to_string();
                }
                if s.needs_curve
                    && let Some(m) = s.model
                    && self.ghub_curve.as_ref().is_none_or(|g| g.0 != m)
                {
                    self.ghub_curve = Some((m, battery::find_ghub_curve(m)));
                }
                if let Some(row) = self.rows.iter_mut().find(|r| r.sel == sel) {
                    row.battery = s.battery.clone();
                    row.percent = s.battery_percent;
                }
                self.snap = Some(s);
            }
            Err(e) => {
                self.save_preset = false;
                self.say(true, e);
                // undo a value shown ahead of the device (see `send`)
                if self.snap.as_ref().is_some_and(|s| s.sel == sel && s.headset.is_some()) {
                    self.send(&sel, Cmd::Refresh);
                }
            }
        }
    }

    fn update_tray(&mut self, ctx: &egui::Context) {
        let parts: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.online)
            .map(|r| match r.percent {
                Some(p) => format!("{} {p}%", r.name),
                None => r.name.clone(),
            })
            .collect();
        let tip = if parts.is_empty() {
            TITLE.to_string()
        } else {
            format!("{TITLE}\n{}", parts.join("\n"))
        };
        if let Some(t) = &self.tray
            && tip != self.tray_tip
        {
            t.set_tooltip(&tip);
        }
        self.tray_tip = tip;

        // Icons: the window/taskbar battery shows the lowest device, the tray one bar each
        // for the first two devices (in the order of the list).
        let levels: Vec<Option<icons::Level>> = self
            .rows
            .iter()
            .map(|r| {
                let pct = r.percent.filter(|_| r.online)?;
                Some(icons::Level {
                    pct,
                    low: pct <= self.alert_level(&r.sel),
                })
            })
            .collect();
        let lowest = levels.iter().flatten().min_by_key(|l| l.pct).copied();
        let state = (
            lowest,
            levels.first().copied().flatten(),
            levels.get(1).copied().flatten(),
        );
        if self.icon_state != Some(state) {
            self.icon_state = Some(state);
            let (small, big) = os::icon_sizes();
            if let Some(t) = &self.tray {
                t.set_levels(state.1, state.2);
            }
            // winit only updates the small icon at run time; the taskbar shows the big one
            if self.hwnd != 0 {
                os::set_window_icons(
                    self.hwnd as _,
                    &icons::app(small, lowest),
                    small,
                    &icons::app(big, lowest),
                    big,
                );
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(egui::IconData {
                    rgba: icons::app(big, lowest),
                    width: big,
                    height: big,
                }))));
            }
        }
    }

    /// Whether closing the window keeps the app running in the tray: only with Tray (start at
    /// login, in the tray) on, and a tray to go to.
    pub fn hides_to_tray(&self) -> bool {
        self.tray.is_some() && self.autostart_gui
    }

    fn set_autostart(&mut self, on: bool) {
        let exe = std::env::current_exe().unwrap_or_default();
        let r = if on {
            autostart::enable(&format!("\"{}\" --tray", exe.display())).and_then(|_| {
                // The GUI now takes over from the background watcher.
                if self.bg_running {
                    autostart::stop_running(&platform::exe("peripheral-lite-bg"))?;
                    self.bg_running = false;
                    let _ = self.tx.send(ToWorker::ApplyPresets(true));
                    self.waker.wake();
                }
                Ok(())
            })
        } else {
            autostart::disable()
        };
        match r {
            Ok(()) => self.autostart_gui = on,
            Err(e) => self.say(true, format!("start at login: {e}")),
        }
    }
}

impl App {
    /// Ask GitHub for a newer release, once at start, on its own thread; `poll_update` takes
    /// the result and shows the update bar unless that release was skipped.
    fn check_for_update(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Checked(update::check()));
            ctx.request_repaint();
        });
        self.update_rx = Some(rx);
    }

    /// "Update" in the update bar: install and restart, or (where installing is not supported)
    /// open the release page.
    pub fn update_clicked(&mut self, ctx: &egui::Context) {
        let UpdateState::Available(r) = std::mem::replace(&mut self.update, UpdateState::Idle) else {
            return;
        };
        if self.demo.is_some() {
            self.say(false, format!("Demo: would update to {}", r.tag));
            self.update = UpdateState::Available(r);
            return;
        }
        if !update::CAN_INSTALL {
            update::open_page(&r);
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        self.update = UpdateState::Installing(r.clone());
        std::thread::spawn(move || {
            let res = update::install_dir().and_then(|dir| update::install(&r, &dir));
            let _ = tx.send(UpdateMsg::Installed(r, res));
            ctx.request_repaint();
        });
        self.update_rx = Some(rx);
    }

    /// "Not now" hides the bar until the next start; "Skip version" until a newer release.
    pub fn update_dismissed(&mut self, skip: bool) {
        if let UpdateState::Available(r) = std::mem::replace(&mut self.update, UpdateState::Idle)
            && skip
        {
            self.settings.skip = r.tag;
            self.save_settings();
        }
    }

    fn poll_update(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.update_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.update_rx = None;
        match msg {
            // no newer release, or no network: nothing to say at start
            UpdateMsg::Checked(Ok(None) | Err(_)) => {}
            UpdateMsg::Checked(Ok(Some(r))) => {
                if r.tag != self.settings.skip {
                    self.update = UpdateState::Available(r);
                }
            }
            UpdateMsg::Installed(r, Err(e)) => {
                self.say(true, format!("Update failed: {e}"));
                self.update = UpdateState::Available(r);
            }
            UpdateMsg::Installed(r, Ok(())) => match self.restart() {
                Ok(()) => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(e) => {
                    self.say(
                        true,
                        format!("{} installed; restart peripheral-lite to use it ({e})", r.tag),
                    );
                    self.update = UpdateState::Idle;
                }
            },
        }
    }

    /// Start the new copy (it waits for this one to exit) and the background watcher if it ran.
    fn restart(&mut self) -> std::io::Result<()> {
        let dir = update::install_dir().map_err(std::io::Error::other)?;
        if self.bg_running {
            let bg = platform::exe("peripheral-lite-bg");
            autostart::stop_running(&bg)?;
            autostart::start(&format!("\"{}\"", dir.join(&bg).display()))?;
        }
        let mut cmd = format!(
            "\"{}\" --updated",
            dir.join(platform::exe("peripheral-lite-gui")).display()
        );
        for a in std::env::args().skip(1).filter(|a| a != "--tray" && a != "--updated") {
            cmd += &format!(" \"{a}\"");
        }
        autostart::start(&cmd)
    }
}

fn worker(
    cfg: PathBuf,
    rx: Receiver<ToWorker>,
    tx: Sender<FromWorker>,
    waker: Arc<Waker>,
    ctx: egui::Context,
    apply_presets: bool,
) {
    let send = |m: FromWorker| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };
    let (mut mon, first) = match Monitor::new(hidpp::WATCH_SWID, false, &cfg) {
        Ok(v) => v,
        Err(e) => return send(FromWorker::Fatal(e.to_string())),
    };
    mon.set_apply_presets(apply_presets);
    for u in first {
        send(FromWorker::Update(u));
    }
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ToWorker::Exec(sel, cmd) => {
                    let r = mon.exec(&sel, cmd).map(Box::new).map_err(|e| e.to_string());
                    send(FromWorker::Snapshot(sel, r));
                }
                ToWorker::ApplyPresets(on) => mon.set_apply_presets(on),
            }
        }
        match mon.next(Some(&waker)) {
            Ok(updates) => updates.into_iter().for_each(|u| send(FromWorker::Update(u))),
            Err(e) => {
                send(FromWorker::Update(Update::Line(format!("error: {e}"))));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

impl eframe::App for App {
    /// Runs before every frame, and also while the window is hidden whenever a repaint is
    /// requested (worker updates, tray clicks), so the tray works with the window hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                FromWorker::Update(u) => self.on_update(u),
                FromWorker::Snapshot(sel, r) => self.on_snapshot(sel, r.map(|s| *s)),
                FromWorker::Fatal(e) => self.fatal = Some(e),
            }
        }
        self.poll_update(ctx);
        self.update_tray(ctx);
        if self.message.as_ref().is_some_and(|m| m.2.elapsed() > MESSAGE_TIME) {
            self.message = None;
        }
        if let Some(t) = self.skin_dirty {
            if t.elapsed() > Duration::from_millis(600) {
                self.skin_dirty = None;
                let (dir, k) = (self.dir.join("skins"), self.skin().clone());
                if let Err(e) = skin::save(&dir, &k) {
                    self.skin_msg = Some((false, format!("Skin not saved: {e}")));
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(650));
            }
        }

        if std::mem::take(&mut self.minimise_at_start) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        if self.flags.skins.swap(false, Ordering::Relaxed) {
            self.show_skins = true;
            self.flags.show.store(true, Ordering::Relaxed);
        }
        if self.flags.show.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.flags.quit.swap(false, Ordering::Relaxed) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // With Tray on, closing the window hides it to the tray (minimises it on Wayland) and
        // Quit (tray menu) really exits; with Tray off, closing quits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.hides_to_tray() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(if self.can_hide {
                egui::ViewportCommand::Visible(false)
            } else {
                egui::ViewportCommand::Minimized(true)
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // transparent: the widget paints its own rounded frame
    }
}

#[cfg(test)]
mod tests {
    use super::charge_state;

    #[test]
    fn level_full_is_not_a_charge_state() {
        assert_eq!(
            charge_state("95% level=full discharging external=none"),
            Some("Discharging")
        );
        assert_eq!(
            charge_state("40% level=good slow charging external=wired"),
            Some("Charging")
        );
        assert_eq!(charge_state("100% level=full full external=wired"), Some("Full"));
        assert_eq!(
            charge_state("≈62%  (3858 mV, flags=0x01, generic Li-ion estimate)"),
            None
        );
    }
}
