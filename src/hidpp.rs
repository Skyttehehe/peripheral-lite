//! HID++ framing, request/response matching, error decoding and feature lookup.
//! Evidence pointers are in NOTES.md; section numbers refer to ../protocol.md.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use crate::hid::{self, Handle};

// Report IDs and total lengths (incl. report ID). [OBS §9.3] [HW receiver c54d caps: 7 / 20]
pub const SHORT: u8 = 0x10;
pub const LONG: u8 = 0x11;
pub const VERY_LONG: u8 = 0x12;
pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;
pub const VERY_LONG_LEN: usize = 64;

/// Device index for a directly connected device or the receiver itself. [OBS §9.3]
pub const DIRECT: u8 = 0xFF;

// HID++ 1.0 sub-IDs. [OBS §9.3, §9.5]
pub const GET_REGISTER: u8 = 0x81;
pub const GET_LONG_REGISTER: u8 = 0x83;
pub const ERR10: u8 = 0x8F;
// HID++ 2.0 error "feature index". [OBS §9.5]
pub const ERR20: u8 = 0xFF;

/// Our software ID for one-shot CLI commands. G HUB uses a random 0x0A..0x0E [OBS §9.4];
/// 0x03 keeps us out of that range. [INF] choice, not a protocol constant.
pub const DEFAULT_SWID: u8 = 0x03;

/// The long-running watchers (app, peripheral-lite-bg) use their own ID: every program reading a
/// device sees every input report, and with the same ID a CLI command could take a watcher's
/// reply to the same request (e.g. two root GetFeature calls) for its own [HW 2026-10-16: a
/// DPI read-back saw another feature's version while the app refreshed after a DPI event].
pub const WATCH_SWID: u8 = 0x04;

pub fn report_id_for_len(len: usize) -> Option<u8> {
    match len {
        SHORT_LEN => Some(SHORT),
        LONG_LEN => Some(LONG),
        VERY_LONG_LEN => Some(VERY_LONG),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Io(String),
    /// The OS refused to open the device (macOS: Input Monitoring not allowed; Linux: no
    /// udev rule). Shown like Io.
    Denied(String),
    Timeout,
    /// HID++ 1.0 error (sub-ID 0x8F), code from byte 5.
    Hpp10(u8),
    /// HID++ 2.0 error (feature index 0xFF), code from byte 5.
    Hpp20(u8),
    /// Reply too short or otherwise malformed for the request.
    BadReply(&'static str),
    NoFeature(u16),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(s) | Error::Denied(s) => write!(f, "I/O: {s}"),
            Error::Timeout => write!(f, "no reply (timeout)"),
            Error::Hpp10(c) => write!(f, "HID++ 1.0 error {c:#04x} ({})", hpp10_error_name(*c)),
            Error::Hpp20(c) => write!(f, "HID++ 2.0 error {c:#04x} ({})", hpp20_error_name(*c)),
            Error::BadReply(s) => write!(f, "malformed reply: {s}"),
            Error::NoFeature(id) => write!(f, "feature {id:#06x} not supported by device"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

/// HID++ 2.0 error codes. [OBS §9.5]
pub fn hpp20_error_name(c: u8) -> &'static str {
    match c {
        0 => "NoError",
        1 => "Unknown",
        2 => "InvalidArgument",
        3 => "OutOfRange",
        4 => "HwError",
        5 => "NotAllowed",
        6 => "InvalidFeatureIndex",
        7 => "InvalidFunctionId",
        8 => "Busy",
        9 => "Unsupported",
        _ => "?",
    }
}

/// HID++ 1.0 error codes. [PUB Solaar hidpp10_constants.ERROR]; G HUB only shows
/// that byte 5 is the code [OBS §9.5].
pub fn hpp10_error_name(c: u8) -> &'static str {
    match c {
        0x01 => "invalid SubID/command",
        0x02 => "invalid address",
        0x03 => "invalid value",
        0x04 => "connection request failed",
        0x05 => "too many devices",
        0x06 => "already exists",
        0x07 => "busy",
        0x08 => "unknown device",
        0x09 => "resource error",
        0x0A => "request unavailable",
        0x0B => "invalid param value",
        0x0C => "wrong PIN code",
        _ => "?",
    }
}

/// Outcome of matching one incoming report against our pending request. [OBS §9.5]
#[derive(Debug, PartialEq)]
pub enum Match {
    Reply,
    Err10(u8),
    Err20(u8),
    No,
}

/// Decide whether `rsp` answers `cmd`. The report ID (byte 0) is ignored, replies must be
/// at least 5 bytes, and `extra` lists additional byte offsets that must echo the request.
pub fn classify(cmd: &[u8], rsp: &[u8], extra: &[usize]) -> Match {
    if rsp.len() < 5 || cmd.len() < 4 {
        return Match::No;
    }
    let idx_ok = rsp[1] == cmd[1];
    if rsp[2] == ERR10 {
        // A receiver may answer index 0x00 for a request sent to 0xFF.
        let idx10 = idx_ok || (cmd[1] == DIRECT && rsp[1] == 0x00);
        if idx10 && rsp[3] == cmd[2] && rsp[4] == cmd[3] {
            return Match::Err10(rsp.get(5).copied().unwrap_or(0));
        }
        return Match::No;
    }
    if rsp[2] == ERR20 && cmd[2] != ERR20 {
        if idx_ok && rsp[3] == cmd[2] && rsp[4] == cmd[3] {
            return Match::Err20(rsp.get(5).copied().unwrap_or(0));
        }
        return Match::No;
    }
    if !(idx_ok && rsp[2] == cmd[2] && rsp[3] == cmd[3]) {
        return Match::No;
    }
    for &k in extra {
        if rsp.get(k) != cmd.get(k) {
            return Match::No;
        }
    }
    Match::Reply
}

/// Build a HID++ 2.0 request: short if params fit in 3 bytes, else long. [OBS §9.3]
pub fn build20(dev: u8, feat_idx: u8, func: u8, swid: u8, params: &[u8]) -> Result<Vec<u8>, Error> {
    let len = if params.len() <= SHORT_LEN - 4 {
        SHORT_LEN
    } else if params.len() <= LONG_LEN - 4 {
        LONG_LEN
    } else {
        return Err(Error::Other("params too long".into()));
    };
    let mut m = vec![0u8; len];
    m[0] = if len == SHORT_LEN { SHORT } else { LONG };
    m[1] = dev;
    m[2] = feat_idx;
    m[3] = (func << 4) | (swid & 0x0F);
    m[4..4 + params.len()].copy_from_slice(params);
    Ok(m)
}

/// Build a HID++ 1.0 short register request `10 dev sub reg p0 p1 p2`. [OBS §9.8]
pub fn build10(dev: u8, sub: u8, reg: u8, params: [u8; 3]) -> Vec<u8> {
    vec![SHORT, dev, sub, reg, params[0], params[1], params[2]]
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// Reports that arrived during a transaction but did not answer it (events, other software's
/// traffic) are kept for `queued`, up to this many; older ones are dropped.
const QUEUE_MAX: usize = 64;

/// Where a report of one output length goes: which open device node, and its length.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Route {
    out_len: usize,
    handle: usize,
}

/// Open one handle per distinct device path and a route per collection, smallest output
/// report first so "first that fits" picks like G HUB [OBS §9.3]. On Windows every collection
/// is its own path; on Linux the short and long reports share one hidraw node, which must be
/// opened once or every input report would arrive twice.
fn plan_routes(infos: &[hid::Info]) -> (Vec<&hid::Info>, Vec<Route>) {
    let mut nodes: Vec<&hid::Info> = Vec::new();
    let mut routes = Vec::new();
    for i in infos {
        let handle = match nodes.iter().position(|n| n.path == i.path) {
            Some(h) => h,
            None => {
                nodes.push(i);
                nodes.len() - 1
            }
        };
        routes.push(Route {
            out_len: i.out_len as usize,
            handle,
        });
    }
    routes.sort_by_key(|r| r.out_len);
    (nodes, routes)
}

/// A HID++ endpoint: the short and/or long collection of one physical interface.
pub struct Hidpp {
    cols: Vec<Handle>,
    routes: Vec<Route>,
    queue: VecDeque<Vec<u8>>,
    pub swid: u8,
    pub trace: bool,
    pub timeout: Duration,
    /// Extra attempts after a timeout or Busy. G HUB uses 9 [OBS §9.6]; 2 is enough to ride
    /// out the SL2's wake-up from sleep [HW: first request after idle timed out at 1 s].
    pub retries: u8,
}

impl Hidpp {
    pub fn open(infos: &[hid::Info], swid: u8, trace: bool) -> Result<Hidpp, Error> {
        let (nodes, routes) = plan_routes(infos);
        let mut cols = Vec::new();
        for i in nodes {
            cols.push(Handle::open(i).map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => Error::Denied(open_hint(&i.path, e)),
                _ => Error::Io(open_hint(&i.path, e)),
            })?);
        }
        Ok(Hidpp {
            cols,
            routes,
            queue: VecDeque::new(),
            swid,
            trace,
            timeout: Duration::from_millis(2000),
            retries: 2,
        })
    }

    /// The open collections, for waiting on several devices at once (`hid::wait_any`).
    pub fn handles(&mut self) -> std::slice::IterMut<'_, Handle> {
        self.cols.iter_mut()
    }

    /// Pop a report queued during an earlier transaction.
    pub fn queued(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }

    /// Write `msg` to the first collection whose output report fits it, zero-padded and
    /// with byte 0 rewritten to that collection's report ID. [OBS §9.3 Win32Device::write]
    fn write(&self, msg: &[u8]) -> Result<(), Error> {
        let route = self
            .routes
            .iter()
            .find(|r| r.out_len >= msg.len() && report_id_for_len(r.out_len).is_some())
            .ok_or(Error::Other(format!(
                "message of {} bytes exceeds every output report",
                msg.len()
            )))?;
        let mut r = msg.to_vec();
        r.resize(route.out_len, 0);
        r[0] = report_id_for_len(route.out_len).unwrap_or(r[0]);
        if self.trace {
            eprintln!("-> {}", hex(&r));
        }
        self.cols[route.handle]
            .write(&r, 1000)
            .map_err(|e| Error::Io(e.to_string()))
    }

    /// Read one input report from any collection; None on timeout.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, Error> {
        let mut refs: Vec<&mut Handle> = self.cols.iter_mut().collect();
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let r = hid::read_any(&mut refs, ms).map_err(|e| Error::Io(e.to_string()))?;
        Ok(r.map(|(_, b)| b))
    }

    /// Send `msg` and wait for its reply, retrying on timeout or Busy with a growing pause.
    /// Resending is safe for everything this tool sends: reads, and writes of absolute values.
    /// G HUB's backoff is `rand()%500 + (10 << attempt)` ms [OBS §9.6]; we use a fixed
    /// 100 ms + (10 << attempt) ms [INF] to avoid needing an RNG.
    pub fn transact(&mut self, msg: &[u8], extra: &[usize]) -> Result<Vec<u8>, Error> {
        let mut attempt = 0;
        loop {
            match self.transact_once(msg, extra) {
                Err(e @ (Error::Timeout | Error::Hpp20(8))) if attempt < self.retries => {
                    attempt += 1;
                    crate::verbose(format_args!("{e}; retry {attempt}/{}", self.retries));
                    std::thread::sleep(Duration::from_millis(100 + (10 << attempt)));
                }
                r => return r,
            }
        }
    }

    fn transact_once(&mut self, msg: &[u8], extra: &[usize]) -> Result<Vec<u8>, Error> {
        self.write(msg)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let Some(r) = self.read(left)? else {
                return Err(Error::Timeout);
            };
            let m = classify(msg, &r, extra);
            if self.trace {
                eprintln!("<- {}{}", hex(&r), if m == Match::No { "  (ignored)" } else { "" });
            }
            match m {
                Match::Reply => return Ok(r),
                Match::Err10(c) => return Err(Error::Hpp10(c)),
                Match::Err20(c) => return Err(Error::Hpp20(c)),
                Match::No => {
                    if self.queue.len() == QUEUE_MAX {
                        self.queue.pop_front();
                    }
                    self.queue.push_back(r);
                }
            }
        }
    }

    /// HID++ 2.0 call; returns the reply payload (report bytes 4..).
    pub fn call(&mut self, dev: u8, feat_idx: u8, func: u8, params: &[u8], extra: &[usize]) -> Result<Vec<u8>, Error> {
        let m = build20(dev, feat_idx, func, self.swid, params)?;
        let r = self.transact(&m, extra)?;
        Ok(r[4..].to_vec())
    }

    /// HID++ 1.0 register read (short 0x81 or long 0x83); returns reply bytes 4.. .
    pub fn read_register(
        &mut self,
        dev: u8,
        sub: u8,
        reg: u8,
        params: [u8; 3],
        extra: &[usize],
    ) -> Result<Vec<u8>, Error> {
        let m = build10(dev, sub, reg, params);
        let r = self.transact(&m, extra)?;
        Ok(r[4..].to_vec())
    }

    /// Root f1 ping. Returns (major, minor) protocol version. The ping byte goes in
    /// params[2] (report byte 6) and must be echoed. [OBS §9.6] A HID++ 1.0 device answers
    /// with an 0x8F error instead. [OBS §9.6]
    pub fn ping(&mut self, dev: u8, data: u8) -> Result<(u8, u8), Error> {
        let p = self.call(dev, 0x00, 1, &[0, 0, data], &[6])?;
        Ok((p[0], p[1]))
    }

    /// Root f0 GetFeature. Returns (index, flags, version); index 0 means unsupported.
    /// [OBS §9.7]
    pub fn get_feature(&mut self, dev: u8, id: u16) -> Result<(u8, u8, u8), Error> {
        let p = self.call(dev, 0x00, 0, &id.to_be_bytes(), &[])?;
        Ok((p[0], p[1], p[2]))
    }

    /// Feature index for `id`, or Error::NoFeature.
    pub fn feature(&mut self, dev: u8, id: u16) -> Result<u8, Error> {
        match self.get_feature(dev, id)? {
            (0, _, _) => Err(Error::NoFeature(id)),
            (i, _, _) => Ok(i),
        }
    }
}

fn open_hint(path: &str, e: std::io::Error) -> String {
    #[cfg(windows)]
    let hint = match e.raw_os_error() {
        Some(5) => " (access denied: collection is held exclusively)",
        Some(32) => " (sharing violation: another program opened it without sharing)",
        _ => "",
    };
    #[cfg(target_os = "linux")]
    let hint = match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            " (permission denied: install the udev rule packaging/linux/70-peripheral-lite.rules, see the README)"
        }
        _ => "",
    };
    #[cfg(target_os = "macos")]
    let hint = match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            " (not permitted: allow this terminal or app under System Settings > Privacy & Security > Input Monitoring)"
        }
        std::io::ErrorKind::ResourceBusy => " (another program, e.g. Logi Options+, holds the device exclusively)",
        _ => "",
    };
    format!("open {path}: {e}{hint}")
}

/// Names for the feature IDs we print. [OBS agent_static_findings.md §3.2]
pub fn feature_name(id: u16) -> &'static str {
    match id {
        0x0000 => "Root",
        0x0001 => "FeatureSet",
        0x0002 => "EngTest",
        0x0003 => "FirmwareInfo",
        0x0005 => "DeviceNameType",
        0x0007 => "DeviceFriendlyName",
        0x0008 => "SwitchAndKeepAlive",
        0x0009 => "Subdevices",
        0x0011 => "PropertyAccess",
        0x0020 => "ConfigChange",
        0x0021 => "CryptoIdentifier",
        0x00C0..=0x00C3 => "DFUControl",
        0x00D0 => "DFU",
        0x1000 => "BatteryLevelStatus",
        0x1001 => "BatteryVoltage",
        0x1004 => "UnifiedBattery",
        0x1010 => "ChargingControl",
        0x1300 => "LEDControl",
        0x1500 => "ForcePairing",
        0x1800 => "GenericTest",
        0x1801 => "ManufacturingMode",
        0x1802 => "DeviceReset",
        0x1805 => "OOBState",
        0x1806 => "ConfigurableDeviceProperties",
        0x1807 => "ConfigurableProperties",
        0x1814 => "ChangeHost",
        0x1815 => "HostsInfos",
        0x1816 => "BleProPrepairing",
        0x1890 => "RfTest",
        0x18A1 => "LEDState",
        0x1B04 => "SpecialKeys(ReprogControlsV4)",
        0x1BC0 => "ReportHIDUsages",
        0x1D4B => "WirelessStatus",
        0x1E00 => "EnableHiddenFeatures",
        0x1F1F => "FirmwareProperties",
        0x1F20 => "ADCMeasurement",
        0x2121 => "HiResWheel",
        0x2201 => "AdjustableDPI",
        0x2202 => "ExtendedAdjustableDPI",
        0x2230 => "AngleSnapping",
        0x2240 => "SurfaceTuning",
        0x2250 => "ActivityTracking",
        0x2400 => "HybridTracking",
        0x8010 => "Gkey",
        0x8020 => "Mkeys",
        0x8030 => "MR",
        0x8040 => "BrightnessControl",
        0x8060 => "ReportRate",
        0x8061 => "ExtendedAdjustableReportRate",
        0x8070 => "ColorLEDEffects",
        0x8071 => "RGBEffects",
        0x8080 | 0x8081 => "PerKeyLighting",
        0x8090 => "ModeStatus",
        0x8100 => "OnboardProfiles",
        0x8110 => "MouseButtonSpy",
        0x8111 => "LatencyMonitoring",
        0x8120 => "GamingAttachments",
        0x8300 => "Sidetone",
        0x8305 => "BassTone",
        0x8310 => "Equalizer",
        0x8320 => "JackDetection",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(path: &str, out_len: u16) -> hid::Info {
        hid::Info {
            path: path.into(),
            vid: 0x46D,
            pid: 0xC54D,
            usage_page: 0xFF00,
            usage: if out_len == 7 { 1 } else { 2 },
            in_len: out_len,
            out_len,
            feat_len: 0,
            product: String::new(),
            iface: "i".into(),
        }
    }

    #[test]
    fn routes_open_each_node_once() {
        // Windows: one path per collection -> two handles, short report first
        let win = [col("long", 20), col("short", 7)];
        let (nodes, routes) = plan_routes(&win);
        assert_eq!(
            nodes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(),
            ["long", "short"]
        );
        assert_eq!(
            routes,
            [Route { out_len: 7, handle: 1 }, Route { out_len: 20, handle: 0 }]
        );
        // Linux: both collections on one hidraw node -> one handle, two routes
        let lin = [col("/dev/hidraw3", 7), col("/dev/hidraw3", 20)];
        let (nodes, routes) = plan_routes(&lin);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            routes,
            [Route { out_len: 7, handle: 0 }, Route { out_len: 20, handle: 0 }]
        );
    }

    #[test]
    fn receiver_conn_state_request_bytes() {
        // [OBS §9.8 0x140e35570] read reg 0x02
        assert_eq!(
            build10(DIRECT, GET_REGISTER, 0x02, [0, 0, 0]),
            [0x10, 0xFF, 0x81, 0x02, 0, 0, 0]
        );
    }

    #[test]
    fn pairing_info_request_bytes() {
        // [OBS §9.8 0x140e35110] slot n (0-based) -> sub-address 0x20+n
        assert_eq!(
            build10(DIRECT, GET_LONG_REGISTER, 0xB5, [0x20, 0, 0]),
            [0x10, 0xFF, 0x83, 0xB5, 0x20, 0, 0]
        );
    }

    #[test]
    fn ping_request_bytes() {
        // [OBS §9.6] 10 FF 00 (0x10|swid) 00 00 P6
        assert_eq!(
            build20(DIRECT, 0, 1, 0x03, &[0, 0, 0x5A]).unwrap(),
            [0x10, 0xFF, 0x00, 0x13, 0, 0, 0x5A]
        );
    }

    #[test]
    fn long_when_params_exceed_short() {
        let m = build20(1, 5, 6, 3, &[0, 1, 2, 3]).unwrap();
        assert_eq!(m.len(), LONG_LEN);
        assert_eq!(&m[..8], &[0x11, 1, 5, 0x63, 0, 1, 2, 3]);
        assert!(build20(1, 5, 6, 3, &[0; 17]).is_err());
    }

    #[test]
    fn matches_normal_reply_and_ping_echo() {
        let cmd = build20(1, 0, 1, 3, &[0, 0, 0x5A]).unwrap();
        let ok = [0x11, 1, 0, 0x13, 4, 2, 0x5A, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(classify(&cmd, &ok, &[6]), Match::Reply);
        let bad_echo = [0x11, 1, 0, 0x13, 4, 2, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(classify(&cmd, &bad_echo, &[6]), Match::No);
        // other software's swid
        let foreign = [0x11, 1, 0, 0x1A, 4, 2, 0x5A];
        assert_eq!(classify(&cmd, &foreign, &[6]), Match::No);
        // wrong device index
        let other_dev = [0x11, 2, 0, 0x13, 4, 2, 0x5A];
        assert_eq!(classify(&cmd, &other_dev, &[6]), Match::No);
    }

    #[test]
    fn matches_errors() {
        let cmd = build20(1, 0, 1, 3, &[0, 0, 0x5A]).unwrap();
        // HID++ 1.0 error from a receiver: 10 01 8F 00 13 <code>
        assert_eq!(
            classify(&cmd, &[0x10, 1, 0x8F, 0x00, 0x13, 0x09, 0], &[6]),
            Match::Err10(9)
        );
        // HID++ 2.0 error: 11 01 FF 00 13 <code>
        let mut e = [0u8; 20];
        e[..6].copy_from_slice(&[0x11, 1, 0xFF, 0x00, 0x13, 0x07]);
        assert_eq!(classify(&cmd, &e, &[6]), Match::Err20(7));
        // index 0x00 answer to a 0xFF HID++ 1.0 request
        let reg = build10(DIRECT, GET_REGISTER, 0x02, [0, 0, 0]);
        assert_eq!(
            classify(&reg, &[0x10, 0x00, 0x8F, 0x81, 0x02, 0x02, 0], &[]),
            Match::Err10(2)
        );
    }

    #[test]
    fn too_short_never_matches() {
        let cmd = build10(DIRECT, GET_REGISTER, 0x02, [0, 0, 0]);
        assert_eq!(classify(&cmd, &[0x10, 0xFF, 0x81, 0x02], &[]), Match::No);
    }

    #[test]
    fn event_with_swid_zero_is_not_a_reply() {
        let cmd = build20(1, 4, 1, 3, &[]).unwrap();
        assert_eq!(classify(&cmd, &[0x11, 1, 4, 0x00, 50, 0, 0], &[]), Match::No);
    }
}
