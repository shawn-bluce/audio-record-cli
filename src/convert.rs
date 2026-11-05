//! Wrapper around an AudioToolbox `AudioConverter` fed with interleaved
//! (mic, system) Float32: the engine behind both resampling and AAC encoding.

use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of, size_of_val};
use std::ptr;

use anyhow::{Result, bail};

use crate::audio::CHANNELS;
use crate::coreaudio::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, OSStatus, check, fcc,
};

type AudioConverterRef = *mut c_void;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AudioStreamPacketDescription {
    pub start_offset: i64,
    pub variable_frames_in_packet: u32,
    pub data_byte_size: u32,
}

type InputProc = unsafe extern "C" fn(
    conv: AudioConverterRef,
    io_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> OSStatus;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(
        src: *const AudioStreamBasicDescription,
        dst: *const AudioStreamBasicDescription,
        out: *mut AudioConverterRef,
    ) -> OSStatus;
    fn AudioConverterSetProperty(
        c: AudioConverterRef,
        id: u32,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn AudioConverterGetProperty(
        c: AudioConverterRef,
        id: u32,
        size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioConverterGetPropertyInfo(
        c: AudioConverterRef,
        id: u32,
        size: *mut u32,
        writable: *mut u8,
    ) -> OSStatus;
    fn AudioConverterFillComplexBuffer(
        c: AudioConverterRef,
        input: InputProc,
        user: *mut c_void,
        io_packets: *mut u32,
        out: *mut AudioBufferList,
        out_desc: *mut AudioStreamPacketDescription,
    ) -> OSStatus;
    fn AudioConverterDispose(c: AudioConverterRef) -> OSStatus;
}

/// Private status meaning "no more input right now" — the converter keeps its state.
const NEED_MORE_INPUT: OSStatus = fcc(b"arMI") as OSStatus;

/// Input handed to the converter during one conversion.
struct Feeder {
    data: *const f32,
    frames: usize,
    end_of_stream: bool,
}

impl Feeder {
    /// `None` signals the end of the stream, so the converter drains its tail.
    fn new(input: Option<&[f32]>) -> Self {
        Self {
            data: input.map_or(ptr::null(), <[f32]>::as_ptr),
            frames: input.map_or(0, |i| i.len() / CHANNELS),
            end_of_stream: input.is_none(),
        }
    }
}

unsafe extern "C" fn feed_input(
    _conv: AudioConverterRef,
    io_packets: *mut u32,
    io_data: *mut AudioBufferList,
    _out_desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> OSStatus {
    let f = unsafe { &mut *(user as *mut Feeder) };
    let list = unsafe { &mut *io_data };
    let n = f.frames.min(unsafe { *io_packets } as usize);
    list.buffers[0] = AudioBuffer {
        channels: CHANNELS as u32,
        byte_size: (n * CHANNELS * 4) as u32,
        data: if n == 0 {
            ptr::null_mut()
        } else {
            f.data as *mut c_void
        },
    };
    unsafe { *io_packets = n as u32 };
    if n == 0 {
        // noErr + 0 packets = end of stream (flushes the tail); otherwise "come back later".
        return if f.end_of_stream { 0 } else { NEED_MORE_INPUT };
    }
    f.data = unsafe { f.data.add(n * CHANNELS) };
    f.frames -= n;
    0
}

pub struct Converter {
    raw: AudioConverterRef,
    /// Boxed: the converter's input callback holds its address during a conversion.
    feeder: Box<Feeder>,
}

// Only ever used from one thread at a time.
unsafe impl Send for Converter {}

impl Converter {
    /// `src` is always interleaved stereo Float32.
    pub fn new(
        src: &AudioStreamBasicDescription,
        dst: &AudioStreamBasicDescription,
        what: &str,
    ) -> Result<Self> {
        let mut raw = ptr::null_mut();
        check(unsafe { AudioConverterNew(src, dst, &mut raw) }, what)?;
        Ok(Self {
            raw,
            feeder: Box::new(Feeder::new(Some(&[]))),
        })
    }

    pub fn set(&self, id: u32, val: u32, what: &str) -> Result<()> {
        let st = unsafe { AudioConverterSetProperty(self.raw, id, 4, (&raw const val).cast()) };
        check(st, what)
    }

    /// Read a fixed-size, plain-data property.
    pub fn get<T: Copy>(&self, id: u32, what: &str) -> Result<T> {
        let mut val = MaybeUninit::<T>::zeroed();
        let mut size = size_of::<T>() as u32;
        let st =
            unsafe { AudioConverterGetProperty(self.raw, id, &mut size, val.as_mut_ptr().cast()) };
        check(st, what)?;
        if size as usize != size_of::<T>() {
            bail!("{what}: unexpected property size {size}");
        }
        Ok(unsafe { val.assume_init() })
    }

    /// Read a variable-size property.
    pub fn get_bytes(&self, id: u32, what: &str) -> Result<Vec<u8>> {
        let (mut size, mut writable) = (0u32, 0u8);
        check(
            unsafe { AudioConverterGetPropertyInfo(self.raw, id, &mut size, &mut writable) },
            what,
        )?;
        let mut buf = vec![0u8; size as usize];
        check(
            unsafe { AudioConverterGetProperty(self.raw, id, &mut size, buf.as_mut_ptr().cast()) },
            what,
        )?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// PCM output: convert `input` (`None` = end of stream) and append the
    /// result to `out`, using `scratch` as the converter's output buffer.
    pub fn pcm(
        &mut self,
        input: Option<&[f32]>,
        scratch: &mut [f32],
        out: &mut Vec<f32>,
    ) -> Result<()> {
        let max_frames = scratch.len() / CHANNELS;
        self.run(input, |c| {
            let (n, more) = c.fill(scratch, max_frames, ptr::null_mut())?;
            out.extend_from_slice(&scratch[..n * CHANNELS]);
            Ok(more)
        })
    }

    /// Packet output: convert `input` (`None` = end of stream) and pass every
    /// finished packet to `sink`. `buf` must fit `descs.len()` packets.
    pub fn packets(
        &mut self,
        input: Option<&[f32]>,
        buf: &mut [u8],
        descs: &mut [AudioStreamPacketDescription],
        mut sink: impl FnMut(&[u8]),
    ) -> Result<()> {
        self.run(input, |c| {
            let (n, more) = c.fill(buf, descs.len(), descs.as_mut_ptr())?;
            for d in &descs[..n] {
                let start = d.start_offset as usize;
                sink(&buf[start..start + d.data_byte_size as usize]);
            }
            Ok(more)
        })
    }

    /// Feed `input` and call `step` until it reports that the converter is done with it.
    fn run(
        &mut self,
        input: Option<&[f32]>,
        mut step: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        *self.feeder = Feeder::new(input);
        let result = loop {
            match step(self) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        // Never keep a pointer into `input` beyond this call.
        *self.feeder = Feeder::new(Some(&[]));
        result
    }

    /// One conversion call into `out`. Returns the packets produced and whether
    /// more output may follow from the current input.
    fn fill<T>(
        &mut self,
        out: &mut [T],
        max_packets: usize,
        descs: *mut AudioStreamPacketDescription,
    ) -> Result<(usize, bool)> {
        let mut packets = max_packets as u32;
        let mut list = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: CHANNELS as u32,
                byte_size: size_of_val(out) as u32,
                data: out.as_mut_ptr().cast(),
            }],
        };
        let st = unsafe {
            AudioConverterFillComplexBuffer(
                self.raw,
                feed_input,
                (&raw mut *self.feeder).cast(),
                &mut packets,
                &mut list,
                descs,
            )
        };
        let n = packets as usize;
        match st {
            NEED_MORE_INPUT => Ok((n, false)),
            0 => Ok((n, n > 0)),
            e => check(e, "AudioConverter").map(|_| (0, false)),
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.raw) };
    }
}
