//! `watch`: long-running, event-driven monitor. `Monitor::next` blocks in a single wait
//! (`hid::wait_any`) on every open HID++ collection plus the PnP notification (and
//! an optional cross-thread `Waker`), so it uses no CPU while nothing happens. The
//! exceptions are batteries read on a timer: headsets that report battery only as a voltage
//! (0x1F20) every BATTERY_POLL, because no unsolicited 0x1F20 event has been seen on hardware
//! (G HUB polls it too), and percentage batteries (0x1004/0x1000/0x1001) every QUICK_POLL,
//! because some devices never send their battery event: an MX Master 3S over Bluetooth on
//! macOS sent none when its charging cable was plugged in. One short read costs nothing
//! measurable.
//! Whenever a device attaches (start-up, plug-in, receiver link-up, wake from sleep) its preset
//! from the preset file is applied; the file is re-read each time, so edits need no restart.
//!
//! The CLI (`run`) prints each `Update`; the GUI consumes the same updates as data and sends
//! commands through `Monitor::exec`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::audio::{self, Adc};
use crate::battery::{self, Curve};
use crate::centurion::{self, Centurion};
use crate::config::{self, Preset};
use crate::corsair::{self, Corsair};
use crate::device::{self, Dpi, EVENT_FEATURES, Event};
use crate::hid::{self, Notifier, Signal, Wake, Waker};
use crate::hidpp::{self, DIRECT, Error, GET_REGISTER, Hidpp};
use crate::sound::{self, Tone};

/// Local wall-clock time for log lines, e.g. `2026-10-15 22:31:02`.
pub fn now() -> String {
    crate::platform::local_time_text()
}

/// `println!` with a timestamp, so watch output (and the background log) is a history.
macro_rules! say {
    ($($t:tt)*) => { println!("{} {}", now(), format_args!($($t)*)) };
}

/// Something that happened, as data. `Display` gives the CLI's `watch` line.
/// How peripheral-lite talks to a device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Protocol {
    /// HID++ (major, minor).
    Hidpp(u8, u8),
    /// Logitech Centurion headset (battery only).
    Centurion,
    /// Corsair Bragi headset (battery only).
    Corsair,
}

impl Protocol {
    pub fn hidpp(self) -> Option<(u8, u8)> {
        match self {
            Protocol::Hidpp(a, b) => Some((a, b)),
            _ => None,
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Protocol::Hidpp(a, b) => write!(f, "HID++ {a}.{b}"),
            Protocol::Centurion => f.write_str("Centurion headset"),
            Protocol::Corsair => f.write_str("Corsair headset"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Update {
    /// Informational line (receiver flags, open errors, ...).
    Line(String),
    Receiver {
        pid: u16,
        product: String,
    },
    Added {
        sel: String,
        name: String,
        protocol: Protocol,
        /// Battery text as shown by `battery`, or "unreadable: ..."; None if no battery.
        battery: Option<String>,
        battery_percent: Option<u8>,
    },
    Offline {
        sel: String,
        reason: String,
    },
    /// An interface disappeared; `sels` are the devices that were on it.
    Removed {
        pid: u16,
        product: String,
        reason: Option<String>,
        sels: Vec<String>,
    },
    Event {
        sel: String,
        event: Event,
    },
    Battery {
        sel: String,
        text: String,
        percent: Option<u8>,
    },
    Preset {
        sel: String,
        result: Result<String, String>,
    },
    /// A Corsair headset's settings changed (an event, e.g. the mic boom).
    Headset {
        sel: String,
        settings: crate::corsair::Settings,
    },
    /// The OS does not let us open this interface (macOS: Input Monitoring; Linux: the udev
    /// rule). Reported once; it is tried again every QUICK_POLL until it opens.
    NoAccess {
        pid: u16,
        product: String,
        reason: String,
    },
}

impl fmt::Display for Update {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Update::Line(s) => f.write_str(s),
            Update::NoAccess { pid, product, reason } => write!(f, "! {pid:04x}  {product}  cannot open: {reason}"),
            Update::Receiver { pid, product } => write!(f, "+ {pid:04x}  {product}  (receiver)"),
            Update::Added {
                sel,
                name,
                protocol,
                battery,
                ..
            } => {
                write!(f, "+ {sel:<8} {name}  ({protocol})")?;
                match battery {
                    Some(b) => write!(f, "  battery {b}"),
                    None => Ok(()),
                }
            }
            Update::Offline { sel, reason } => write!(f, "  {sel:<8} offline ({reason})"),
            Update::Removed {
                pid, product, reason, ..
            } => {
                write!(f, "- {pid:04x}  {product}  removed")?;
                match reason {
                    Some(r) => write!(f, " ({r})"),
                    None => Ok(()),
                }
            }
            Update::Event { sel, event } => write!(f, "  {sel:<8} {event}"),
            Update::Battery { sel, text, .. } => write!(f, "  {sel:<8} battery {text}"),
            Update::Preset { sel, result: Ok(s) } => write!(f, "  {sel:<8} preset applied: {s}"),
            Update::Preset { sel, result: Err(e) } => write!(f, "  {sel:<8} preset failed: {e}"),
            Update::Headset { sel, settings } => write!(f, "  {sel:<8} {settings}"),
        }
    }
}

/// Commands the GUI sends to the monitor's thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    Refresh,
    SetDpi(u16),
    SetRate(u32),
    SetMode(u8),
    /// Corsair headset: all LEDs (logo and mic) on or off.
    SetLights(bool),
    /// Corsair headset: auto-off after this many idle minutes, 0 = never.
    SetAutoOff(u32),
    /// Build or refine the measured battery curve from the voltage recording.
    BuildCurve,
    /// Save the given values as this model's preset (None clears that field).
    SavePreset {
        dpi: Option<u16>,
        rate: Option<u32>,
    },
}

/// Everything the GUI shows for one device, read fresh from the hardware.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub sel: String,
    pub name: String,
    pub battery: Option<String>,
    pub battery_percent: Option<u8>,
    /// Battery voltage for devices that only report a voltage (0x1F20).
    pub voltage_mv: Option<u16>,
    /// A voltage-only battery whose percentage is only the generic estimate.
    pub needs_curve: bool,
    /// Where the percentage comes from (voltage-only batteries).
    pub curve_label: Option<String>,
    /// State of the voltage recording that measured curves are built from.
    pub recording: Option<String>,
    /// Hours left in tenths, when the curve knows the full runtime (voltage-only batteries).
    pub tenths_left: Option<u32>,
    pub dpi: Option<Dpi>,
    /// (current Hz, allowed Hz)
    pub rate: Option<(u32, Vec<u32>)>,
    pub mode: Option<u8>,
    pub model: Option<u16>,
    pub preset: Option<Preset>,
    /// Settings of a Corsair headset (lighting and auto-off adjustable, mic read-only).
    pub headset: Option<corsair::Settings>,
}

/// One HID++ device on an interface: the direct device (index 0xFF) or a receiver slot.
struct Dev {
    index: u8,
    name: String,
    /// Feature index -> ID for the features whose events we decode.
    feats: Vec<(u8, u16)>,
    /// Battery reported only as a voltage (0x1F20); read on a timer.
    has_voltage: bool,
    /// Battery reported as a percentage (0x1004/0x1000/0x1001); also read on a timer.
    has_percent: bool,
    model: u16,
    /// Best curve for a voltage-only battery (always set for those).
    curve: Option<Curve>,
    /// Lowest voltage since the last charge; a clear rise above it means charging.
    low_mv: u16,
    /// Steadied voltage for display (median, no flicker upwards).
    steady: battery::Steady,
    /// What was last reported for the battery, to report only changes.
    last_battery: String,
    /// The dongle answers but the headset is off (0x1F20 NotAllowed); reported as offline.
    off: bool,
}

const BATTERY_POLL: Duration = Duration::from_secs(300);
/// Poll interval for Corsair headsets, for any polled headset while it is off (neither
/// announces being switched off or on, so the battery timer alone would take minutes to
/// notice) and for percentage batteries (charging starts and stops show within it).
const QUICK_POLL: Duration = Duration::from_secs(30);

/// One open HID++ interface (a direct device or a receiver).
struct Live {
    key: String,
    pid: u16,
    product: String,
    h: Hidpp,
    devs: Vec<Dev>,
    presets: PathBuf,
    /// Apply presets on attach (off when another watcher already does it).
    apply_presets: bool,
}

impl Live {
    /// Folder of the preset file, which also holds battery curves and recordings.
    fn dir(&self) -> PathBuf {
        self.presets.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    fn sel(&self, index: u8) -> String {
        if index == DIRECT {
            format!("{:04x}", self.pid)
        } else {
            format!("{:04x}:{index}", self.pid)
        }
    }
}

/// The protocol behind a polled headset.
enum Link {
    /// Logitech Centurion (PRO X 2 LIGHTSPEED). [INF] confirmed only through a user's report.
    Centurion(Centurion),
    /// Corsair Bragi (HS80 RGB Wireless); `events` is its event collection, waited on with the
    /// HID++ handles so charging and mic changes show at once. [HW]
    Corsair { c: Corsair, events: Option<hid::Handle> },
}

/// A headset without HID++ (Centurion or Corsair): its battery is read on the poll timer, and
/// more often while it is off so that switching it back on is noticed soon.
struct Polled {
    key: String,
    pid: u16,
    product: String,
    link: Link,
    /// Last battery read failed (headset off or out of range): reported as offline.
    off: bool,
    /// Last battery text and percentage.
    last: String,
    percent: Option<u8>,
    /// Last known settings (Corsair), kept current from sets and events.
    settings: Option<corsair::Settings>,
    /// The settings folder (the preset file's), which holds the lights choice.
    dir: PathBuf,
    /// The owner switched the lights off (`corsair::lights_off_wanted`); applied again whenever
    /// the headset comes back.
    lights_off: bool,
    /// Held while this process is the one that plays the headset's tones (`Polled::tone`).
    sounds: Option<crate::platform::InstanceLock>,
    /// Battery warning already played (`sound::battery_warning`).
    battery_warned: u8,
}

impl Polled {
    fn sel(&self) -> String {
        match self.link {
            Link::Centurion(_) => format!("{:04x}", self.pid),
            Link::Corsair { .. } => corsair::selector(self.pid),
        }
    }

    /// Read every QUICK_POLL rather than only with the battery timer.
    fn quick(&self) -> bool {
        self.off || matches!(self.link, Link::Corsair { .. })
    }

    fn protocol(&self) -> Protocol {
        match self.link {
            Link::Centurion(_) => Protocol::Centurion,
            Link::Corsair { .. } => Protocol::Corsair,
        }
    }

    /// Read the battery: (text, percent).
    fn read(&mut self) -> Result<(String, u8), Error> {
        match &mut self.link {
            Link::Centurion(c) => {
                let b = c.battery()?;
                Ok((b.to_string(), b.percent))
            }
            Link::Corsair { c, .. } => {
                let b = c.battery()?;
                Ok((b.to_string(), b.percent()))
            }
        }
    }

    /// A report from the event collection: battery changes are read and reported at once; a
    /// changed setting (mic, lighting, auto-off) is taken from the event itself, no reads.
    fn event(&mut self, r: &[u8], out: &mut Vec<Update>) {
        let Some(n) = corsair::parse_event(r) else {
            return;
        };
        crate::verbose(format_args!("{}: event {n:?}", self.sel()));
        match n.property {
            p if p == corsair::prop::BATTERY as u16 || p == corsair::prop::BATTERY_STATE as u16 => self.poll(out),
            p if n.target == corsair::HEADSET && corsair::Settings::covers(p) => {
                let s = self.settings.unwrap_or_default().with(p, n.value);
                if let Some(t) = sound::mic_tone(self.settings.and_then(|s| s.mic_muted), s.mic_muted) {
                    self.tone(t, out);
                }
                if self.settings != Some(s) {
                    self.settings = Some(s);
                    out.push(Update::Headset {
                        sel: self.sel(),
                        settings: s,
                    });
                }
            }
            _ => {}
        }
    }

    /// The owner switched this headset's lights off: apply that again (after the headset was
    /// switched on or the dongle plugged in, which brings back hardware mode and its LEDs).
    fn restore_lights(&mut self, out: &mut Vec<Update>) {
        if !self.lights_off {
            return;
        }
        let Link::Corsair { c, .. } = &mut self.link else {
            return;
        };
        match c.set_lights(false) {
            Ok(()) => {
                let s = self
                    .settings
                    .unwrap_or_default()
                    .with(corsair::prop::BRIGHTNESS as u16, 0);
                self.settings = Some(s);
                out.push(Update::Headset {
                    sel: self.sel(),
                    settings: s,
                });
            }
            Err(e) => out.push(Update::Line(format!("  {:<8} lights off failed: {e}", self.sel()))),
        }
    }

    /// Play one of peripheral-lite's own tones in place of the headset's, which stays silent while
    /// its lights are off (software mode, see `sound`). The app and the watcher both see every
    /// event, so only the process holding the "sounds" lock plays them.
    fn tone(&mut self, t: Tone, out: &mut Vec<Update>) {
        let sel = self.sel();
        if !self.lights_off {
            return crate::verbose(format_args!("{sel}: {t:?} tone: lights on, the headset plays its own"));
        }
        if self.sounds.is_none() {
            self.sounds = crate::platform::instance_lock(&format!("sounds-{:04x}", self.pid));
        }
        if self.sounds.is_none() {
            return crate::verbose(format_args!("{sel}: {t:?} tone: another peripheral-lite plays them"));
        }
        let Some((id, name)) = sound::output_for(&self.product, &crate::platform::audio_outputs()) else {
            out.push(Update::Line(format!("  {sel:<8} no audio output for its tones")));
            return;
        };
        std::thread::spawn(move || {
            let start = Instant::now();
            let r = crate::platform::play_pcm(id, sound::RATE, &sound::samples(t));
            let ms = start.elapsed().as_millis();
            match r {
                Ok(()) => crate::verbose(format_args!("{sel}: {t:?} tone played on {name} ({ms} ms)")),
                Err(e) => crate::verbose(format_args!("{sel}: {t:?} tone: {e}")),
            }
        });
    }

    /// Timer read: report changes, and offline/online transitions.
    fn poll(&mut self, out: &mut Vec<Update>) {
        match self.read() {
            Ok((text, pct)) => {
                if std::mem::take(&mut self.off) {
                    self.restore_lights(out);
                }
                self.percent = Some(pct);
                if let Some(t) = sound::battery_warning(pct, &mut self.battery_warned) {
                    self.tone(t, out);
                }
                if text != self.last {
                    self.last = text.clone();
                    out.push(Update::Battery {
                        sel: self.sel(),
                        text,
                        percent: Some(pct),
                    });
                }
            }
            Err(e) => {
                crate::verbose(format_args!("{}: battery read failed: {e}", self.sel()));
                if !self.off {
                    self.off = true;
                    self.last.clear();
                    out.push(Update::Offline {
                        sel: self.sel(),
                        reason: audio::HEADSET_OFF.into(),
                    });
                }
            }
        }
    }

    /// Run a settings command (Corsair only). The values set are read back by `Corsair::set*`,
    /// so the cached settings are updated without reading everything again.
    fn apply(&mut self, cmd: &Cmd) -> Result<(), Error> {
        let Link::Corsair { c, .. } = &mut self.link else {
            return Err(Error::Other("this headset supports battery reading only so far".into()));
        };
        use corsair::prop;
        let s = self.settings.unwrap_or_default();
        self.settings = Some(match *cmd {
            Cmd::SetLights(on) => {
                c.set_lights(on)?;
                self.lights_off = !on;
                if let Err(e) = corsair::remember_lights(&self.dir, self.pid, on) {
                    crate::verbose(format_args!("{:04x}: cannot remember the lights choice: {e}", self.pid));
                }
                s.with(prop::BRIGHTNESS as u16, if on { 1000 } else { 0 })
            }
            Cmd::SetAutoOff(0) => {
                c.set_auto_off(0)?;
                s.with(prop::SLEEP_ENABLED as u16, 0)
            }
            Cmd::SetAutoOff(minutes) => {
                c.set_auto_off(minutes)?;
                s.with(prop::SLEEP_ENABLED as u16, 1)
                    .with(prop::SLEEP_MS as u16, minutes * 60_000)
            }
            _ => return Err(Error::Other("not a setting of this headset".into())),
        });
        Ok(())
    }

    /// Read battery and settings afresh (Refresh), and remember them.
    fn snapshot(&mut self) -> Snapshot {
        match self.read() {
            Ok((t, p)) => {
                self.last = t;
                self.percent = Some(p);
                if std::mem::take(&mut self.off) {
                    // back on: the drawer gets the result through the settings read below
                    self.restore_lights(&mut Vec::new());
                }
            }
            Err(_) => self.percent = None,
        }
        if let Link::Corsair { c, .. } = &mut self.link
            && self.percent.is_some()
        {
            self.settings = c.settings().ok().or(self.settings);
        }
        self.cached()
    }

    /// The last known state, without asking the headset (each request costs ~100 ms).
    fn cached(&self) -> Snapshot {
        let (battery, battery_percent) = match self.percent {
            Some(p) if !self.off => (Some(self.last.clone()), Some(p)),
            _ => (Some(audio::HEADSET_OFF.into()), None),
        };
        let headset = if battery_percent.is_some() { self.settings } else { None };
        Snapshot {
            sel: self.sel(),
            name: self.product.clone(),
            battery,
            battery_percent,
            voltage_mv: None,
            needs_curve: false,
            curve_label: None,
            recording: None,
            tenths_left: None,
            dpi: None,
            rate: None,
            mode: None,
            model: None,
            preset: None,
            headset,
        }
    }
}

pub struct Monitor {
    notifier: Notifier,
    live: Vec<Live>,
    /// Interfaces the OS would not let us open (keys), already reported as NoAccess.
    denied: Vec<String>,
    polled: Vec<Polled>,
    swid: u8,
    trace: bool,
    presets: PathBuf,
    next_poll: Instant,
    /// Next QUICK_POLL of the headsets that need it (`Polled::quick`).
    next_quick: Instant,
    apply_presets: bool,
}

impl Monitor {
    /// Open every HID++ interface; the returned updates describe what was found.
    pub fn new(swid: u8, trace: bool, presets: &Path) -> Result<(Monitor, Vec<Update>), Error> {
        let notifier = Notifier::hid().map_err(|e| Error::Io(e.to_string()))?;
        let mut m = Monitor {
            notifier,
            live: Vec::new(),
            denied: Vec::new(),
            polled: Vec::new(),
            swid,
            trace,
            presets: presets.to_path_buf(),
            next_poll: Instant::now() + BATTERY_POLL,
            next_quick: Instant::now() + QUICK_POLL,
            apply_presets: true,
        };
        let mut out = Vec::new();
        m.sync(&mut out)?;
        Ok((m, out))
    }

    /// Turn automatic preset application on attach on or off (on by default). The GUI turns it
    /// off while `peripheral-lite-bg` is running, so only one process applies presets.
    pub fn set_apply_presets(&mut self, on: bool) {
        self.apply_presets = on;
        for l in self.live.iter_mut() {
            l.apply_presets = on;
        }
    }

    /// Selectors of all attached (online) devices.
    pub fn devices(&self) -> Vec<String> {
        self.live
            .iter()
            .flat_map(|l| l.devs.iter().map(|d| l.sel(d.index)))
            .chain(self.polled.iter().map(Polled::sel))
            .collect()
    }

    /// Block until something happens and return what did. Returns an empty list when only
    /// `waker` fired (a command is waiting for the caller).
    pub fn next(&mut self, waker: Option<&Waker>) -> Result<Vec<Update>, Error> {
        let mut out = Vec::new();
        // Reports that arrived while a transaction was waiting for its reply.
        for l in self.live.iter_mut() {
            while let Some(r) = l.h.queued() {
                handle(l, &r, self.trace, &mut out);
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
        // Map the wake-up to plain indices so the borrow of `live` ends here.
        #[derive(Clone, Copy)]
        enum Owner {
            Live(usize),
            Polled(usize),
        }
        enum Got {
            Report(Owner, Vec<u8>),
            Gone(Owner, std::io::Error),
            Pnp,
            Woken,
            Poll,
        }
        // timer reads of percentage batteries, and retries of interfaces we may not open yet
        let percent = !self.denied.is_empty() || self.live.iter().any(|l| l.devs.iter().any(|d| d.has_percent));
        let polling =
            percent || !self.polled.is_empty() || self.live.iter().any(|l| l.devs.iter().any(|d| d.has_voltage));
        let due = if percent || self.polled.iter().any(Polled::quick) {
            self.next_poll.min(self.next_quick)
        } else {
            self.next_poll
        };
        let timeout = if polling {
            // < FOREVER, so a long wait can never be mistaken for "no timeout"
            let ms = due.saturating_duration_since(Instant::now()).as_millis();
            ms.min(u32::MAX as u128 - 1) as u32
        } else {
            hid::FOREVER
        };
        let got = {
            let mut hs = Vec::new();
            let mut owner = Vec::new();
            for (k, l) in self.live.iter_mut().enumerate() {
                for h in l.h.handles() {
                    hs.push(h);
                    owner.push(Owner::Live(k));
                }
            }
            for (k, p) in self.polled.iter_mut().enumerate() {
                if let Link::Corsair { events: Some(h), .. } = &mut p.link {
                    hs.push(h);
                    owner.push(Owner::Polled(k));
                }
            }
            let mut extras: Vec<&dyn Signal> = vec![&self.notifier];
            if let Some(w) = waker {
                extras.push(w);
            }
            match hid::wait_any(&mut hs, &extras, timeout) {
                Wake::Report(i, r) => Got::Report(owner[i], r),
                Wake::Failed(i, e) => Got::Gone(owner[i], e),
                Wake::Extra(0) => Got::Pnp,
                Wake::Extra(_) => Got::Woken,
                Wake::Timeout => Got::Poll,
            }
        };
        match got {
            Got::Report(Owner::Live(k), r) => handle(&mut self.live[k], &r, self.trace, &mut out),
            Got::Report(Owner::Polled(k), r) => self.polled[k].event(&r, &mut out),
            Got::Gone(Owner::Live(k), e) => {
                let l = self.live.remove(k);
                out.push(removed(&l, Some(e.to_string())));
            }
            Got::Gone(Owner::Polled(k), e) => {
                let p = self.polled.remove(k);
                out.push(Update::Removed {
                    pid: p.pid,
                    product: p.product.clone(),
                    reason: Some(e.to_string()),
                    sels: vec![p.sel()],
                });
            }
            Got::Pnp => {
                // One plug-in raises several interface notifications; let them settle, then
                // re-enumerate once.
                self.notifier.settle(300);
                self.sync(&mut out)?;
            }
            Got::Woken => {}
            Got::Poll => {
                let now = Instant::now();
                if now >= self.next_quick {
                    self.next_quick = now + QUICK_POLL;
                    for l in self.live.iter_mut() {
                        poll_percent(l, &mut out);
                    }
                    if !self.denied.is_empty() {
                        // access may have been allowed since: no event says so
                        self.sync(&mut out)?;
                    }
                }
                if now >= self.next_poll {
                    self.next_poll = now + BATTERY_POLL;
                    for l in self.live.iter_mut() {
                        poll_battery(l, &mut out);
                    }
                    for c in self.polled.iter_mut() {
                        c.poll(&mut out);
                    }
                } else {
                    for c in self.polled.iter_mut().filter(|c| c.quick()) {
                        c.poll(&mut out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Re-enumerate HID++ interfaces: drop the ones that disappeared, open and describe new ones.
    fn sync(&mut self, out: &mut Vec<Update>) -> Result<(), Error> {
        let ifs = device::interfaces()?;
        let keys: Vec<String> = ifs.iter().map(|f| f.cols[0].iface.clone()).collect();
        let (keep, gone): (Vec<Live>, Vec<Live>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|l| keys.contains(&l.key));
        self.live = keep;
        out.extend(gone.iter().map(|l| removed(l, None)));
        for (f, key) in ifs.iter().zip(keys) {
            if self.live.iter().any(|l| l.key == key) {
                continue;
            }
            match Hidpp::open(&f.cols, self.swid, self.trace) {
                Ok(h) => {
                    let mut l = Live {
                        key,
                        pid: f.pid,
                        product: f.product.clone(),
                        h,
                        devs: Vec::new(),
                        presets: self.presets.clone(),
                        apply_presets: self.apply_presets,
                    };
                    describe(&mut l, out);
                    self.live.push(l);
                }
                Err(e @ Error::Denied(_)) => {
                    if !self.denied.contains(&key) {
                        self.denied.push(key);
                        out.push(Update::NoAccess {
                            pid: f.pid,
                            product: f.product.clone(),
                            reason: e.to_string(),
                        });
                    }
                }
                Err(e) => out.push(Update::Line(format!(
                    "! {:04x}  {}  cannot open: {e}",
                    f.pid, f.product
                ))),
            }
        }
        // forget denials of interfaces that opened since or are gone
        let live = &self.live;
        self.denied
            .retain(|k| ifs.iter().any(|f| &f.cols[0].iface == k) && !live.iter().any(|l| &l.key == k));

        // Polled headsets: Centurion (no HID++ interface of the same PID) and Corsair
        let found: Vec<(u16, String, hid::Info, Protocol)> = centurion::interfaces()
            .into_iter()
            .filter(|c| !ifs.iter().any(|f| f.pid == c.0))
            .map(|(pid, product, info)| (pid, product, info, Protocol::Centurion))
            .chain(
                corsair::interfaces()
                    .into_iter()
                    .map(|(pid, product, info)| (pid, product, info, Protocol::Corsair)),
            )
            .collect();
        let (keep, gone): (Vec<Polled>, Vec<Polled>) = std::mem::take(&mut self.polled)
            .into_iter()
            .partition(|c| found.iter().any(|f| f.2.iface == c.key));
        self.polled = keep;
        for c in gone {
            out.push(Update::Removed {
                pid: c.pid,
                product: c.product.clone(),
                reason: None,
                sels: vec![c.sel()],
            });
        }
        let dir = self.presets.parent().map(Path::to_path_buf).unwrap_or_default();
        for (pid, product, info, protocol) in found {
            if self.polled.iter().any(|c| c.key == info.iface) {
                continue;
            }
            let link = match protocol {
                Protocol::Corsair => Corsair::open(&info, self.trace).map(|c| Link::Corsair {
                    c,
                    // without it only the poll timer notices changes
                    events: corsair::event_collection(&info).and_then(|e| {
                        hid::Handle::open(&e)
                            .map_err(|err| crate::verbose(format_args!("{pid:04x}: no event collection: {err}")))
                            .ok()
                    }),
                }),
                _ => Centurion::open(&info, self.trace).map(Link::Centurion),
            };
            match link {
                Ok(link) => {
                    let mut c = Polled {
                        key: info.iface.clone(),
                        pid,
                        product,
                        link,
                        off: false,
                        last: String::new(),
                        percent: None,
                        settings: None,
                        lights_off: protocol == Protocol::Corsair && corsair::lights_off_wanted(&dir, pid),
                        dir: dir.clone(),
                        sounds: None,
                        battery_warned: 0,
                    };
                    let (battery, battery_percent) = match c.read() {
                        Ok((t, p)) => {
                            c.last = t.clone();
                            c.percent = Some(p);
                            // no warning for a battery already low when peripheral-lite starts
                            sound::battery_warning(p, &mut c.battery_warned);
                            (Some(t), Some(p))
                        }
                        Err(_) => {
                            c.off = true;
                            (Some(audio::HEADSET_OFF.into()), None)
                        }
                    };
                    out.push(Update::Added {
                        sel: c.sel(),
                        name: c.product.clone(),
                        protocol: c.protocol(),
                        battery,
                        battery_percent,
                    });
                    if !c.off {
                        c.restore_lights(out);
                    }
                    self.polled.push(c);
                }
                Err(e) => out.push(Update::Line(format!("! {pid:04x}  {product}  cannot open: {e}"))),
            }
        }
        Ok(())
    }

    fn find(&mut self, sel: &str) -> Result<(&mut Live, u8), Error> {
        for l in self.live.iter_mut() {
            if let Some(d) = l.devs.iter().find(|d| l.sel(d.index) == sel) {
                let i = d.index;
                return Ok((l, i));
            }
        }
        Err(Error::Other(format!("device {sel} is not connected")))
    }

    /// Read the current state of one device.
    pub fn snapshot(&mut self, sel: &str) -> Result<Snapshot, Error> {
        if let Some(c) = self.polled.iter_mut().find(|c| c.sel() == sel) {
            return Ok(c.snapshot());
        }
        let (l, i) = self.find(sel)?;
        let name = l
            .devs
            .iter()
            .find(|d| d.index == i)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        // A curve may have been imported or built since the last look: look again.
        let dir = l.dir();
        let (mut needs_curve, mut curve_label, mut recording) = (false, None, None);
        if let Some(d) = l.devs.iter_mut().find(|d| d.index == i && d.has_voltage) {
            let c = battery::find_curve(&dir, d.model);
            needs_curve = c.is_approx();
            curve_label = Some(c.label());
            recording = Some(battery::recording_status(&dir, d.model));
            d.curve = Some(c);
        }
        let (battery, battery_percent, voltage_mv) = battery_now(l, i);
        let tenths_left = voltage_mv.and_then(|mv| {
            let d = l.devs.iter().find(|d| d.index == i)?;
            d.curve.as_ref()?.tenths_of_hours_left(mv)
        });
        let dpi = optional(device::dpi(&mut l.h, i))?;
        let rate = optional(device::report_rate(&mut l.h, i))?.map(|r| {
            let allowed = (0..8u8)
                .filter(|b| r.supported & (1 << b) != 0)
                .map(device::rate_hz)
                .collect();
            (device::rate_hz(r.current), allowed)
        });
        let mode = device::onboard_mode(&mut l.h, i).ok().flatten();
        let model = device::model_id(&mut l.h, i).ok();
        let preset = match model {
            Some(m) => config::load(&l.presets)
                .map_err(Error::Other)?
                .into_iter()
                .find(|p| p.model == m),
            None => None,
        };
        Ok(Snapshot {
            sel: sel.to_string(),
            name,
            battery,
            battery_percent,
            voltage_mv,
            needs_curve,
            curve_label,
            recording,
            tenths_left,
            dpi,
            rate,
            mode,
            model,
            preset,
            headset: None,
        })
    }

    /// Run a GUI command against one device, then return its fresh state.
    pub fn exec(&mut self, sel: &str, cmd: Cmd) -> Result<Snapshot, Error> {
        if let Some(p) = self.polled.iter_mut().find(|c| c.sel() == sel) {
            if matches!(cmd, Cmd::Refresh) {
                return Ok(p.snapshot());
            }
            // a change: only what was set is read back; the rest comes from the cache
            p.apply(&cmd)?;
            return Ok(p.cached());
        }
        {
            let (l, i) = self.find(sel)?;
            match &cmd {
                Cmd::Refresh => {}
                Cmd::BuildCurve => {
                    let dir = l.dir();
                    let d = l
                        .devs
                        .iter_mut()
                        .find(|d| d.index == i && d.has_voltage)
                        .ok_or_else(|| Error::Other("this device reports no battery voltage".into()))?;
                    match battery::update_measured(&dir, d.model).map_err(Error::Other)? {
                        Some(c) => d.curve = Some(c),
                        None => {
                            return Err(Error::Other(
                                "the measured curve already includes the latest recorded discharge".into(),
                            ));
                        }
                    }
                }
                Cmd::SetDpi(v) => {
                    device::set_dpi_verified(&mut l.h, i, *v)?;
                }
                Cmd::SetRate(hz) => {
                    device::set_rate_verified(&mut l.h, i, *hz)?;
                }
                Cmd::SetMode(m) => {
                    device::set_mode_verified(&mut l.h, i, *m)?;
                }
                Cmd::SetLights(_) | Cmd::SetAutoOff(_) => {
                    return Err(Error::Other("only Corsair headsets have this setting".into()));
                }
                Cmd::SavePreset { dpi, rate } => {
                    let model = device::model_id(&mut l.h, i)?;
                    let mut list = config::load(&l.presets).map_err(Error::Other)?;
                    list.retain(|p| p.model != model);
                    if dpi.is_some() || rate.is_some() {
                        list.push(Preset {
                            model,
                            dpi: *dpi,
                            rate: *rate,
                        });
                    }
                    config::save(&l.presets, &list).map_err(Error::Other)?;
                }
            }
        }
        self.snapshot(sel)
    }
}

/// NoFeature -> Ok(None): the device simply does not have that setting.
fn optional<T>(r: Result<T, Error>) -> Result<Option<T>, Error> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Error::NoFeature(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn removed(l: &Live, reason: Option<String>) -> Update {
    Update::Removed {
        pid: l.pid,
        product: l.product.clone(),
        reason,
        sels: l.devs.iter().map(|d| l.sel(d.index)).collect(),
    }
}

/// Battery text, percent and (for voltage-only batteries) millivolts for a device:
/// 0x1004/0x1000/0x1001, or 0x1F20 with a curve.
fn battery_now(l: &mut Live, index: u8) -> (Option<String>, Option<u8>, Option<u16>) {
    if l.devs.iter().any(|d| d.index == index && d.has_voltage) {
        return match audio::adc(&mut l.h, index) {
            Ok(a) => {
                let mut ignored = Vec::new();
                report_voltage(l, index, &a, &mut ignored);
                let d = l.devs.iter().find(|d| d.index == index);
                let shown = d.and_then(|d| d.steady.shown());
                match (d.and_then(|d| d.curve.as_ref()), shown) {
                    (Some(c), Some(mv)) => {
                        let s = Adc { mv, flags: a.flags };
                        (Some(audio::voltage_text(&s, c)), Some(c.percent(mv)), Some(mv))
                    }
                    _ => (Some("not reported right now".into()), None, None),
                }
            }
            Err(e) if audio::is_headset_off(&e) => (Some(audio::HEADSET_OFF.into()), None, None),
            Err(e) => (Some(format!("unreadable: {e}")), None, None),
        };
    }
    match device::battery(&mut l.h, index) {
        Ok(b) => (Some(b.to_string()), b.percent(), None),
        Err(Error::NoFeature(_)) => (None, None, None),
        Err(e) => (Some(format!("unreadable: {e}")), None, None),
    }
}

/// CLI `watch`: print every update with a timestamp until Ctrl+C.
pub fn run(swid: u8, trace: bool, presets: &Path) -> Result<(), Error> {
    say!("presets    {}", presets.display());
    let (mut m, first) = Monitor::new(swid, trace, presets)?;
    for u in first {
        say!("{u}");
    }
    say!("watching for device and setting changes (Ctrl+C to stop)");
    loop {
        for u in m.next(None)? {
            say!("{u}");
        }
    }
}

/// Identify a newly opened interface; for receivers, attach every online paired device.
fn describe(l: &mut Live, out: &mut Vec<Update>) {
    match l.h.ping(DIRECT, 0x5A) {
        Ok(_) => attach(l, DIRECT, out),
        Err(Error::Hpp10(_)) => {
            // HID++ 1.0 answer to a 2.0 ping: a receiver. [OBS §9.6]
            out.push(Update::Receiver {
                pid: l.pid,
                product: l.product.clone(),
            });
            // Notification flags (register 0x00) are only read, never written. [OBS §9.8]
            out.push(Update::Line(
                match l.h.read_register(DIRECT, GET_REGISTER, 0x00, [0, 0, 0], &[]) {
                    Ok(p) => format!("  notification flags (reg 0x00): {}", hidpp::hex(&p[..3.min(p.len())])),
                    Err(e) => format!("  notification flags (reg 0x00): {e}"),
                },
            ));
            for slot in 0..6u8 {
                if device::pairing(&mut l.h, slot).is_ok() {
                    attach(l, slot + 1, out);
                }
            }
        }
        Err(e) => out.push(Update::Line(format!(
            "+ {:04x}  {}  [no HID++ 2.0 answer: {e}]",
            l.pid, l.product
        ))),
    }
}

/// Ping a device, report its name and battery, and learn the indices of the event features.
fn attach(l: &mut Live, index: u8, out: &mut Vec<Update>) {
    let sel = l.sel(index);
    let protocol = match l.h.ping(index, 0x5A) {
        Ok(v) => v,
        // [HW] the receiver answers 0x8F code 0x08 at once for a paired device whose link is down.
        Err(Error::Hpp10(0x08)) => {
            out.push(Update::Offline {
                sel,
                reason: "paired, not connected".into(),
            });
            return;
        }
        Err(e) => {
            out.push(Update::Offline {
                sel,
                reason: e.to_string(),
            });
            return;
        }
    };
    let name = device::name(&mut l.h, index).unwrap_or_else(|_| l.product.clone());
    let mut feats = Vec::new();
    for id in EVENT_FEATURES {
        if let Ok((i, _, _)) = l.h.get_feature(index, id)
            && i != 0
        {
            feats.push((i, id));
        }
    }
    let has_voltage = feats.iter().any(|f| f.1 == 0x1F20);
    let mut dev = Dev {
        index,
        name: name.clone(),
        feats,
        has_voltage,
        has_percent: false,
        model: 0,
        curve: None,
        low_mv: u16::MAX,
        steady: battery::Steady::default(),
        off: false,
        last_battery: String::new(),
    };
    let (battery, battery_percent) = if has_voltage {
        let dir = l.dir();
        dev.model = device::model_id(&mut l.h, index).unwrap_or(l.pid);
        let adc = audio::adc(&mut l.h, index);
        if let Ok(a) = &adc {
            learn(&dir, &mut dev, a, &sel, out);
        }
        // The headset may have been charged while nothing was running; the reading just
        // recorded then closes the discharge before it, so build from it now.
        match battery::update_measured(&dir, dev.model) {
            Ok(Some(_)) => out.push(Update::Line(format!(
                "  {sel:<8} battery curve updated from the recorded discharge"
            ))),
            Ok(None) => {}
            Err(e) => crate::verbose(format_args!("{sel}: no new measured curve: {e}")),
        }
        let c = battery::find_curve(&dir, dev.model);
        let r = match adc {
            Ok(a) => match dev.steady.push(battery::unix_now(), a.mv) {
                Some(mv) => {
                    let s = Adc { mv, flags: a.flags };
                    dev.last_battery = battery_key(&s, Some(&c));
                    (Some(audio::voltage_text(&s, &c)), Some(c.percent(mv)))
                }
                None => (Some("not reported right now".into()), None),
            },
            Err(e) if audio::is_headset_off(&e) => {
                dev.off = true;
                (Some(audio::HEADSET_OFF.into()), None)
            }
            Err(e) => (Some(format!("unreadable: {e}")), None),
        };
        dev.curve = Some(c);
        r
    } else {
        match device::battery(&mut l.h, index) {
            Ok(b) => {
                dev.has_percent = b.percent().is_some();
                dev.last_battery = b.to_string();
                (Some(b.to_string()), b.percent())
            }
            Err(_) => (None, None),
        }
    };
    out.push(Update::Added {
        sel,
        name,
        protocol: Protocol::Hidpp(protocol.0, protocol.1),
        battery,
        battery_percent,
    });
    l.devs.retain(|d| d.index != index);
    l.devs.push(dev);
    if l.apply_presets {
        apply_preset(l, index, out);
    }
}

/// Apply this device's preset, if any. Errors are reported, never fatal: `watch` keeps running.
fn apply_preset(l: &mut Live, index: u8, out: &mut Vec<Update>) {
    let sel = l.sel(index);
    let list = match config::load(&l.presets) {
        Ok(v) => v,
        Err(e) => {
            out.push(Update::Line(format!("  {sel:<8} presets not loaded: {e}")));
            return;
        }
    };
    if list.is_empty() {
        return;
    }
    let model = match device::model_id(&mut l.h, index) {
        Ok(m) => m,
        Err(e) => {
            out.push(Update::Line(format!(
                "  {sel:<8} model ID unknown, preset not applied: {e}"
            )));
            return;
        }
    };
    let Some(p) = list.iter().find(|p| p.model == model) else {
        return;
    };
    let result = config::apply(&mut l.h, index, p).map_err(|e| e.to_string());
    out.push(Update::Preset { sel, result });
}

fn handle(l: &mut Live, r: &[u8], trace: bool, out: &mut Vec<Update>) {
    if r.len() < 4 {
        return;
    }
    let feats: &[(u8, u16)] = l.devs.iter().find(|d| d.index == r[1]).map_or(&[], |d| &d.feats);
    let Some(event) = device::decode_event(r, feats) else {
        if trace {
            eprintln!("<- {}  (not an event we decode)", hidpp::hex(r));
        }
        return;
    };
    if trace {
        eprintln!("<- {}", hidpp::hex(r));
    }
    match event {
        Event::Connection {
            index, online: true, ..
        } => {
            out.push(Update::Event {
                sel: l.sel(index),
                event,
            });
            attach(l, index, out);
        }
        Event::Disconnected { index } => {
            out.push(Update::Event {
                sel: l.sel(index),
                event,
            });
            l.devs.retain(|d| d.index != index);
        }
        Event::Voltage { mv, flags } => report_voltage(l, r[1], &Adc { mv, flags }, out),
        Event::Battery(ref b) => {
            // remembered, so the next timer read does not report it again
            if let Some(d) = l.devs.iter_mut().find(|d| d.index == r[1]) {
                d.last_battery = b.to_string();
            }
            out.push(Update::Event {
                sel: l.sel(r[1]),
                event,
            });
        }
        _ => out.push(Update::Event {
            sel: l.sel(r[1]),
            event,
        }),
    }
}

/// Change key for battery reporting: the percentage with a curve, else 10 mV steps.
fn battery_key(a: &Adc, curve: Option<&Curve>) -> String {
    match curve {
        Some(c) => format!("{}%", c.percent(a.mv)),
        None => format!("{}", a.mv / 10),
    }
}

/// Record a voltage reading and report it if it changed what the user sees.
fn report_voltage(l: &mut Live, index: u8, a: &Adc, out: &mut Vec<Update>) {
    let sel = l.sel(index);
    let dir = l.dir();
    let Some(d) = l.devs.iter_mut().find(|d| d.index == index) else {
        return;
    };
    learn(&dir, d, a, &sel, out);
    let Some(mv) = d.steady.push(battery::unix_now(), a.mv) else {
        return;
    };
    d.off = false; // a valid reading: the headset is back
    let s = Adc { mv, flags: a.flags };
    let key = battery_key(&s, d.curve.as_ref());
    if key != d.last_battery
        && let Some(c) = &d.curve
    {
        d.last_battery = key;
        out.push(Update::Battery {
            sel,
            text: audio::voltage_text(&s, c),
            percent: Some(c.percent(mv)),
        });
    }
}

/// Add the reading to the voltage recording. When charging starts after a discharge (a clear
/// rise over the lowest voltage seen), build or refine the measured curve from the recording.
fn learn(dir: &Path, d: &mut Dev, a: &Adc, sel: &str, out: &mut Vec<Update>) {
    if !battery::valid_mv(a.mv) {
        crate::verbose(format_args!("{sel}: ignoring battery reading {} mV", a.mv));
        return;
    }
    if let Err(e) = battery::record(dir, d.model, a.mv, a.flags) {
        crate::verbose(format_args!("{sel}: voltage recording failed: {e}"));
    }
    if d.low_mv != u16::MAX && a.mv > d.low_mv.saturating_add(battery::CHARGE_RISE_MV) {
        d.low_mv = a.mv;
        match battery::update_measured(dir, d.model) {
            Ok(Some(c)) => {
                out.push(Update::Line(format!(
                    "  {sel:<8} battery curve updated from the recorded discharge"
                )));
                d.curve = Some(c);
            }
            Ok(None) => {}
            Err(e) => crate::verbose(format_args!("{sel}: no new measured curve: {e}")),
        }
    } else {
        d.low_mv = d.low_mv.min(a.mv);
    }
}

/// Timer read for voltage-only batteries. A failed read (e.g. headset switched off) is only
/// shown with --verbose; watch keeps running.
fn poll_battery(l: &mut Live, out: &mut Vec<Update>) {
    let idx: Vec<u8> = l.devs.iter().filter(|d| d.has_voltage).map(|d| d.index).collect();
    for i in idx {
        match audio::adc(&mut l.h, i) {
            Ok(a) => report_voltage(l, i, &a, out),
            Err(e) if audio::is_headset_off(&e) => {
                let sel = l.sel(i);
                if let Some(d) = l.devs.iter_mut().find(|d| d.index == i)
                    && !d.off
                {
                    d.off = true;
                    d.last_battery.clear(); // so the next good reading is reported again
                    out.push(Update::Offline {
                        sel,
                        reason: audio::HEADSET_OFF.into(),
                    });
                }
            }
            Err(e) => crate::verbose(format_args!("battery read failed: {e}")),
        }
    }
}

/// Timer read of percentage batteries: report what changed since the last reading or event.
fn poll_percent(l: &mut Live, out: &mut Vec<Update>) {
    let idx: Vec<u8> = l.devs.iter().filter(|d| d.has_percent).map(|d| d.index).collect();
    for i in idx {
        match device::battery(&mut l.h, i) {
            Ok(b) => {
                let text = b.to_string();
                let sel = l.sel(i);
                if let Some(d) = l.devs.iter_mut().find(|d| d.index == i)
                    && d.last_battery != text
                {
                    d.last_battery = text.clone();
                    out.push(Update::Battery {
                        sel,
                        text,
                        percent: b.percent(),
                    });
                }
            }
            // a receiver's device that went to sleep: its disconnect event says so
            Err(e) => crate::verbose(format_args!("battery read failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The CLI `watch` lines must stay exactly as before the Monitor refactor ([HW] traces).
    #[test]
    fn update_lines_match_cli_watch() {
        let cases: Vec<(Update, &str)> = vec![
            (
                Update::Receiver {
                    pid: 0xC54D,
                    product: "USB Receiver".into(),
                },
                "+ c54d  USB Receiver  (receiver)",
            ),
            (
                Update::Added {
                    sel: "c54d:1".into(),
                    name: "PRO X 2".into(),
                    protocol: Protocol::Hidpp(4, 2),
                    battery: Some("54% level=good discharging external=none".into()),
                    battery_percent: Some(54),
                },
                "+ c54d:1   PRO X 2  (HID++ 4.2)  battery 54% level=good discharging external=none",
            ),
            (
                Update::Added {
                    sel: "0af7".into(),
                    name: "PRO X 2 LIGHTSPEED".into(),
                    protocol: Protocol::Centurion,
                    battery: Some("93% discharging".into()),
                    battery_percent: Some(93),
                },
                "+ 0af7     PRO X 2 LIGHTSPEED  (Centurion headset)  battery 93% discharging",
            ),
            (
                Update::Added {
                    sel: "1b1c:0a73".into(),
                    name: "CORSAIR HS80 RGB Wireless Gaming Receiver".into(),
                    protocol: Protocol::Corsair,
                    battery: Some("31% discharging (3657 mV)".into()),
                    battery_percent: Some(31),
                },
                "+ 1b1c:0a73 CORSAIR HS80 RGB Wireless Gaming Receiver  (Corsair headset)  battery 31% discharging (3657 mV)",
            ),
            (
                Update::Added {
                    sel: "0aba".into(),
                    name: "H".into(),
                    protocol: Protocol::Hidpp(4, 2),
                    battery: None,
                    battery_percent: None,
                },
                "+ 0aba     H  (HID++ 4.2)",
            ),
            (
                Update::Offline {
                    sel: "c54d:1".into(),
                    reason: "paired, not connected".into(),
                },
                "  c54d:1   offline (paired, not connected)",
            ),
            (
                Update::Removed {
                    pid: 0xC54D,
                    product: "USB Receiver".into(),
                    reason: Some("x".into()),
                    sels: vec![],
                },
                "- c54d  USB Receiver  removed (x)",
            ),
            (
                Update::Removed {
                    pid: 0xC09B,
                    product: "PRO X 2".into(),
                    reason: None,
                    sels: vec![],
                },
                "- c09b  PRO X 2  removed",
            ),
            (
                Update::Event {
                    sel: "c54d:1".into(),
                    event: Event::Dpi { x: 800, y: 800, lod: 2 },
                },
                "  c54d:1   dpi 800 (y 800, lod 2)",
            ),
            (
                Update::Battery {
                    sel: "0aba".into(),
                    text: "77%".into(),
                    percent: Some(77),
                },
                "  0aba     battery 77%",
            ),
            (
                Update::Preset {
                    sel: "c54d:1".into(),
                    result: Ok("host mode, dpi 1200".into()),
                },
                "  c54d:1   preset applied: host mode, dpi 1200",
            ),
            (
                Update::Preset {
                    sel: "c54d:1".into(),
                    result: Err("boom".into()),
                },
                "  c54d:1   preset failed: boom",
            ),
        ];
        for (u, want) in cases {
            assert_eq!(u.to_string(), want);
        }
    }
}
