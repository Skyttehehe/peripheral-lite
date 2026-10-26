//! macOS HID backend: IOHIDManager to enumerate, IOHIDDevice to talk to a device, the raw
//! report descriptor for the collection layout (the same parser as Linux).
//!
//! IOKit delivers input reports as callbacks on a CFRunLoop, while the rest of peripheral-lite
//! waits on several devices at once. So every open device gets a small thread running its own
//! run loop; the callback queues the report and writes a byte to a pipe, and `wait_any` is the
//! same `poll(2)` loop as on Linux, over those pipes. Hot-plug works the same way: an
//! IOHIDManager on its own run loop thread signals a pipe.
//!
//! Access: HID++ lives on a vendor collection (page 0xFF00 or 0xFF43), which macOS lets a
//! normal user open without any permission prompt [HW MX Master 4 over Bluetooth LE].
//! Device paths are `DevSrvsID:<IORegistry entry ID>`, like hidapi's.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::{FOREVER, Info, Signal, Wake, descriptor};

/// What `wait_any` waits on: a pollable file descriptor.
pub type RawWaitable = RawFd;

// ---- CoreFoundation / IOKit FFI: just the calls used below -------------------------------

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;
type CFRunLoopRef = *mut c_void;
type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IOReturn = i32;
type IoObject = u32;

type ReportCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, u32, u32, *mut u8, CFIndex);
type RemovalCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void);
type DeviceCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, IOHIDDeviceRef);

const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
const SINT32: CFIndex = 3; // kCFNumberSInt32Type
const OUTPUT_REPORT: u32 = 1; // kIOHIDReportTypeOutput
const NOT_PERMITTED: IOReturn = 0xE000_02E2_u32 as i32; // kIOReturnNotPermitted
const EXCLUSIVE: IOReturn = 0xE000_02C5_u32 as i32; // kIOReturnExclusiveAccess

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFNumberCreate(alloc: CFTypeRef, kind: CFIndex, value: *const c_void) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, enc: u32) -> u8;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    fn CFDictionaryCreateMutable(
        alloc: CFTypeRef,
        capacity: CFIndex,
        keys: *const c_void,
        values: *const c_void,
    ) -> *mut c_void;
    fn CFDictionarySetValue(d: *mut c_void, key: CFTypeRef, value: CFTypeRef);
    fn CFSetGetCount(s: CFTypeRef) -> CFIndex;
    fn CFSetGetValues(s: CFTypeRef, values: *mut CFTypeRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopStop(rl: CFRunLoopRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(alloc: CFTypeRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(m: IOHIDManagerRef, matching: CFTypeRef);
    fn IOHIDManagerCopyDevices(m: IOHIDManagerRef) -> CFTypeRef;
    fn IOHIDManagerRegisterDeviceMatchingCallback(m: IOHIDManagerRef, cb: Option<DeviceCallback>, ctx: *mut c_void);
    fn IOHIDManagerRegisterDeviceRemovalCallback(m: IOHIDManagerRef, cb: Option<DeviceCallback>, ctx: *mut c_void);
    fn IOHIDManagerScheduleWithRunLoop(m: IOHIDManagerRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDManagerUnscheduleFromRunLoop(m: IOHIDManagerRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDDeviceGetProperty(d: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    fn IOHIDDeviceGetService(d: IOHIDDeviceRef) -> IoObject;
    fn IOHIDDeviceCreate(alloc: CFTypeRef, service: IoObject) -> IOHIDDeviceRef;
    fn IOHIDDeviceOpen(d: IOHIDDeviceRef, options: u32) -> IOReturn;
    fn IOHIDDeviceClose(d: IOHIDDeviceRef, options: u32) -> IOReturn;
    fn IOHIDDeviceRegisterInputReportCallback(
        d: IOHIDDeviceRef,
        report: *mut u8,
        len: CFIndex,
        cb: Option<ReportCallback>,
        ctx: *mut c_void,
    );
    fn IOHIDDeviceRegisterRemovalCallback(d: IOHIDDeviceRef, cb: Option<RemovalCallback>, ctx: *mut c_void);
    fn IOHIDDeviceScheduleWithRunLoop(d: IOHIDDeviceRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDDeviceUnscheduleFromRunLoop(d: IOHIDDeviceRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDDeviceSetReport(d: IOHIDDeviceRef, kind: u32, id: CFIndex, report: *const u8, len: CFIndex) -> IOReturn;
    fn IORegistryEntryGetRegistryEntryID(entry: IoObject, id: *mut u64) -> i32;
    fn IORegistryEntryIDMatching(id: u64) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoObject;
    fn IOObjectRelease(o: IoObject) -> i32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

/// kIOHIDRequestTypeListenEvent: Input Monitoring.
const LISTEN_EVENT: u32 = 1;

/// Ask macOS for Input Monitoring: it lists the app (or the terminal) under System Settings >
/// Privacy & Security > Input Monitoring and shows its prompt while nothing is decided (first
/// use, or after the entry was removed); a decided request asks nothing. Opening succeeds once
/// the user allows it, without a restart (the watcher tries again every 30 s).
fn request_input_monitoring() {
    // SAFETY: a plain call without pointers; it returns at once, the prompt is macOS's own.
    unsafe {
        IOHIDRequestAccess(LISTEN_EVENT);
    }
}

/// A CF object we own (from a Create/Copy call), released on drop.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference.
            unsafe { CFRelease(self.0) }
        }
    }
}

fn cfstr(s: &CStr) -> Cf {
    // SAFETY: valid C string; the result is owned by the returned Cf.
    Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) })
}

/// `{VendorID: vid}`, the matching dictionary for one vendor's devices.
fn vendor_matching(vid: u16) -> Cf {
    let v = vid as i32;
    // SAFETY: standard CF calls; the dictionary retains key and value.
    unsafe {
        let d = CFDictionaryCreateMutable(
            std::ptr::null(),
            0,
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        );
        let key = cfstr(c"VendorID");
        let num = Cf(CFNumberCreate(std::ptr::null(), SINT32, (&v as *const i32).cast()));
        CFDictionarySetValue(d, key.0, num.0);
        Cf(d)
    }
}

/// A device property (borrowed, not owned), if it has the expected CF type.
fn property(d: IOHIDDeviceRef, key: &CStr, type_id: usize) -> Option<CFTypeRef> {
    let k = cfstr(key);
    // SAFETY: d is a valid device; the property is borrowed for the duration of the caller.
    let v = unsafe { IOHIDDeviceGetProperty(d, k.0) };
    // SAFETY: v is non-null and a CF object.
    (!v.is_null() && unsafe { CFGetTypeID(v) } == type_id).then_some(v)
}

fn int_property(d: IOHIDDeviceRef, key: &CStr) -> Option<i32> {
    // SAFETY: plain getter.
    let n = property(d, key, unsafe { CFNumberGetTypeID() })?;
    let mut v = 0i32;
    // SAFETY: n is a CFNumber; v is a valid out-pointer for SInt32.
    (unsafe { CFNumberGetValue(n, SINT32, (&mut v as *mut i32).cast()) } != 0).then_some(v)
}

fn string_property(d: IOHIDDeviceRef, key: &CStr) -> Option<String> {
    // SAFETY: plain getter.
    let s = property(d, key, unsafe { CFStringGetTypeID() })?;
    let mut buf = [0 as c_char; 256];
    // SAFETY: s is a CFString, buf is valid for its length.
    if unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) } == 0 {
        return None;
    }
    // SAFETY: CFStringGetCString wrote a NUL-terminated string.
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

fn data_property(d: IOHIDDeviceRef, key: &CStr) -> Option<Vec<u8>> {
    // SAFETY: plain getter.
    let data = property(d, key, unsafe { CFDataGetTypeID() })?;
    // SAFETY: data is a CFData; pointer and length describe its bytes.
    Some(unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize) }.to_vec())
}

fn entry_id(d: IOHIDDeviceRef) -> Option<u64> {
    let mut id = 0u64;
    // SAFETY: d is valid; the service it returns is not owned by us.
    (unsafe { IORegistryEntryGetRegistryEntryID(IOHIDDeviceGetService(d), &mut id) } == 0).then_some(id)
}

const PATH_PREFIX: &str = "DevSrvsID:";

/// Present HID interfaces of vendor `vid`, one `Info` per top-level collection.
pub fn enumerate(vid: u16) -> io::Result<Vec<Info>> {
    let mut out = Vec::new();
    // SAFETY: the manager and the device set are owned by Cf guards; the devices are borrowed
    // from the set while it lives.
    unsafe {
        let m = Cf(IOHIDManagerCreate(std::ptr::null(), 0));
        if m.0.is_null() {
            return Err(io::Error::other("IOHIDManagerCreate failed"));
        }
        let matching = vendor_matching(vid);
        IOHIDManagerSetDeviceMatching(m.0 as IOHIDManagerRef, matching.0);
        let set = Cf(IOHIDManagerCopyDevices(m.0 as IOHIDManagerRef));
        if set.0.is_null() {
            return Ok(out); // no device of this vendor
        }
        let mut devs: Vec<CFTypeRef> = vec![std::ptr::null(); CFSetGetCount(set.0) as usize];
        CFSetGetValues(set.0, devs.as_mut_ptr());
        let mut found = Vec::new();
        for d in devs {
            let d = d as IOHIDDeviceRef;
            let (Some(id), Some(pid)) = (entry_id(d), int_property(d, c"ProductID")) else {
                continue;
            };
            let Some(desc) = data_property(d, c"ReportDescriptor") else {
                continue;
            };
            let product = string_property(d, c"Product").unwrap_or_default();
            found.push((id, pid as u16, product, desc));
        }
        // CFSet order is arbitrary; keep the listing stable between runs.
        found.sort_by_key(|f| f.0);
        for (id, pid, product, desc) in found {
            let path = format!("{PATH_PREFIX}{id}");
            for c in descriptor::parse(&desc) {
                out.push(Info {
                    path: path.clone(),
                    vid,
                    pid,
                    usage_page: c.usage_page,
                    usage: c.usage,
                    in_len: c.in_len,
                    out_len: c.out_len,
                    feat_len: c.feat_len,
                    product: product.clone(),
                    iface: path.clone(),
                });
            }
        }
    }
    Ok(out)
}

// ---- pipes ---------------------------------------------------------------------------------

/// A non-blocking, close-on-exec pipe: (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: valid out-array; the fds are owned below.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let (r, w) = (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]));
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok((r, w))
    }
}

/// Make `fd` readable. A full pipe already is, so a failed write loses nothing.
fn signal(fd: &OwnedFd) {
    // SAFETY: writes one byte from a valid buffer.
    unsafe { libc::write(fd.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
}

/// Empty the pipe: it is readable again only after the next `signal`.
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 256];
    // SAFETY: valid buffer; the fd is non-blocking.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

fn poll_one(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut p, 1, ms) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r > 0)
}

/// Runs `body` on a new thread that owns a CFRunLoop, and returns that run loop once `body`
/// has scheduled its sources. The thread runs until `stop` is set and the loop is stopped.
fn run_loop_thread(
    name: &str,
    stop: Arc<AtomicBool>,
    body: impl FnOnce(CFRunLoopRef) -> Box<dyn FnOnce()> + Send + 'static,
) -> io::Result<(SendPtr, std::thread::JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel();
    let t = std::thread::Builder::new().name(name.into()).spawn(move || {
        // SAFETY: the current thread's run loop, valid for the thread's lifetime.
        let rl = unsafe { CFRunLoopGetCurrent() };
        let cleanup = body(rl);
        let _ = tx.send(SendPtr(rl));
        while !stop.load(Ordering::Acquire) {
            // SAFETY: runs this thread's own loop; returns when stopped or after the interval.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 60.0, 0) };
        }
        cleanup();
    })?;
    let rl = rx
        .recv()
        .map_err(|_| io::Error::other("HID run loop thread failed to start"))?;
    Ok((rl, t))
}

/// A CF pointer handed between threads. CFRunLoopStop and the IOHIDDevice calls used here
/// are documented as thread-safe.
#[derive(Clone, Copy)]
struct SendPtr(*mut c_void);
// SAFETY: see above.
unsafe impl Send for SendPtr {}

fn stop_thread(stop: &AtomicBool, rl: SendPtr, t: Option<std::thread::JoinHandle<()>>) {
    stop.store(true, Ordering::Release);
    // SAFETY: rl is the worker's run loop, alive until the thread exits (joined below).
    unsafe { CFRunLoopStop(rl.0) };
    if let Some(t) = t {
        let _ = t.join();
    }
}

// ---- devices -------------------------------------------------------------------------------

/// State shared between a `Handle` and its run loop thread's callbacks.
struct Shared {
    /// Report IDs worth queueing; empty = all. See `vendor_report_ids`.
    ids: Vec<u8>,
    queue: Mutex<VecDeque<Vec<u8>>>,
    gone: AtomicBool,
    wake: OwnedFd,
}

/// Report IDs of the vendor-defined collections (page 0xFF00 and up) in `desc`.
///
/// Unlike Windows, where each collection is a device of its own, an IOHIDDevice delivers the
/// reports of all its collections. A Bluetooth mouse sends its HID++ replies and its pointer
/// movement through the same device [HW MX Master 4: report 0x02 with every move], so without
/// this filter a watcher would wake up for every mouse movement.
fn vendor_report_ids(desc: &[u8]) -> Vec<u8> {
    let mut ids: Vec<u8> = descriptor::parse(desc)
        .into_iter()
        .filter(|c| c.usage_page >= 0xFF00)
        .flat_map(|c| c.report_ids)
        .filter(|&id| id != 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Longest input report we accept: the HID++ very long report.
const IN_MAX: usize = 64;

extern "C" fn on_report(
    ctx: *mut c_void,
    _r: IOReturn,
    _s: *mut c_void,
    _t: u32,
    id: u32,
    data: *mut u8,
    len: CFIndex,
) {
    // SAFETY: ctx is the Arc<Shared> pointer registered in Handle::open, kept alive by the
    // run loop thread; data/len describe the report buffer.
    let (sh, bytes) = unsafe { (&*(ctx as *const Shared), std::slice::from_raw_parts(data, len as usize)) };
    // IOKit passes numbered reports with the ID as byte 0; keep that layout either way.
    if !sh.ids.is_empty() && !sh.ids.contains(&(id as u8)) {
        return; // e.g. pointer movement on a Bluetooth mouse
    }
    let report = if id != 0 && bytes.first() != Some(&(id as u8)) {
        [&[id as u8], bytes].concat()
    } else {
        bytes.to_vec()
    };
    if let Ok(mut q) = sh.queue.lock() {
        q.push_back(report);
        signal(&sh.wake);
    }
}

extern "C" fn on_removed(ctx: *mut c_void, _r: IOReturn, _s: *mut c_void) {
    // SAFETY: as in on_report.
    let sh = unsafe { &*(ctx as *const Shared) };
    sh.gone.store(true, Ordering::Release);
    signal(&sh.wake);
}

/// An opened IOHIDDevice (all report IDs of one interface).
pub struct Handle {
    dev: SendPtr,
    shared: Arc<Shared>,
    ready: OwnedFd,
    stop: Arc<AtomicBool>,
    rl: SendPtr,
    thread: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: the device is only used through thread-safe IOHIDDevice calls (SetReport, Close);
// callbacks run on the handle's own thread.
unsafe impl Send for Handle {}

fn open_error(code: IOReturn) -> io::Error {
    match code {
        NOT_PERMITTED => io::Error::new(io::ErrorKind::PermissionDenied, "not permitted"),
        EXCLUSIVE => io::Error::new(io::ErrorKind::ResourceBusy, "held exclusively by another program"),
        c => io::Error::other(format!("IOReturn {:#010x}", c as u32)),
    }
}

impl Handle {
    pub fn open(info: &Info) -> io::Result<Handle> {
        let id: u64 = info
            .path
            .strip_prefix(PATH_PREFIX)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a macOS HID path"))?;
        // SAFETY: IOServiceGetMatchingService consumes the matching dictionary; the service
        // object is released after the device holds its own reference.
        let dev = unsafe {
            let service = IOServiceGetMatchingService(0, IORegistryEntryIDMatching(id));
            if service == 0 {
                return Err(io::Error::new(io::ErrorKind::NotFound, "device is gone"));
            }
            let dev = IOHIDDeviceCreate(std::ptr::null(), service);
            IOObjectRelease(service);
            if dev.is_null() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "device is gone"));
            }
            let r = IOHIDDeviceOpen(dev, 0);
            if r != 0 {
                CFRelease(dev);
                if r == NOT_PERMITTED {
                    request_input_monitoring();
                }
                return Err(open_error(r));
            }
            SendPtr(dev)
        };
        let ids = data_property(dev.0, c"ReportDescriptor")
            .map(|d| vendor_report_ids(&d))
            .unwrap_or_default();
        let (ready, wake) = pipe()?;
        let shared = Arc::new(Shared {
            ids,
            queue: Mutex::new(VecDeque::new()),
            gone: AtomicBool::new(false),
            wake,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SendPtr(Arc::as_ptr(&shared) as *mut c_void);
        let keep = shared.clone();
        let spawned = run_loop_thread("peripheral-lite hid", stop.clone(), move |rl| {
            let (d, ctx) = (dev, ctx); // whole SendPtrs, not their raw-pointer fields
            let mut buf = vec![0u8; IN_MAX].into_boxed_slice();
            // SAFETY: buf and `keep` (behind ctx) outlive the registration: both are moved
            // into the cleanup closure, which unregisters before dropping them.
            unsafe {
                IOHIDDeviceRegisterInputReportCallback(
                    d.0,
                    buf.as_mut_ptr(),
                    IN_MAX as CFIndex,
                    Some(on_report),
                    ctx.0,
                );
                IOHIDDeviceRegisterRemovalCallback(d.0, Some(on_removed), ctx.0);
                IOHIDDeviceScheduleWithRunLoop(d.0, rl, kCFRunLoopDefaultMode);
            }
            let rl = SendPtr(rl);
            Box::new(move || {
                // SAFETY: same device and run loop as above, on the same thread.
                unsafe {
                    IOHIDDeviceUnscheduleFromRunLoop(d.0, rl.0, kCFRunLoopDefaultMode);
                    IOHIDDeviceRegisterInputReportCallback(
                        d.0,
                        buf.as_mut_ptr(),
                        IN_MAX as CFIndex,
                        None,
                        std::ptr::null_mut(),
                    );
                    IOHIDDeviceRegisterRemovalCallback(d.0, None, std::ptr::null_mut());
                }
                drop((buf, keep));
            })
        });
        match spawned {
            Ok((rl, t)) => Ok(Handle {
                dev,
                shared,
                ready,
                stop,
                rl,
                thread: Some(t),
            }),
            Err(e) => {
                // SAFETY: opened and created above, not used elsewhere.
                unsafe {
                    IOHIDDeviceClose(dev.0, 0);
                    CFRelease(dev.0);
                }
                Err(e)
            }
        }
    }

    /// Write one output report (report ID in byte 0). IOHIDDeviceSetReport is synchronous;
    /// IOKit applies its own timeout, so `timeout_ms` is not used here.
    pub fn write(&self, report: &[u8], _timeout_ms: u32) -> io::Result<()> {
        let Some(&id) = report.first() else {
            return Err(io::Error::other("empty HID report"));
        };
        if self.shared.gone.load(Ordering::Acquire) {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        // With report IDs IOKit wants the whole report, ID byte included (as hidapi sends it).
        // SAFETY: dev is open; report is valid for its length.
        let r = unsafe {
            IOHIDDeviceSetReport(
                self.dev.0,
                OUTPUT_REPORT,
                id as CFIndex,
                report.as_ptr(),
                report.len() as CFIndex,
            )
        };
        if r != 0 {
            return Err(io::Error::other(format!(
                "HID write failed: IOReturn {:#010x}",
                r as u32
            )));
        }
        Ok(())
    }

    /// One report if one is waiting, else None; an error once the device is gone.
    fn read_now(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut q = self
            .shared
            .queue
            .lock()
            .map_err(|_| io::Error::other("HID queue poisoned"))?;
        let r = q.pop_front();
        if q.is_empty() {
            drain(&self.ready); // callbacks signal under the same lock, so nothing is lost
        }
        match r {
            Some(r) => Ok(Some(r)),
            None if self.shared.gone.load(Ordering::Acquire) => Err(io::Error::from_raw_os_error(libc::ENODEV)),
            None => Ok(None),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        stop_thread(&self.stop, self.rl, self.thread.take());
        // SAFETY: the run loop thread has unregistered and exited; we own the device.
        unsafe {
            IOHIDDeviceClose(self.dev.0, 0);
            CFRelease(self.dev.0);
        }
    }
}

/// Wait up to `timeout_ms` (`FOREVER` allowed) for an input report on any of `hs`, or for one
/// of `extras` to be signalled. One `poll(2)` call over the handles' pipes; no busy waiting.
pub fn wait_any(hs: &mut [&mut Handle], extras: &[&dyn Signal], timeout_ms: u32) -> Wake {
    let deadline = (timeout_ms != FOREVER).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
    loop {
        // A report may already be queued (the pipe is drained only when the queue empties).
        for (i, h) in hs.iter_mut().enumerate() {
            match h.read_now() {
                Ok(Some(rep)) => return Wake::Report(i, rep),
                Ok(None) => {}
                Err(e) => return Wake::Failed(i, e),
            }
        }
        let mut fds: Vec<libc::pollfd> = hs
            .iter()
            .map(|h| h.ready.as_raw_fd())
            .chain(extras.iter().map(|s| s.raw()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ms = match deadline {
            None => -1,
            Some(d) => d
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32,
        };
        // SAFETY: fds is a valid array of pollfd for the call.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Wake::Failed(0, e);
        }
        if r == 0 {
            return Wake::Timeout;
        }
        for (k, s) in extras.iter().enumerate() {
            if fds[hs.len() + k].revents & libc::POLLIN != 0 {
                s.consume(); // like an auto-reset event: one wake-up consumes the signal
                return Wake::Extra(k);
            }
        }
        // A handle's pipe is readable: the loop's first step returns its report.
    }
}

/// Wakes a thread blocked in `wait_any` from another thread: a pipe.
pub struct Waker {
    r: OwnedFd,
    w: OwnedFd,
}

impl Waker {
    pub fn new() -> io::Result<Waker> {
        let (r, w) = pipe()?;
        Ok(Waker { r, w })
    }

    pub fn wake(&self) {
        signal(&self.w);
    }

    pub(super) fn raw_waitable(&self) -> RawWaitable {
        self.r.as_raw_fd()
    }

    pub(super) fn consume(&self) {
        drain(&self.r);
    }
}

/// Device arrival/removal: an IOHIDManager for Logitech devices on its own run loop thread.
/// Its callbacks also fire once for every device present when it starts; `hid()` absorbs
/// that first batch.
pub struct Notifier {
    r: OwnedFd,
    stop: Arc<AtomicBool>,
    rl: SendPtr,
    thread: Option<std::thread::JoinHandle<()>>,
}

extern "C" fn on_device(ctx: *mut c_void, _r: IOReturn, _s: *mut c_void, _d: IOHIDDeviceRef) {
    // SAFETY: ctx is the write end owned by the run loop thread's cleanup closure.
    signal(unsafe { &*(ctx as *const OwnedFd) });
}

impl Notifier {
    pub fn hid() -> io::Result<Notifier> {
        let (r, w) = pipe()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (rl, t) = run_loop_thread("peripheral-lite hotplug", stop.clone(), move |rl| {
            let w = Box::new(w);
            let ctx = &*w as *const OwnedFd as *mut c_void;
            // One manager per vendor peripheral-lite talks to (Logitech, Corsair), sharing the callback.
            // SAFETY: the managers are owned by the cleanup closure; `w` outlives the callbacks.
            let ms: Vec<SendPtr> = [crate::LOGITECH_VID, crate::CORSAIR_VID]
                .into_iter()
                .map(|vid| unsafe {
                    let m = IOHIDManagerCreate(std::ptr::null(), 0);
                    let matching = vendor_matching(vid);
                    IOHIDManagerSetDeviceMatching(m, matching.0);
                    IOHIDManagerRegisterDeviceMatchingCallback(m, Some(on_device), ctx);
                    IOHIDManagerRegisterDeviceRemovalCallback(m, Some(on_device), ctx);
                    IOHIDManagerScheduleWithRunLoop(m, rl, kCFRunLoopDefaultMode);
                    SendPtr(m)
                })
                .collect();
            // deliver the initial "matched" calls for present devices now
            // SAFETY: on this thread's run loop.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 0) };
            let rl = SendPtr(rl);
            Box::new(move || {
                for m in ms {
                    // SAFETY: same managers and run loop, on the same thread.
                    unsafe {
                        IOHIDManagerUnscheduleFromRunLoop(m.0, rl.0, kCFRunLoopDefaultMode);
                        CFRelease(m.0);
                    }
                }
                drop(w);
            })
        })?;
        drain(&r); // the start-up batch is not a change
        Ok(Notifier {
            r,
            stop,
            rl,
            thread: Some(t),
        })
    }

    /// Wait up to `ms` for further notifications, consuming them (lets a burst from one
    /// plug-in settle before re-enumerating).
    pub fn settle(&self, ms: u32) {
        drain(&self.r);
        while poll_one(self.r.as_raw_fd(), Duration::from_millis(ms as u64)).unwrap_or(false) {
            drain(&self.r);
        }
    }

    pub(super) fn raw_waitable(&self) -> RawWaitable {
        self.r.as_raw_fd()
    }

    pub(super) fn consume(&self) {
        drain(&self.r);
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        stop_thread(&self.stop, self.rl, self.thread.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_ids_from_an_mx_master_4_descriptor() {
        // [HW] MX Master 4 over Bluetooth LE: mouse (ID 2) plus HID++ long on 0xFF43 (ID 0x11)
        let desc = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xa1, 0x00, 0x95, 0x07, 0x75, 0x01, 0x15, 0x00,
            0x25, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x07, 0x81, 0x02, 0x95, 0x09, 0x81, 0x03, 0x05, 0x01, 0xa4, 0x95,
            0x02, 0x75, 0x0c, 0x16, 0x00, 0xf8, 0x26, 0xff, 0x07, 0x36, 0x00, 0xf8, 0x46, 0xff, 0x07, 0x55, 0x0d, 0x65,
            0x13, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06, 0xb4, 0x95, 0x01, 0x75, 0x08, 0x15, 0x81, 0x25, 0x7f, 0x09, 0x38,
            0x81, 0x06, 0x05, 0x0c, 0x0a, 0x38, 0x02, 0x81, 0x06, 0xc0, 0xc0, 0x06, 0x43, 0xff, 0x0a, 0x02, 0x02, 0xa1,
            0x01, 0x85, 0x11, 0x95, 0x13, 0x75, 0x08, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09, 0x02, 0x81, 0x00, 0x09, 0x02,
            0x91, 0x00, 0xc0,
        ];
        assert_eq!(vendor_report_ids(&desc), [0x11]);
    }

    #[test]
    fn enumerate_and_notifier_start_without_hardware() {
        // No device needed: enumeration returns what is present (possibly nothing), and the
        // hot-plug thread starts and stops cleanly.
        enumerate(crate::LOGITECH_VID).unwrap();
        let n = Notifier::hid().unwrap();
        assert!(matches!(wait_any(&mut [], &[&n], 10), Wake::Timeout));
        drop(n);
    }
}
