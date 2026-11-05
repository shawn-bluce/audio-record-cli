//! Thin FFI layer over the Core Audio HAL plus one capture "session":
//!
//!   Process Tap (global, stereo) ─┐
//!   Default input device (mic) ───┼─ private Aggregate Device ── IOProc ── ring buffer
//!   Default output device ────────┘   (output = clock master; mic + tap drift-compensated)
//!
//! Putting both sources in one aggregate device means a single IOProc receives
//! microphone and system samples for the same instant, so the tracks stay
//! sample-aligned. A session is bound to specific devices; when the user switches
//! devices the recorder tears the session down and builds a new one.

use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};
use ringbuf::HeapProd;
use ringbuf::traits::{Observer, Producer};

pub type AudioObjectID = u32;
pub type OSStatus = i32;

pub const fn fcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL: u32 = fcc(b"glob");
const SCOPE_INPUT: u32 = fcc(b"inpt");
const ELEMENT_MAIN: u32 = 0;

const HW_DEFAULT_INPUT_DEVICE: u32 = fcc(b"dIn ");
const HW_DEFAULT_OUTPUT_DEVICE: u32 = fcc(b"dOut");
const HW_RUN_LOOP: u32 = fcc(b"rnlp");
const OBJ_NAME: u32 = fcc(b"lnam");
const DEV_UID: u32 = fcc(b"uid ");
const DEV_IS_ALIVE: u32 = fcc(b"livn");
const DEV_NOMINAL_SAMPLE_RATE: u32 = fcc(b"nsrt");
const DEV_STREAM_CONFIGURATION: u32 = fcc(b"slay");
const DEV_STREAMS: u32 = fcc(b"stm#");
const STREAM_VIRTUAL_FORMAT: u32 = fcc(b"sfmt");

const FORMAT_LINEAR_PCM: u32 = fcc(b"lpcm");
const FORMAT_FLAG_IS_FLOAT: u32 = 1;
/// kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
const FORMAT_FLAGS_FLOAT_PACKED: u32 = 1 | 8;
const TIMESTAMP_HOST_TIME_VALID: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamBasicDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

impl AudioStreamBasicDescription {
    /// Packed, interleaved Float32 linear PCM.
    pub fn float32(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FORMAT_FLAGS_FLOAT_PACKED,
            bytes_per_packet: 4 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub channels: u32,
    pub byte_size: u32,
    pub data: *mut c_void,
}

/// Variable-length in C; `buffers` really has `count` entries.
#[repr(C)]
pub struct AudioBufferList {
    pub count: u32,
    pub buffers: [AudioBuffer; 1],
}

impl AudioBufferList {
    /// # Safety
    /// `self` must point to a valid AudioBufferList with `count` buffers.
    unsafe fn as_slice(&self) -> &[AudioBuffer] {
        unsafe { std::slice::from_raw_parts(self.buffers.as_ptr(), self.count as usize) }
    }
}

#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: [u8; 24],
    flags: u32,
    reserved: u32,
}

type IOProc = unsafe extern "C" fn(
    device: AudioObjectID,
    now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus;
type IOProcID = Option<IOProc>;

type ListenerProc = unsafe extern "C" fn(
    obj: AudioObjectID,
    count: u32,
    addrs: *const PropertyAddress,
    client: *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        obj: AudioObjectID,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        obj: AudioObjectID,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
        obj: AudioObjectID,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn AudioObjectAddPropertyListener(
        obj: AudioObjectID,
        addr: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectRemovePropertyListener(
        obj: AudioObjectID,
        addr: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> OSStatus;
    fn AudioHardwareCreateAggregateDevice(
        desc: CFDictionaryRef,
        out: *mut AudioObjectID,
    ) -> OSStatus;
    fn AudioHardwareDestroyAggregateDevice(dev: AudioObjectID) -> OSStatus;
    fn AudioHardwareCreateProcessTap(desc: *mut AnyObject, out: *mut AudioObjectID) -> OSStatus;
    fn AudioHardwareDestroyProcessTap(tap: AudioObjectID) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        dev: AudioObjectID,
        proc_: IOProc,
        client: *mut c_void,
        out: *mut IOProcID,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(dev: AudioObjectID, id: IOProcID) -> OSStatus;
    fn AudioDeviceStart(dev: AudioObjectID, id: IOProcID) -> OSStatus;
    fn AudioDeviceStop(dev: AudioObjectID, id: IOProcID) -> OSStatus;
    fn AudioConvertHostTimeToNanos(host_time: u64) -> u64;
    fn AudioGetCurrentHostTime() -> u64;
}

/// Render an OSStatus as a four-char code when printable (e.g. `'!obj'`), else as a number.
fn status_str(st: OSStatus) -> String {
    let b = (st as u32).to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        format!("'{}' ({st})", String::from_utf8_lossy(&b))
    } else {
        st.to_string()
    }
}

pub fn check(st: OSStatus, what: &str) -> Result<()> {
    if st == 0 {
        Ok(())
    } else {
        Err(anyhow!("{what} failed: OSStatus {}", status_str(st)))
    }
}

fn addr(selector: u32, scope: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

/// Read a fixed-size, plain-data global property.
fn get_prop<T: Copy>(obj: AudioObjectID, selector: u32) -> Result<T> {
    let mut val = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            &addr(selector, SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            val.as_mut_ptr().cast(),
        )
    };
    check(st, "Reading audio property")?;
    Ok(unsafe { val.assume_init() })
}

/// Variable-size property read. Returns the (8-byte aligned) storage and the byte length.
fn get_prop_bytes(obj: AudioObjectID, selector: u32, scope: u32) -> Result<(Vec<u64>, usize)> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    check(
        unsafe { AudioObjectGetPropertyDataSize(obj, &a, 0, ptr::null(), &mut size) },
        "Reading audio property size",
    )?;
    // u64 storage keeps the buffer 8-byte aligned for structs containing pointers.
    let mut buf = vec![0u64; (size as usize).div_ceil(8).max(1)];
    check(
        unsafe {
            AudioObjectGetPropertyData(
                obj,
                &a,
                0,
                ptr::null(),
                &mut size,
                buf.as_mut_ptr() as *mut c_void,
            )
        },
        "Reading audio property",
    )?;
    Ok((buf, size as usize))
}

fn get_string(obj: AudioObjectID, selector: u32) -> Result<String> {
    let s: CFStringRef = get_prop(obj, selector)?;
    if s.is_null() {
        bail!("Device string property is empty");
    }
    Ok(unsafe { CFString::wrap_under_create_rule(s) }.to_string())
}

/// Channel count of every buffer in the device's stream configuration for `scope`.
fn buffer_channels(dev: AudioObjectID, scope: u32) -> Result<Vec<u32>> {
    let (buf, _) = get_prop_bytes(dev, DEV_STREAM_CONFIGURATION, scope)?;
    let list = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    Ok(unsafe { list.as_slice() }
        .iter()
        .map(|b| b.channels)
        .collect())
}

/// Deliver HAL property notifications on the HAL's own thread. By default they
/// go to the main thread's CFRunLoop, which a CLI never runs.
pub fn use_hal_notification_thread() -> Result<()> {
    let a = addr(HW_RUN_LOOP, SCOPE_GLOBAL);
    let null_run_loop: *const c_void = ptr::null();
    check(
        unsafe {
            AudioObjectSetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                ptr::null(),
                size_of::<*const c_void>() as u32,
                &null_run_loop as *const _ as *const c_void,
            )
        },
        "Configuring Core Audio notifications",
    )
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: AudioObjectID,
    pub name: String,
    pub uid: String,
}

impl Device {
    fn load(id: AudioObjectID) -> Result<Self> {
        Ok(Self {
            id,
            name: get_string(id, OBJ_NAME).unwrap_or_else(|_| format!("Device #{id}")),
            uid: get_string(id, DEV_UID)?,
        })
    }

    pub fn default_input() -> Result<Self> {
        let id: AudioObjectID = get_prop(SYSTEM_OBJECT, HW_DEFAULT_INPUT_DEVICE)?;
        if id == 0 {
            bail!("No microphone found (no default input device)");
        }
        Self::load(id)
    }

    pub fn default_output() -> Result<Self> {
        let id: AudioObjectID = get_prop(SYSTEM_OBJECT, HW_DEFAULT_OUTPUT_DEVICE)?;
        if id == 0 {
            bail!("No default output device found");
        }
        Self::load(id)
    }

    /// False once the device has been unplugged / disappeared.
    pub fn is_alive(&self) -> bool {
        get_prop::<u32>(self.id, DEV_IS_ALIVE).is_ok_and(|v| v != 0)
    }

    pub fn nominal_rate(&self) -> Option<f64> {
        get_prop::<f64>(self.id, DEV_NOMINAL_SAMPLE_RATE).ok()
    }
}

// ---------------------------------------------------------------------------
// Device change notifications
// ---------------------------------------------------------------------------

/// Listener: just bump a generation counter; the recorder polls it and decides what to do.
unsafe extern "C" fn on_property_change(
    _obj: AudioObjectID,
    _count: u32,
    _addrs: *const PropertyAddress,
    client: *mut c_void,
) -> OSStatus {
    if !client.is_null() {
        let generation = unsafe { &*(client as *const AtomicU64) };
        generation.fetch_add(1, Ordering::SeqCst);
    }
    0
}

/// A set of property listeners that bump `generation`; removed on drop.
pub struct DeviceWatch {
    generation: Arc<AtomicU64>,
    registered: Vec<(AudioObjectID, PropertyAddress)>,
}

impl DeviceWatch {
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        Self {
            generation,
            registered: Vec::new(),
        }
    }

    fn client(&self) -> *mut c_void {
        Arc::as_ptr(&self.generation) as *mut c_void
    }

    fn add(&mut self, obj: AudioObjectID, selector: u32) -> Result<()> {
        let a = addr(selector, SCOPE_GLOBAL);
        check(
            unsafe { AudioObjectAddPropertyListener(obj, &a, on_property_change, self.client()) },
            "Registering audio device listener",
        )?;
        self.registered.push((obj, a));
        Ok(())
    }

    /// System default input / output changes.
    pub fn watch_defaults(&mut self) -> Result<()> {
        self.add(SYSTEM_OBJECT, HW_DEFAULT_INPUT_DEVICE)?;
        self.add(SYSTEM_OBJECT, HW_DEFAULT_OUTPUT_DEVICE)
    }

    /// Device unplugged or its sample rate changed (best effort).
    pub fn watch_device(&mut self, dev: &Device) {
        let _ = self.add(dev.id, DEV_IS_ALIVE);
        let _ = self.add(dev.id, DEV_NOMINAL_SAMPLE_RATE);
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        let client = self.client();
        for (obj, a) in &self.registered {
            unsafe { AudioObjectRemovePropertyListener(*obj, a, on_property_change, client) };
        }
    }
}

// ---------------------------------------------------------------------------
// Tap + aggregate device
// ---------------------------------------------------------------------------

/// Creates a global stereo process tap (captures everything the system plays).
/// Returns (tap object id, tap UUID string).
fn create_global_tap() -> Result<(AudioObjectID, String)> {
    let cls = AnyClass::get(c"CATapDescription").ok_or_else(|| {
        anyhow!("Core Audio process taps are not available (requires macOS 14.2+)")
    })?;
    unsafe {
        let exclude: Retained<NSArray<NSNumber>> = NSArray::new();
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        let desc: Retained<AnyObject> =
            msg_send![alloc, initStereoGlobalTapButExcludeProcesses: &*exclude];
        let name = NSString::from_str("arec system audio tap");
        let _: () = msg_send![&*desc, setName: &*name];
        let _: () = msg_send![&*desc, setPrivate: true];
        // CATapUnmuted (0): keep playing audio to the speakers while we capture it.
        let _: () = msg_send![&*desc, setMuteBehavior: 0isize];
        let uuid: Retained<NSUUID> = msg_send![&*desc, UUID];
        let uuid = uuid.UUIDString().to_string();

        let mut tap_id: AudioObjectID = 0;
        let st =
            AudioHardwareCreateProcessTap(Retained::as_ptr(&desc) as *mut AnyObject, &mut tap_id);
        check(st, "Creating system audio process tap")?;
        Ok((tap_id, uuid))
    }
}

fn cf_dict(pairs: &[(&str, CFType)]) -> CFDictionary<CFString, CFType> {
    let pairs: Vec<(CFString, CFType)> = pairs
        .iter()
        .map(|(k, v)| (CFString::new(k), v.clone()))
        .collect();
    CFDictionary::from_CFType_pairs(&pairs)
}

/// Sub-devices in aggregate order. The output device is the clock master: it must
/// be part of the aggregate so the HAL keeps it running even when nothing is
/// playing — otherwise the tap has no clock and IO stalls until audio starts.
fn sub_devices<'a>(mic: &'a Device, output: &'a Device) -> Vec<&'a Device> {
    if mic.uid == output.uid {
        vec![mic]
    } else {
        vec![mic, output]
    }
}

fn create_aggregate(mic: &Device, output: &Device, tap_uuid: &str) -> Result<AudioObjectID> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let uid = format!(
        "dev.arec.aggregate.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let subs: Vec<CFDictionary<CFString, CFType>> = sub_devices(mic, output)
        .into_iter()
        .map(|d| {
            cf_dict(&[
                ("uid", CFString::new(&d.uid).as_CFType()),
                // Everything not the clock master gets resampled to follow it.
                ("drift", CFBoolean::from(d.uid != output.uid).as_CFType()),
            ])
        })
        .collect();
    let sub_tap = cf_dict(&[
        ("uid", CFString::new(tap_uuid).as_CFType()),
        ("drift", CFBoolean::true_value().as_CFType()),
    ]);
    let desc = cf_dict(&[
        ("name", CFString::new("arec recorder").as_CFType()),
        ("uid", CFString::new(&uid).as_CFType()),
        ("master", CFString::new(&output.uid).as_CFType()),
        ("private", CFBoolean::true_value().as_CFType()),
        ("stacked", CFBoolean::false_value().as_CFType()),
        // Must be false: with auto-start the aggregate's IO stalls until some process
        // actually plays audio, so the microphone would record nothing while the
        // system is quiet. With it off, IO starts immediately and the tap delivers
        // silence until something plays.
        ("tapautostart", CFBoolean::false_value().as_CFType()),
        ("subdevices", CFArray::from_CFTypes(&subs).as_CFType()),
        ("taps", CFArray::from_CFTypes(&[sub_tap]).as_CFType()),
    ]);
    let mut id: AudioObjectID = 0;
    check(
        unsafe { AudioHardwareCreateAggregateDevice(desc.as_concrete_TypeRef(), &mut id) },
        "Creating aggregate audio device",
    )?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Real-time callback
// ---------------------------------------------------------------------------

/// State shared with the real-time IOProc. Only touched from the HAL IO thread
/// while the device runs; owned by `Capture` otherwise.
struct CallbackState {
    producer: HeapProd<f32>,
    /// Input buffer index ranges for each source (other sub-device inputs are ignored).
    mic: std::ops::Range<usize>,
    sys: std::ops::Range<usize>,
    scratch: Vec<f32>,
    dropped_frames: Arc<AtomicU64>,
    diag: Arc<Diag>,
}

/// Per-session counters and clock info, written lock-free from the IO thread.
#[derive(Default)]
pub struct Diag {
    pub callbacks: AtomicU64,
    frames: AtomicU64,
    /// Host time of the first input sample of the session (0 = no audio yet).
    first_host: AtomicU64,
    /// Host time of the first sample of the most recent callback, and its frame count.
    last_host: AtomicU64,
    last_frames: AtomicU64,
}

impl Diag {
    pub fn summary(&self) -> String {
        format!(
            "callbacks={} frames={}",
            self.callbacks.load(Ordering::Relaxed),
            self.frames.load(Ordering::Relaxed),
        )
    }

    /// Wall-clock (host time, ns) of the session's first sample.
    pub fn first_ns(&self) -> Option<u64> {
        match self.first_host.load(Ordering::SeqCst) {
            0 => None,
            t => Some(unsafe { AudioConvertHostTimeToNanos(t) }),
        }
    }

    /// Wall-clock (host time, ns) just after the session's last sample.
    pub fn end_ns(&self, rate: f64) -> Option<u64> {
        match self.last_host.load(Ordering::SeqCst) {
            0 => None,
            t => {
                let frames = self.last_frames.load(Ordering::SeqCst) as f64;
                Some(unsafe { AudioConvertHostTimeToNanos(t) } + (frames / rate * 1e9) as u64)
            }
        }
    }
}

const SCRATCH_FRAMES: usize = 4096;

/// Downmix all channels of `bufs` at `frame` to a single mono sample.
#[inline]
unsafe fn mono_sample(bufs: &[AudioBuffer], frame: usize) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0u32;
    for b in bufs {
        if b.data.is_null() || b.channels == 0 {
            continue;
        }
        let ch = b.channels as usize;
        let data = b.data as *const f32;
        for c in 0..ch {
            sum += unsafe { *data.add(frame * ch + c) };
        }
        n += b.channels;
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}

fn frames_in(bufs: &[AudioBuffer]) -> usize {
    bufs.iter()
        .filter(|b| b.channels > 0 && !b.data.is_null())
        .map(|b| b.byte_size as usize / (4 * b.channels as usize))
        .min()
        .unwrap_or(0)
}

/// Real-time callback: interleave (mic, system) mono pairs into the ring buffer.
/// No allocation, no locks, no syscalls.
unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let state = unsafe { &mut *(client as *mut CallbackState) };
    let bufs = unsafe { (*input).as_slice() };
    let pick = |r: &std::ops::Range<usize>| &bufs[r.start.min(bufs.len())..r.end.min(bufs.len())];
    let (mic, sys) = (pick(&state.mic), pick(&state.sys));

    let frames = match (frames_in(mic), frames_in(sys)) {
        (0, s) => s,
        (m, 0) => m,
        (m, s) => m.min(s),
    };

    let host = match unsafe { input_time.as_ref() } {
        Some(t) if t.flags & TIMESTAMP_HOST_TIME_VALID != 0 => t.host_time,
        _ => unsafe { AudioGetCurrentHostTime() },
    };
    let diag = &state.diag;
    if diag.first_host.load(Ordering::Relaxed) == 0 {
        diag.first_host.store(host, Ordering::SeqCst);
    }
    diag.last_host.store(host, Ordering::SeqCst);
    diag.last_frames.store(frames as u64, Ordering::SeqCst);
    diag.callbacks.fetch_add(1, Ordering::Relaxed);
    diag.frames.fetch_add(frames as u64, Ordering::Relaxed);

    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(SCRATCH_FRAMES);
        for i in 0..n {
            let f = done + i;
            state.scratch[2 * i] = unsafe { mono_sample(mic, f) };
            state.scratch[2 * i + 1] = unsafe { mono_sample(sys, f) };
        }
        let want = 2 * n;
        let room = state.producer.vacant_len() & !1;
        let take = want.min(room);
        state.producer.push_slice(&state.scratch[..take]);
        if take < want {
            state
                .dropped_frames
                .fetch_add(((want - take) / 2) as u64, Ordering::Relaxed);
        }
        done += n;
    }
    0
}

// ---------------------------------------------------------------------------
// Capture session
// ---------------------------------------------------------------------------

pub struct Capture {
    tap_id: AudioObjectID,
    agg_id: AudioObjectID,
    proc_id: IOProcID,
    state: *mut CallbackState,
    pub sample_rate: f64,
    pub diag: Arc<Diag>,
}

impl Capture {
    pub fn new(
        mic: &Device,
        output: &Device,
        producer: HeapProd<f32>,
        dropped_frames: Arc<AtomicU64>,
    ) -> Result<Self> {
        let (tap_id, tap_uuid) = create_global_tap()?;
        let mut cap = Self {
            tap_id,
            agg_id: 0,
            proc_id: None,
            state: ptr::null_mut(),
            sample_rate: 0.0,
            diag: Arc::new(Diag::default()),
        };
        cap.agg_id = create_aggregate(mic, output, &tap_uuid)?;

        cap.sample_rate = get_prop::<f64>(cap.agg_id, DEV_NOMINAL_SAMPLE_RATE)
            .context("Reading aggregate device sample rate")?;
        // Aggregate input buffers = each sub-device's input buffers in list order, then the tap.
        let mic_buffers = buffer_channels(mic.id, SCOPE_INPUT)
            .context("Reading microphone channel layout")?
            .len();
        let sub_inputs: usize = sub_devices(mic, output)
            .iter()
            .map(|d| {
                buffer_channels(d.id, SCOPE_INPUT)
                    .map(|v| v.len())
                    .unwrap_or(0)
            })
            .sum();
        let agg_buffers = buffer_channels(cap.agg_id, SCOPE_INPUT)
            .context("Reading aggregate device channel layout")?
            .len();
        if mic_buffers == 0 || agg_buffers <= sub_inputs {
            bail!(
                "Unexpected aggregate device layout ({agg_buffers} input buffers, \
                 {sub_inputs} from sub-devices, {mic_buffers} from the microphone)"
            );
        }
        let (mic_range, sys_range) = (0..mic_buffers, sub_inputs..agg_buffers);
        cap.verify_float_format()?;

        let state = Box::new(CallbackState {
            producer,
            mic: mic_range,
            sys: sys_range,
            scratch: vec![0.0; SCRATCH_FRAMES * 2],
            dropped_frames,
            diag: cap.diag.clone(),
        });
        cap.state = Box::into_raw(state);
        let mut pid: IOProcID = None;
        check(
            unsafe {
                AudioDeviceCreateIOProcID(cap.agg_id, io_proc, cap.state as *mut c_void, &mut pid)
            },
            "Registering audio callback",
        )?;
        cap.proc_id = pid;
        Ok(cap)
    }

    /// The HAL hands IOProcs the stream's virtual format; we rely on it being Float32.
    fn verify_float_format(&self) -> Result<()> {
        let (streams, bytes) = get_prop_bytes(self.agg_id, DEV_STREAMS, SCOPE_INPUT)?;
        let count = bytes / size_of::<AudioObjectID>();
        let ids =
            unsafe { std::slice::from_raw_parts(streams.as_ptr() as *const AudioObjectID, count) };
        for &sid in ids {
            let f: AudioStreamBasicDescription = get_prop(sid, STREAM_VIRTUAL_FORMAT)?;
            if f.format_id != FORMAT_LINEAR_PCM
                || f.format_flags & FORMAT_FLAG_IS_FLOAT == 0
                || f.bits_per_channel != 32
            {
                bail!("Unsupported input stream format (expected Float32 PCM): {f:?}");
            }
        }
        Ok(())
    }

    pub fn start(&mut self) -> Result<()> {
        check(
            unsafe { AudioDeviceStart(self.agg_id, self.proc_id) },
            "Starting audio capture",
        )
    }
}

impl Drop for Capture {
    /// Stop IO, destroy the IOProc (after which it can no longer run), then tear down
    /// the aggregate device and tap and free the callback state.
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.agg_id, self.proc_id); // harmless if never started
                AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id);
            }
            if self.agg_id != 0 {
                AudioHardwareDestroyAggregateDevice(self.agg_id);
            }
            if self.tap_id != 0 {
                AudioHardwareDestroyProcessTap(self.tap_id);
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
            }
        }
    }
}
