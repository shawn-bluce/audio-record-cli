//! Power management while recording (`Power`):
//!   * `IdleSleepGuard` keeps the Mac awake (idle sleep) for the whole recording,
//!   * `SleepWatch` notices an imminent system sleep (lid closed, Apple menu ›
//!     Sleep, low battery…) and holds it off until the recording is saved,
//!   * `Lid` reports the clamshell state, so closing the lid also stops the
//!     recording when the Mac would stay awake (external display + power).

use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFAllocatorRef, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::CFMutableDictionaryRef;
use core_foundation_sys::string::CFStringRef;

use crate::ui;

type IoObject = u32;
type IoConnect = u32;
type KernReturn = i32;
type IoNotificationPortRef = *mut c_void;
type DispatchQueue = *mut c_void;
type InterestCallback =
    unsafe extern "C" fn(refcon: *mut c_void, service: IoObject, msg: u32, arg: *mut c_void);

const MAIN_PORT_DEFAULT: u32 = 0;
const ASSERTION_LEVEL_ON: u32 = 255;
const MSG_CAN_SYSTEM_SLEEP: u32 = 0xe000_0270;
const MSG_SYSTEM_WILL_SLEEP: u32 = 0xe000_0280;
/// macOS forces sleep after 30 s without an answer; answer well before that.
const MAX_SLEEP_DELAY: Duration = Duration::from_secs(20);
const LID_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut u32,
    ) -> KernReturn;
    fn IOPMAssertionRelease(id: u32) -> KernReturn;
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut IoNotificationPortRef,
        callback: InterestCallback,
        notifier: *mut IoObject,
    ) -> IoConnect;
    fn IODeregisterForSystemPower(notifier: *mut IoObject) -> KernReturn;
    fn IOAllowPowerChange(kernel_port: IoConnect, notification_id: isize) -> KernReturn;
    fn IONotificationPortSetDispatchQueue(port: IoNotificationPortRef, queue: DispatchQueue);
    fn IONotificationPortDestroy(port: IoNotificationPortRef);
    fn IOServiceClose(connect: IoConnect) -> KernReturn;
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFMutableDictionaryRef) -> IoObject;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOObjectRelease(obj: IoObject) -> KernReturn;
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(queue: DispatchQueue, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(obj: DispatchQueue);
}

/// Power handling for one recording: keep the Mac awake, stop before it sleeps.
/// Every part is best effort; failures are reported as warnings.
pub struct Power {
    idle: Option<IdleSleepGuard>,
    sleep: Option<SleepWatch>,
    lid: Option<Lid>,
    last_lid_poll: Instant,
}

impl Power {
    pub fn new() -> Self {
        let idle = IdleSleepGuard::new("arec is recording audio")
            .map_err(|e| ui::warning(format_args!("cannot prevent idle sleep ({e:#})")))
            .ok();
        let sleep = SleepWatch::new()
            .map_err(|e| {
                ui::warning(format_args!(
                    "system sleep notifications unavailable ({e:#})"
                ))
            })
            .ok();
        Self {
            idle,
            sleep,
            lid: Lid::new(),
            last_lid_poll: Instant::now(),
        }
    }

    /// One line for the startup summary.
    pub fn summary(&self) -> String {
        format!(
            "{}; {}",
            if self.idle.is_some() {
                "idle sleep prevented"
            } else {
                "idle sleep NOT prevented"
            },
            if self.lid.is_some() {
                "closing the lid stops and saves"
            } else {
                "system sleep stops and saves"
            }
        )
    }

    /// Why the recording has to stop now, if it does.
    pub fn stop_reason(&mut self) -> Option<&'static str> {
        if self.sleep.as_ref().is_some_and(SleepWatch::requested) {
            return Some("The Mac is going to sleep — stopping");
        }
        if self.last_lid_poll.elapsed() >= LID_POLL_INTERVAL {
            self.last_lid_poll = Instant::now();
            if self.lid.as_mut().is_some_and(Lid::just_closed) {
                return Some("Lid closed — stopping");
            }
        }
        None
    }

    /// The recording is safe on disk: let a pending system sleep go ahead and
    /// allow idle sleep again.
    pub fn release(&mut self) {
        if let Some(s) = &self.sleep {
            s.release();
        }
        self.idle = None;
    }
}

/// Prevents idle system sleep while alive (the display may still turn off).
struct IdleSleepGuard(u32);

impl IdleSleepGuard {
    fn new(reason: &str) -> Result<Self> {
        let kind = CFString::from_static_string("PreventUserIdleSystemSleep");
        let name = CFString::new(reason);
        let mut id = 0;
        let st = unsafe {
            IOPMAssertionCreateWithName(
                kind.as_concrete_TypeRef(),
                ASSERTION_LEVEL_ON,
                name.as_concrete_TypeRef(),
                &mut id,
            )
        };
        if st != 0 {
            bail!("IOPMAssertionCreateWithName failed: 0x{st:x}");
        }
        Ok(Self(id))
    }
}

impl Drop for IdleSleepGuard {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

struct SleepState {
    root_port: AtomicU32,
    requested: AtomicBool,
    /// Set once the recording is safely on disk; the pending sleep may proceed.
    released: Mutex<bool>,
    cv: Condvar,
}

unsafe extern "C" fn power_callback(refcon: *mut c_void, _: IoObject, msg: u32, arg: *mut c_void) {
    let state = unsafe { &*(refcon as *const SleepState) };
    let port = state.root_port.load(Ordering::SeqCst);
    match msg {
        // Idle sleep is already blocked by the assertion; never veto anything else.
        MSG_CAN_SYSTEM_SLEEP => unsafe {
            IOAllowPowerChange(port, arg as isize);
        },
        MSG_SYSTEM_WILL_SLEEP => {
            state.requested.store(true, Ordering::SeqCst);
            let released = state.released.lock().unwrap_or_else(|e| e.into_inner());
            let _ = state
                .cv
                .wait_timeout_while(released, MAX_SLEEP_DELAY, |r| !*r);
            unsafe { IOAllowPowerChange(port, arg as isize) };
        }
        _ => {}
    }
}

extern "C" fn noop(_: *mut c_void) {}

/// Receives system sleep notifications on a private dispatch queue.
struct SleepWatch {
    state: *mut SleepState,
    port: IoNotificationPortRef,
    notifier: IoObject,
    queue: DispatchQueue,
}

impl SleepWatch {
    fn new() -> Result<Self> {
        let state = Box::into_raw(Box::new(SleepState {
            root_port: AtomicU32::new(0),
            requested: AtomicBool::new(false),
            released: Mutex::new(false),
            cv: Condvar::new(),
        }));
        let mut port: IoNotificationPortRef = ptr::null_mut();
        let mut notifier: IoObject = 0;
        let root = unsafe {
            IORegisterForSystemPower(
                state as *mut c_void,
                &mut port,
                power_callback,
                &mut notifier,
            )
        };
        if root == 0 {
            unsafe { drop(Box::from_raw(state)) };
            bail!("IORegisterForSystemPower failed");
        }
        unsafe { &*state }.root_port.store(root, Ordering::SeqCst);
        let queue = unsafe { dispatch_queue_create(c"dev.arec.power".as_ptr(), ptr::null()) };
        unsafe { IONotificationPortSetDispatchQueue(port, queue) };
        Ok(Self {
            state,
            port,
            notifier,
            queue,
        })
    }

    fn state(&self) -> &SleepState {
        unsafe { &*self.state }
    }

    /// The system wants to sleep and is waiting for us.
    fn requested(&self) -> bool {
        self.state().requested.load(Ordering::SeqCst)
    }

    /// Let a pending (or future) sleep proceed.
    fn release(&self) {
        let mut r = self
            .state()
            .released
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *r = true;
        self.state().cv.notify_all();
    }
}

impl Drop for SleepWatch {
    fn drop(&mut self) {
        self.release();
        unsafe {
            // Wait for an in-flight callback to finish before tearing down.
            dispatch_sync_f(self.queue, ptr::null_mut(), noop);
            IODeregisterForSystemPower(&mut self.notifier);
            IOServiceClose(self.state().root_port.load(Ordering::SeqCst));
            IONotificationPortDestroy(self.port);
            dispatch_sync_f(self.queue, ptr::null_mut(), noop);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.state));
        }
    }
}

/// Clamshell (lid) state of a MacBook.
struct Lid {
    root_domain: IoObject,
    was_closed: bool,
}

impl Lid {
    /// `None` on Macs without a lid.
    fn new() -> Option<Self> {
        let svc = unsafe {
            IOServiceGetMatchingService(
                MAIN_PORT_DEFAULT,
                IOServiceMatching(c"IOPMrootDomain".as_ptr()),
            )
        };
        if svc == 0 {
            return None;
        }
        let mut lid = Self {
            root_domain: svc,
            was_closed: false,
        };
        lid.was_closed = lid.closed()?;
        Some(lid)
    }

    fn closed(&self) -> Option<bool> {
        let key = CFString::from_static_string("AppleClamshellState");
        let v = unsafe {
            IORegistryEntryCreateCFProperty(
                self.root_domain,
                key.as_concrete_TypeRef(),
                kCFAllocatorDefault,
                0,
            )
        };
        if v.is_null() {
            return None;
        }
        let v = unsafe { CFType::wrap_under_create_rule(v) };
        v.downcast::<CFBoolean>().map(bool::from)
    }

    /// True once per open → closed transition (starting with the lid closed is fine).
    fn just_closed(&mut self) -> bool {
        let Some(closed) = self.closed() else {
            return false;
        };
        let was_closed = std::mem::replace(&mut self.was_closed, closed);
        closed && !was_closed
    }
}

impl Drop for Lid {
    fn drop(&mut self) {
        unsafe { IOObjectRelease(self.root_domain) };
    }
}
