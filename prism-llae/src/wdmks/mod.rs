//! WDM Kernel Streaming backend: enumerate KS filters via SetupAPI, create wave
//! pins with `KsCreatePin`, and stream with overlapped IOCTLs (no WASAPI engine).

mod sys;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, HDEVINFO, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, HANDLE,
};
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::Media::KernelStreaming::{
    KsCreatePin, KSCATEGORY_AUDIO, KSDATAFORMAT_SUBTYPE_PCM, KSDATAFORMAT_TYPE_AUDIO,
    KSINTERFACESETID_Standard, KSMEDIUMSETID_Standard, KSPIN_COMMUNICATION_BOTH,
    KSPIN_COMMUNICATION_SINK, KSPIN_CONNECT, KSPIN_DATAFLOW_IN, KSPIN_DATAFLOW_OUT,
    KSPROPERTY_CONNECTION_STATE, KSPROPERTY_PIN_COMMUNICATION, KSPROPERTY_PIN_CTYPES,
    KSPROPERTY_PIN_DATAFLOW, KSPROPERTY_PIN_DATARANGES, KSPROPSETID_Connection, KSPROPSETID_Pin,
    KSDATAFORMAT_SPECIFIER_WAVEFORMATEX, KSSTATE_ACQUIRE, KSSTATE_PAUSE, KSSTATE_RUN, KSSTATE_STOP,
    KSSTREAM_HEADER, KSSTREAM_HEADER_OPTIONSF_DATADISCONTINUITY,
    KSSTREAM_HEADER_OPTIONSF_TIMEDISCONTINUITY, KSSTREAM_HEADER_OPTIONSF_TIMEVALID,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use self::sys::{
    ks_get_property, ks_get_property_size, ks_get_property_var, ks_set_property, KsDataFormatWfx,
    KsDataRangeAudio, KsIdentifier, KsPPin, KsPinConnect, KsProperty, GENERIC_READ, GENERIC_WRITE,
    GUID_NULL, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET,
};
use crate::duplex::{make_duplex, CaptureSink, RenderSource};
use crate::error::{EngineError, Result};
use crate::format::{SampleFormat, StreamFormat};
use crate::thread_prio::ProAudioGuard;
use crate::wfx::{WAVE_FORMAT_PCM};
use crate::{DeviceInfo, EngineConfig, Processor, ShareMode, Stream, StreamInfo, StreamStats};

// re-exported for sys.rs
pub use windows::Win32::Media::KernelStreaming::{
    IOCTL_KS_PROPERTY, IOCTL_KS_READ_STREAM, IOCTL_KS_WRITE_STREAM,
};

const KSINTERFACE_STANDARD_STREAMING: u32 = 0;
const KSMEDIUM_STANDARD_DEVIO: u32 = 0;
// 3 in-flight buffers per pin: glitch-free to ~32-frame periods, minimal latency
const N_BUFFERS: usize = 3;

struct Filter {
    path_w: Vec<u16>, // NUL-terminated wide path for CreateFileW
    name: String,
}

fn enumerate_filters() -> Result<Vec<Filter>> {
    let mut out = Vec::new();
    unsafe {
        let hdev: HDEVINFO = SetupDiGetClassDevsW(
            Some(&KSCATEGORY_AUDIO),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )?;

        let mut index = 0u32;
        loop {
            let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(hdev, None, &KSCATEGORY_AUDIO, index, &mut ifdata)
                .is_err()
            {
                break; // ERROR_NO_MORE_ITEMS
            }
            index += 1;

            // first call: required size
            let mut required = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &ifdata,
                None,
                0,
                Some(&mut required),
                None,
            );
            if required == 0 {
                continue;
            }

            let mut buf = vec![0u8; required as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            // cbSize is the fixed-header size, not the whole buffer
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            let mut devinfo = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };

            if SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &ifdata,
                Some(detail),
                required,
                None,
                Some(&mut devinfo),
            )
            .is_err()
            {
                continue;
            }

            let path_ptr = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
            let path_w = read_wide_z(path_ptr);
            let name = registry_name(hdev, &devinfo).unwrap_or_else(|| "<ks audio>".to_string());

            out.push(Filter { path_w, name });
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    Ok(out)
}

unsafe fn read_wide_z(mut p: *const u16) -> Vec<u16> {
    let mut v = Vec::new();
    while *p != 0 {
        v.push(*p);
        p = p.add(1);
    }
    v.push(0);
    v
}

fn registry_name(hdev: HDEVINFO, devinfo: &SP_DEVINFO_DATA) -> Option<String> {
    unsafe {
        for prop in [SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC] {
            let mut buf = [0u8; 512];
            if SetupDiGetDeviceRegistryPropertyW(
                hdev,
                devinfo,
                prop,
                None,
                Some(&mut buf),
                None,
            )
            .is_ok()
            {
                let wide: &[u16] = std::slice::from_raw_parts(buf.as_ptr() as *const u16, 256);
                let len = wide.iter().position(|&c| c == 0).unwrap_or(0);
                if len > 0 {
                    return Some(String::from_utf16_lossy(&wide[..len]));
                }
            }
        }
    }
    None
}

fn open_filter(path_w: &[u16]) -> Result<HANDLE> {
    unsafe {
        let h = CreateFileW(
            PCWSTR(path_w.as_ptr()),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )?;
        Ok(h)
    }
}

fn pin_count(filter: HANDLE) -> Result<u32> {
    let prop = KsProperty {
        set: KSPROPSETID_Pin,
        id: KSPROPERTY_PIN_CTYPES.0 as u32,
        flags: KSPROPERTY_TYPE_GET,
    };
    let count: u32 = unsafe { ks_get_property(filter, &prop)? };
    Ok(count)
}

fn pin_u32(filter: HANDLE, pin: u32, prop_id: i32) -> Result<u32> {
    let prop = KsPPin {
        set: KSPROPSETID_Pin,
        id: prop_id as u32,
        flags: KSPROPERTY_TYPE_GET,
        pin_id: pin,
        reserved: 0,
    };
    let v: u32 = unsafe { ks_get_property(filter, &prop)? };
    Ok(v)
}

struct PinMatch {
    pin_id: u32,
    format: StreamFormat,
}

fn is_audio_pcm(major: GUID, sub: GUID, spec: GUID) -> bool {
    let major_ok = major == KSDATAFORMAT_TYPE_AUDIO || major == GUID_NULL;
    let sub_ok = sub == KSDATAFORMAT_SUBTYPE_PCM || sub == GUID_NULL;
    let spec_ok = spec == KSDATAFORMAT_SPECIFIER_WAVEFORMATEX || spec == GUID_NULL;
    major_ok && sub_ok && spec_ok
}

// pick a concrete PCM format from a pin's data ranges for want_rate/want_ch
fn negotiate_pin_format(
    filter: HANDLE,
    pin: u32,
    want_rate: u32,
    want_ch: u16,
    force: Option<SampleFormat>,
) -> Option<StreamFormat> {
    let prop = KsPPin {
        set: KSPROPSETID_Pin,
        id: KSPROPERTY_PIN_DATARANGES.0 as u32,
        flags: KSPROPERTY_TYPE_GET,
        pin_id: pin,
        reserved: 0,
    };
    let size = unsafe { ks_get_property_size(filter, &prop).ok()? };
    if size < 8 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let got = unsafe { ks_get_property_var(filter, &prop, &mut buf).ok()? } as usize;
    if got < 8 {
        return None;
    }

    // leading KSMULTIPLE_ITEM { Size: u32, Count: u32 }
    let count = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let mut off = 8usize;
    for _ in 0..count {
        if off + std::mem::size_of::<KsDataRangeAudio>() > buf.len() {
            break;
        }
        // SAFETY: bounds checked; we only read POD fields.
        let r = unsafe { std::ptr::read_unaligned(buf[off..].as_ptr() as *const KsDataRangeAudio) };
        let entry_size = (r.format_size as usize).max(std::mem::size_of::<KsDataRangeAudio>());

        if is_audio_pcm(r.major_format, r.sub_format, r.specifier)
            && r.max_channels >= 1
            && want_rate >= r.min_sample_frequency
            && want_rate <= r.max_sample_frequency
        {
            let ch = want_ch.clamp(1, r.max_channels as u16);
            let bits_pref = match force {
                Some(SampleFormat::I16) => vec![16u32],
                Some(SampleFormat::I24) => vec![24],
                Some(SampleFormat::I32) => vec![32],
                Some(SampleFormat::F32) | None => vec![16, 24, 32],
            };
            let bits = match bits_pref
                .into_iter()
                .find(|b| *b >= r.min_bits_per_sample && *b <= r.max_bits_per_sample)
            {
                Some(b) => b,
                None => continue, // forced depth not in this range; try next
            };
            let sample_format = match bits {
                16 => SampleFormat::I16,
                24 => SampleFormat::I24,
                32 => SampleFormat::I32,
                _ => continue,
            };
            return Some(StreamFormat::new(want_rate, ch, sample_format));
        }
        off += (entry_size + 7) & !7; // 8-byte aligned
    }
    None
}

struct FoundPin {
    filter: HANDLE,
    pin: PinMatch,
    name: String,
}

// scan filters for a pin of the requested flow accepting PCM at want_rate/want_ch
fn find_pin(
    render: bool,
    want_rate: u32,
    want_ch: u16,
    select: Option<&str>,
    force: Option<SampleFormat>,
) -> Result<FoundPin> {
    let want_flow = if render {
        KSPIN_DATAFLOW_IN.0
    } else {
        KSPIN_DATAFLOW_OUT.0
    };
    for filter in enumerate_filters()? {
        if let Some(sel) = select {
            let path = String::from_utf16_lossy(&filter.path_w);
            if !path.contains(sel.trim_end_matches('\0')) {
                continue;
            }
        }
        let h = match open_filter(&filter.path_w) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let n = pin_count(h).unwrap_or(0);
        for pin in 0..n {
            let comm = pin_u32(h, pin, KSPROPERTY_PIN_COMMUNICATION.0).unwrap_or(0) as i32;
            if comm != KSPIN_COMMUNICATION_SINK.0 && comm != KSPIN_COMMUNICATION_BOTH.0 {
                continue;
            }
            let flow = pin_u32(h, pin, KSPROPERTY_PIN_DATAFLOW.0).unwrap_or(0) as i32;
            if flow != want_flow {
                continue;
            }
            if let Some(format) = negotiate_pin_format(h, pin, want_rate, want_ch, force) {
                return Ok(FoundPin {
                    filter: h,
                    pin: PinMatch { pin_id: pin, format },
                    name: filter.name.clone(),
                });
            }
        }
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    Err(EngineError::PinNotFound)
}

// instantiate a pin on `filter`; `render` selects data flow / access
fn create_pin(filter: HANDLE, pin_id: u32, format: StreamFormat, render: bool) -> Result<HANDLE> {
    #[repr(C)]
    struct PinConnectFull {
        connect: KsPinConnect,
        format: KsDataFormatWfx,
    }

    let block_align = format.frame_bytes() as u16;
    let wfx = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: format.sample_format.bits(),
        cbSize: 0,
    };

    let format_size = 64u32 + 18u32; // KSDATAFORMAT + WAVEFORMATEX

    let full = PinConnectFull {
        connect: KsPinConnect {
            interface: KsIdentifier {
                set: KSINTERFACESETID_Standard,
                id: KSINTERFACE_STANDARD_STREAMING,
                flags: 0,
            },
            medium: KsIdentifier {
                set: KSMEDIUMSETID_Standard,
                id: KSMEDIUM_STANDARD_DEVIO,
                flags: 0,
            },
            pin_id,
            pin_to_handle: std::ptr::null_mut(),
            priority_class: 1, // KSPRIORITY_NORMAL
            priority_sub_class: 1,
        },
        format: KsDataFormatWfx {
            format_size,
            flags: 0,
            sample_size: block_align as u32,
            reserved: 0,
            major_format: KSDATAFORMAT_TYPE_AUDIO,
            sub_format: KSDATAFORMAT_SUBTYPE_PCM,
            specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
            wfx,
        },
    };

    let access = if render { GENERIC_WRITE } else { GENERIC_READ };
    let mut pin_handle = HANDLE::default();
    let status = unsafe {
        KsCreatePin(
            filter,
            &full.connect as *const KsPinConnect as *const KSPIN_CONNECT,
            access,
            &mut pin_handle,
        )
    };
    if status != 0 || pin_handle.is_invalid() {
        return Err(EngineError::Backend(format!(
            "KsCreatePin failed (status 0x{status:08x}) for {}",
            format.describe()
        )));
    }
    Ok(pin_handle)
}

fn set_pin_state(pin: HANDLE, state: i32) -> Result<()> {
    let prop = KsProperty {
        set: KSPROPSETID_Connection,
        id: KSPROPERTY_CONNECTION_STATE.0 as u32,
        flags: KSPROPERTY_TYPE_SET,
    };
    unsafe { ks_set_property::<_, i32>(pin, &prop, &state)? };
    Ok(())
}

// == streaming ==============================================

struct KsBuf {
    data: Box<[u8]>,
    header: KSSTREAM_HEADER,
    ovl: Box<OVERLAPPED>,
    event: HANDLE,
}

struct PinStream {
    filter: HANDLE,
    pin: HANDLE,
    bufs: Vec<KsBuf>,
    frame_bytes: usize,
    period_frames: usize,
    sample_rate: u32,
}

// SAFETY: a PinStream is only touched by the thread it is moved to
unsafe impl Send for PinStream {}

impl Drop for PinStream {
    fn drop(&mut self) {
        unsafe {
            let _ = set_pin_state(self.pin, KSSTATE_PAUSE.0);
            let _ = set_pin_state(self.pin, KSSTATE_ACQUIRE.0);
            let _ = set_pin_state(self.pin, KSSTATE_STOP.0);

            let _ = CancelIoEx(self.pin, None);
            for b in &self.bufs {
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.pin, &*b.ovl, &mut n, true);
            }

            let _ = CloseHandle(self.pin);
            let _ = CloseHandle(self.filter);
            for b in &self.bufs {
                let _ = CloseHandle(b.event);
            }
        }
    }
}

fn make_pin_stream(
    filter: HANDLE,
    pin: HANDLE,
    format: StreamFormat,
    period_frames: usize,
) -> Result<PinStream> {
    let frame_bytes = format.frame_bytes();
    let cap = period_frames * frame_bytes;
    let mut bufs = Vec::with_capacity(N_BUFFERS);
    for _ in 0..N_BUFFERS {
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
        let mut data = vec![0u8; cap].into_boxed_slice();
        let mut header = KSSTREAM_HEADER::default();
        header.Size = std::mem::size_of::<KSSTREAM_HEADER>() as u32;
        header.FrameExtent = cap as u32;
        header.DataUsed = 0;
        header.Data = data.as_mut_ptr() as *mut c_void;
        let mut ovl = Box::new(OVERLAPPED::default());
        ovl.hEvent = event;
        bufs.push(KsBuf {
            data,
            header,
            ovl,
            event,
        });
    }
    Ok(PinStream {
        filter,
        pin,
        bufs,
        frame_bytes,
        period_frames,
        sample_rate: format.sample_rate,
    })
}

// submit one buffer for I/O; `write` selects direction
unsafe fn submit(pin: HANDLE, b: &mut KsBuf, write: bool) -> Result<()> {
    // reset overlapped state, keep the event
    let ev = b.event;
    *b.ovl = OVERLAPPED::default();
    b.ovl.hEvent = ev;
    b.header.DataUsed = if write { b.data.len() as u32 } else { 0 };

    let ioctl = if write {
        IOCTL_KS_WRITE_STREAM
    } else {
        IOCTL_KS_READ_STREAM
    };
    let r = windows::Win32::System::IO::DeviceIoControl(
        pin,
        ioctl,
        None,
        0,
        Some(&mut b.header as *mut KSSTREAM_HEADER as *mut c_void),
        std::mem::size_of::<KSSTREAM_HEADER>() as u32,
        None,
        Some(&mut *b.ovl),
    );
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => Ok(()),
        Err(e) => Err(EngineError::Windows(e)),
    }
}

fn render_loop(s: PinStream, source: RenderSource, info: StreamInfo, stop: Arc<AtomicBool>) {
    let _mmcss = ProAudioGuard::enter();
    let guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut s = s;
        let mut source = source;
        source.on_start(&info);
        let mut run = || -> Result<()> {
            unsafe {
                let _ = set_pin_state(s.pin, KSSTATE_ACQUIRE.0);
                let _ = set_pin_state(s.pin, KSSTATE_PAUSE.0);
                // pre-roll silence (zeroed buffers) to fill the queue
                for i in 0..s.bufs.len() {
                    submit(s.pin, &mut s.bufs[i], true)?;
                }
                set_pin_state(s.pin, KSSTATE_RUN.0)?;

                let events: Vec<HANDLE> = s.bufs.iter().map(|b| b.event).collect();
                let mut head = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    WaitForMultipleObjects(&events, false, 100);
                    // drain every completed buffer to keep all IRPs in rotation
                    loop {
                        let mut bytes = 0u32;
                        match GetOverlappedResult(s.pin, &*s.bufs[head].ovl, &mut bytes, false) {
                            Ok(()) => {
                                source.fill(&mut s.bufs[head].data, s.period_frames);
                                submit(s.pin, &mut s.bufs[head], true)?;
                                head = (head + 1) % s.bufs.len();
                            }
                            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => break,
                            Err(_) => break,
                        }
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("[prism-llae] ks render error: {e}");
        }
    }));
    if guard.is_err() {
        eprintln!("[prism-llae] ks render thread panicked; pin released via Drop");
    }
}

const HNS_PER_SEC: i64 = 10_000_000;

// KS has no discontinuity flag like WASAPI's, so infer one
enum GlitchMode {
    Timestamp { next_expected_pts: i64 },
    WallClock { start: Instant, frames_total: u64 },
}

struct GlitchDetector {
    rate: i64,
    period_frames: usize,
    mode: Option<GlitchMode>,
}

impl GlitchDetector {
    fn new(sample_rate: u32, period_frames: usize) -> Self {
        GlitchDetector {
            rate: sample_rate.max(1) as i64,
            period_frames,
            mode: None,
        }
    }

    fn observe(&mut self, header: &KSSTREAM_HEADER, frames: usize) -> bool {
        let hinted = header.OptionsFlags
            & (KSSTREAM_HEADER_OPTIONSF_DATADISCONTINUITY
                | KSSTREAM_HEADER_OPTIONSF_TIMEDISCONTINUITY)
            != 0;
        let dur = frames as i64 * HNS_PER_SEC / self.rate;

        match &mut self.mode {
            None => {
                if header.OptionsFlags & KSSTREAM_HEADER_OPTIONSF_TIMEVALID != 0 {
                    let pts = header.PresentationTime.Time;
                    self.mode = Some(GlitchMode::Timestamp {
                        next_expected_pts: pts + dur,
                    });
                } else {
                    self.mode = Some(GlitchMode::WallClock {
                        start: Instant::now(),
                        frames_total: 0,
                    });
                }
                false
            }
            Some(GlitchMode::Timestamp { next_expected_pts }) => {
                let pts = header.PresentationTime.Time;
                let tol = (self.period_frames as i64).max(1) * HNS_PER_SEC / self.rate / 2;
                let gap = pts - *next_expected_pts > tol;
                *next_expected_pts = pts + dur;
                gap || hinted
            }
            Some(GlitchMode::WallClock { start, frames_total }) => {
                *frames_total += frames as u64;
                let rate = self.rate as f64;
                let expected = *frames_total as f64 / rate;
                let actual = start.elapsed().as_secs_f64();
                let one_period = self.period_frames as f64 / rate;
                if actual - expected > N_BUFFERS as f64 * one_period {
                    *start = Instant::now();
                    *frames_total = 0;
                    true
                } else {
                    hinted
                }
            }
        }
    }
}

fn capture_loop(s: PinStream, sink: CaptureSink, stop: Arc<AtomicBool>) {
    let _mmcss = ProAudioGuard::enter();
    let guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut s = s;
        let mut sink = sink;
        let mut run = || -> Result<()> {
            unsafe {
                let _ = set_pin_state(s.pin, KSSTATE_ACQUIRE.0);
                let _ = set_pin_state(s.pin, KSSTATE_PAUSE.0);
                for i in 0..s.bufs.len() {
                    submit(s.pin, &mut s.bufs[i], false)?;
                }
                set_pin_state(s.pin, KSSTATE_RUN.0)?;

                let events: Vec<HANDLE> = s.bufs.iter().map(|b| b.event).collect();
                let mut head = 0usize;
                let mut detector = GlitchDetector::new(s.sample_rate, s.period_frames);
                while !stop.load(Ordering::Relaxed) {
                    WaitForMultipleObjects(&events, false, 100);
                    // drain every completed read so no captured audio is dropped
                    loop {
                        let mut transferred = 0u32;
                        match GetOverlappedResult(s.pin, &*s.bufs[head].ovl, &mut transferred, false) {
                            Ok(()) => {
                                // captured count is in header.DataUsed, NOT the IOCTL transfer (= header size)
                                let n = s.bufs[head].header.DataUsed as usize;
                                if n >= s.frame_bytes {
                                    let frames = n / s.frame_bytes;
                                    let disc = detector.observe(&s.bufs[head].header, frames);
                                    sink.submit(&s.bufs[head].data[..frames * s.frame_bytes], frames, disc);
                                }
                                submit(s.pin, &mut s.bufs[head], false)?;
                                head = (head + 1) % s.bufs.len();
                            }
                            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => break,
                            Err(_) => break,
                        }
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("[prism-llae] ks capture error: {e}");
        }
    }));
    if guard.is_err() {
        eprintln!("[prism-llae] ks capture thread panicked; pin released via Drop");
    }
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for filter in enumerate_filters()? {
        let h = match open_filter(&filter.path_w) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let n = pin_count(h).unwrap_or(0);
        let mut has_render = false;
        let mut has_capture = false;
        for pin in 0..n {
            let comm = pin_u32(h, pin, KSPROPERTY_PIN_COMMUNICATION.0).unwrap_or(0) as i32;
            if comm != KSPIN_COMMUNICATION_SINK.0 && comm != KSPIN_COMMUNICATION_BOTH.0 {
                continue;
            }
            match pin_u32(h, pin, KSPROPERTY_PIN_DATAFLOW.0).unwrap_or(0) as i32 {
                x if x == KSPIN_DATAFLOW_IN.0 => has_render = true,
                x if x == KSPIN_DATAFLOW_OUT.0 => has_capture = true,
                _ => {}
            }
        }
        unsafe {
            let _ = CloseHandle(h);
        }
        if has_render {
            out.push(DeviceInfo {
                id: String::from_utf16_lossy(&filter.path_w),
                name: format!("{} (render)", filter.name),
                is_capture: false,
            });
        }
        if has_capture {
            out.push(DeviceInfo {
                id: String::from_utf16_lossy(&filter.path_w),
                name: format!("{} (capture)", filter.name),
                is_capture: true,
            });
        }
    }
    Ok(out)
}

pub fn start(config: &EngineConfig, processor: Box<dyn Processor>) -> Result<Stream> {
    let want_rate = if config.sample_rate != 0 {
        config.sample_rate
    } else {
        48_000
    };
    let want_ch = if config.channels != 0 {
        config.channels
    } else {
        2
    };
    let period_frames = if config.buffer_frames != 0 {
        config.buffer_frames as usize
    } else {
        256
    };

    let render = find_pin(
        true,
        want_rate,
        want_ch,
        config.render_device.as_deref(),
        config.force_format,
    )?;
    let capture = if config.output_only {
        None
    } else {
        match find_pin(
            false,
            want_rate,
            want_ch,
            config.capture_device.as_deref(),
            config.force_format,
        ) {
            Ok(p) => Some(p),
            Err(_) if config.capture_device.is_none() => None,
            Err(e) => return Err(e),
        }
    };

    let render_pin = create_pin(render.filter, render.pin.pin_id, render.pin.format, true)?;

    let proc_ch = render.pin.format.channels as usize;
    let cap_fmt = capture
        .as_ref()
        .map(|c| c.pin.format)
        .unwrap_or(render.pin.format);
    let info = StreamInfo {
        backend: crate::Backend::WdmKs,
        share_mode: ShareMode::Exclusive,
        capture_format: capture.as_ref().map(|c| c.pin.format),
        render_format: render.pin.format,
        channels: render.pin.format.channels,
        period_frames: period_frames as u32,
        capture_period_frames: if capture.is_some() {
            period_frames as u32
        } else {
            0
        },
    };
    match &capture {
        Some(c) => eprintln!(
            "[prism-llae] ks render pin: {} ({}), capture pin: {} ({})",
            render.name,
            render.pin.format.describe(),
            c.name,
            c.pin.format.describe()
        ),
        None => eprintln!(
            "[prism-llae] ks render pin: {} ({}), capture: none (render-only)",
            render.name,
            render.pin.format.describe()
        ),
    }

    let stats = Arc::new(StreamStats::default());
    let ring_frames = (period_frames * N_BUFFERS * 4).next_power_of_two();
    // cushion so render pulls don't race ahead of capture pushes
    let prime_frames = period_frames * 2;
    let (sink, source) = make_duplex(
        cap_fmt,
        render.pin.format,
        proc_ch,
        ring_frames,
        prime_frames,
        processor,
        stats.clone(),
    );

    let render_stream = make_pin_stream(render.filter, render_pin, render.pin.format, period_frames)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_r = stop.clone();
    let info_r = info.clone();
    let t_ren = std::thread::Builder::new()
        .name("ks-render".into())
        .spawn(move || render_loop(render_stream, source, info_r, stop_r))
        .map_err(|e| EngineError::Backend(format!("spawn ks render: {e}")))?;
    let mut threads = vec![t_ren];

    if let Some(c) = capture {
        let capture_pin = create_pin(c.filter, c.pin.pin_id, c.pin.format, false)?;
        let capture_stream = make_pin_stream(c.filter, capture_pin, c.pin.format, period_frames)?;
        let stop_c = stop.clone();
        let t_cap = std::thread::Builder::new()
            .name("ks-capture".into())
            .spawn(move || capture_loop(capture_stream, sink, stop_c))
            .map_err(|e| EngineError::Backend(format!("spawn ks capture: {e}")))?;
        threads.push(t_cap);
    } else {
        drop(sink);
    }

    Ok(Stream::new(stop, threads, stats, info))
}
