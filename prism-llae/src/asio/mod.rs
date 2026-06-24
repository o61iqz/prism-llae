//! ASIO backend. Natively full-duplex: the `Processor` runs directly in
//! `bufferSwitch` (no ring). Only one driver may be active, so the live runtime
//! lives in a process-global pointer the C callbacks dereference.

mod sys;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use windows::core::{GUID, PCWSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, CLSIDFromString, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    RRF_RT_REG_SZ,
};

use self::sys::*;
use crate::error::{EngineError, Result};
use crate::format::{SampleFormat, StreamFormat};
use crate::{
    Backend, DeviceInfo, EngineConfig, Processor, ShareMode, Stream, StreamInfo, StreamStats,
};

// live runtime for the C callbacks; non-null only while streaming
static CURRENT: AtomicPtr<AsioRuntime> = AtomicPtr::new(std::ptr::null_mut());
// guards against a second concurrent ASIO stream
static IN_USE: AtomicBool = AtomicBool::new(false);

// == registry ===============================================

// installed ASIO drivers as (name, clsid)
fn enumerate_drivers() -> Vec<(String, GUID)> {
    let mut out = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!("SOFTWARE\\ASIO"),
            0,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return out;
        }

        let mut index = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let r = RegEnumKeyExW(
                hkey,
                index,
                windows::core::PWSTR(name.as_mut_ptr()),
                &mut name_len,
                None,
                windows::core::PWSTR::null(),
                None,
                None,
            );
            if r.is_err() {
                break;
            }
            index += 1;

            let subkey: Vec<u16> = name[..name_len as usize]
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .collect();

            let mut clsid_buf = [0u16; 128];
            let mut size = (clsid_buf.len() * 2) as u32;
            if RegGetValueW(
                hkey,
                PCWSTR(subkey.as_ptr()),
                windows::core::w!("CLSID"),
                RRF_RT_REG_SZ,
                None,
                Some(clsid_buf.as_mut_ptr() as *mut c_void),
                Some(&mut size),
            )
            .is_ok()
            {
                if let Ok(clsid) = CLSIDFromString(PCWSTR(clsid_buf.as_ptr())) {
                    let nm = String::from_utf16_lossy(&name[..name_len as usize]);
                    out.push((nm, clsid));
                }
            }
        }
        let _ = RegCloseKey(hkey);
    }
    out
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for (name, clsid) in enumerate_drivers() {
        let id = format!("{clsid:?}");
        // one full-duplex device; surface on both sides for --in / --out
        out.push(DeviceInfo {
            id: id.clone(),
            name: format!("{name} (render)"),
            is_capture: false,
        });
        out.push(DeviceInfo {
            id,
            name: format!("{name} (capture)"),
            is_capture: true,
        });
    }
    Ok(out)
}

fn pick_driver(config: &EngineConfig) -> Option<(String, GUID)> {
    let drivers = enumerate_drivers();
    let sel = config
        .render_device
        .as_deref()
        .or(config.capture_device.as_deref());
    match sel {
        Some(want) => {
            let w = want.to_lowercase();
            drivers
                .into_iter()
                .find(|(n, c)| n.to_lowercase().contains(&w) || format!("{c:?}").contains(want))
        }
        None => drivers.into_iter().next(),
    }
}

// == sample conversion ======================================

#[inline]
unsafe fn read_sample(base: *const u8, st: i32, frame: usize) -> f32 {
    match st {
        ASIOST_INT16_LSB => {
            let p = base.add(frame * 2) as *const i16;
            p.read_unaligned() as f32 / 32_768.0
        }
        ASIOST_INT24_LSB => {
            let p = base.add(frame * 3);
            let b0 = *p as i32;
            let b1 = *p.add(1) as i32;
            let b2 = *p.add(2) as i32;
            let raw = b0 | (b1 << 8) | (b2 << 16);
            ((raw << 8) >> 8) as f32 / 8_388_608.0
        }
        ASIOST_INT32_LSB => {
            let p = base.add(frame * 4) as *const i32;
            p.read_unaligned() as f32 / 2_147_483_648.0
        }
        ASIOST_FLOAT32_LSB => {
            let p = base.add(frame * 4) as *const f32;
            p.read_unaligned()
        }
        // right-justified ints in a 32-bit container
        ASIOST_INT32_LSB16 => int32_scaled(base, frame, 15),
        ASIOST_INT32_LSB18 => int32_scaled(base, frame, 17),
        ASIOST_INT32_LSB20 => int32_scaled(base, frame, 19),
        ASIOST_INT32_LSB24 => int32_scaled(base, frame, 23),
        _ => 0.0,
    }
}

#[inline]
unsafe fn int32_scaled(base: *const u8, frame: usize, shift: u32) -> f32 {
    let p = base.add(frame * 4) as *const i32;
    p.read_unaligned() as f32 / (1u32 << shift) as f32
}

#[inline]
unsafe fn write_sample(base: *mut u8, st: i32, frame: usize, v: f32) {
    let c = v.clamp(-1.0, 1.0);
    match st {
        ASIOST_INT16_LSB => {
            let p = base.add(frame * 2) as *mut i16;
            p.write_unaligned((c * 32_767.0) as i16);
        }
        ASIOST_INT24_LSB => {
            let p = base.add(frame * 3);
            let s = (c * 8_388_607.0) as i32;
            *p = s as u8;
            *p.add(1) = (s >> 8) as u8;
            *p.add(2) = (s >> 16) as u8;
        }
        ASIOST_INT32_LSB => {
            let p = base.add(frame * 4) as *mut i32;
            p.write_unaligned((c as f64 * 2_147_483_647.0) as i32);
        }
        ASIOST_FLOAT32_LSB => {
            let p = base.add(frame * 4) as *mut f32;
            p.write_unaligned(c);
        }
        ASIOST_INT32_LSB16 => write_int32_scaled(base, frame, c, 15),
        ASIOST_INT32_LSB18 => write_int32_scaled(base, frame, c, 17),
        ASIOST_INT32_LSB20 => write_int32_scaled(base, frame, c, 19),
        ASIOST_INT32_LSB24 => write_int32_scaled(base, frame, c, 23),
        _ => {}
    }
}

#[inline]
unsafe fn write_int32_scaled(base: *mut u8, frame: usize, v: f32, shift: u32) {
    let p = base.add(frame * 4) as *mut i32;
    let max = ((1u32 << shift) - 1) as f32;
    p.write_unaligned((v * max) as i32);
}

fn sample_format_of(st: i32) -> SampleFormat {
    match st {
        ASIOST_INT16_LSB => SampleFormat::I16,
        ASIOST_INT24_LSB => SampleFormat::I24,
        ASIOST_FLOAT32_LSB => SampleFormat::F32,
        _ => SampleFormat::I32, // all 32-bit int variants
    }
}

fn valid_bits_of(st: i32) -> u16 {
    match st {
        ASIOST_INT16_LSB => 16,
        ASIOST_INT24_LSB => 24,
        ASIOST_INT32_LSB16 => 16,
        ASIOST_INT32_LSB18 => 18,
        ASIOST_INT32_LSB20 => 20,
        ASIOST_INT32_LSB24 => 24,
        ASIOST_FLOAT32_LSB => 32,
        _ => 32,
    }
}

// == runtime ================================================

struct AsioRuntime {
    driver: *mut IAsio,
    channels: usize,
    sample_type: i32,
    buffer_size: usize,
    in_ptrs: Vec<[*mut c_void; 2]>,
    out_ptrs: Vec<[*mut c_void; 2]>,
    in_f32: Vec<f32>,
    out_f32: Vec<f32>,
    proc: Box<dyn Processor>,
    output_ready: bool,
    output_only: bool,
    stats: Arc<StreamStats>,
}

// SAFETY: never accessed concurrently (control thread, then callback thread)
unsafe impl Send for AsioRuntime {}

impl AsioRuntime {
    fn process(&mut self, index: usize) {
        let n = self.buffer_size;
        let ch = self.channels;

        if self.output_only {
            for s in self.in_f32[..n * ch].iter_mut() {
                *s = 0.0;
            }
        } else {
            for c in 0..ch {
                let base = self.in_ptrs[c][index] as *const u8;
                for f in 0..n {
                    self.in_f32[f * ch + c] = unsafe { read_sample(base, self.sample_type, f) };
                }
            }
        }

        for s in self.out_f32[..n * ch].iter_mut() {
            *s = 0.0;
        }
        self.proc
            .process(&self.in_f32[..n * ch], &mut self.out_f32[..n * ch], ch);

        for c in 0..ch {
            let base = self.out_ptrs[c][index] as *mut u8;
            for f in 0..n {
                unsafe { write_sample(base, self.sample_type, f, self.out_f32[f * ch + c]) };
            }
        }

        if self.output_ready {
            unsafe {
                ((*self.driver).vtbl().output_ready)(self.driver);
            }
        }

        self.stats.render_calls.fetch_add(1, Ordering::Relaxed);
        self.stats.capture_calls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .render_frames
            .fetch_add(n as u64, Ordering::Relaxed);
        self.stats
            .capture_frames
            .fetch_add(n as u64, Ordering::Relaxed);
    }
}

// == callbacks ==============================================

extern "system" fn cb_buffer_switch(index: i32, _direct: i32) {
    let rt = CURRENT.load(Ordering::Acquire);
    if !rt.is_null() {
        // SAFETY: non-null only while the runtime is alive
        unsafe { (*rt).process(index.max(0) as usize) };
    }
}

extern "system" fn cb_sample_rate_did_change(_rate: f64) {}

extern "system" fn cb_asio_message(
    selector: i32,
    value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => match value {
            K_ASIO_ENGINE_VERSION
            | K_ASIO_RESET_REQUEST
            | K_ASIO_BUFFER_SIZE_CHANGE
            | K_ASIO_RESYNC_REQUEST
            | K_ASIO_LATENCIES_CHANGED
            | K_ASIO_SUPPORTS_TIME_INFO => 1,
            _ => 0,
        },
        K_ASIO_ENGINE_VERSION => 2,
        K_ASIO_SUPPORTS_TIME_INFO => 0, // we use plain bufferSwitch
        _ => 0,
    }
}

extern "system" fn cb_buffer_switch_time_info(
    _params: *mut c_void,
    index: i32,
    direct: i32,
) -> *mut c_void {
    cb_buffer_switch(index, direct);
    std::ptr::null_mut()
}

static CALLBACKS: AsioCallbacks = AsioCallbacks {
    buffer_switch: cb_buffer_switch,
    sample_rate_did_change: cb_sample_rate_did_change,
    asio_message: cb_asio_message,
    buffer_switch_time_info: cb_buffer_switch_time_info,
};

// == lifecycle ==============================================

unsafe fn driver_error(driver: *mut IAsio) -> String {
    let mut buf = [0u8; 256];
    ((*driver).vtbl().get_error_message)(driver, buf.as_mut_ptr());
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

// STA control thread: build, start, park, tear down
fn run_control(
    config: EngineConfig,
    mut processor: Box<dyn Processor>,
    stats: Arc<StreamStats>,
    setup_tx: mpsc::Sender<Result<StreamInfo>>,
    stop: Arc<AtomicBool>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let result = (|| -> Result<(StreamInfo, *mut AsioRuntime, *mut IAsio)> {
            let (_name, clsid) =
                pick_driver(&config).ok_or(EngineError::NoDevice)?;

            let mut raw: *mut c_void = std::ptr::null_mut();
            CoCreateInstance(&clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &clsid, &mut raw)
                .ok()?;
            let driver = raw as *mut IAsio;

            if ((*driver).vtbl().init)(driver, std::ptr::null_mut()) != ASIO_TRUE {
                let msg = driver_error(driver);
                ((*driver).vtbl().release)(driver);
                return Err(EngineError::Backend(format!("ASIO init failed: {msg}")));
            }

            let (mut n_in, mut n_out) = (0i32, 0i32);
            ((*driver).vtbl().get_channels)(driver, &mut n_in, &mut n_out);

            if std::env::var_os("AUDIO_DEBUG").is_some() {
                for (is_in, count) in [(1i32, n_in), (0i32, n_out)] {
                    for c in 0..count {
                        let mut ci = AsioChannelInfo {
                            channel: c,
                            is_input: is_in,
                            ..Default::default()
                        };
                        ((*driver).vtbl().get_channel_info)(driver, &mut ci);
                        let len = ci.name.iter().position(|&b| b == 0).unwrap_or(32);
                        let nm = String::from_utf8_lossy(&ci.name[..len]);
                        eprintln!(
                            "[dbg] {} ch {c}: {nm}",
                            if is_in == 1 { "in " } else { "out" }
                        );
                    }
                }
            }
            let want_ch = if config.channels != 0 {
                config.channels as i32
            } else {
                2
            };
            let channels = want_ch.min(n_in).min(n_out).max(1) as usize;

            // Sample rate.
            let want_rate = if config.sample_rate != 0 {
                config.sample_rate as f64
            } else {
                let mut cur = 0.0f64;
                ((*driver).vtbl().get_sample_rate)(driver, &mut cur);
                if cur <= 0.0 {
                    48_000.0
                } else {
                    cur
                }
            };
            if ((*driver).vtbl().can_sample_rate)(driver, want_rate) == ASE_OK {
                ((*driver).vtbl().set_sample_rate)(driver, want_rate);
            }
            let mut rate = 0.0f64;
            ((*driver).vtbl().get_sample_rate)(driver, &mut rate);

            // Buffer size.
            let (mut bmin, mut bmax, mut bpref, mut bgran) = (0i32, 0i32, 0i32, 0i32);
            ((*driver).vtbl().get_buffer_size)(
                driver, &mut bmin, &mut bmax, &mut bpref, &mut bgran,
            );
            let buffer_size = if config.buffer_frames != 0 {
                (config.buffer_frames as i32).clamp(bmin.max(1), bmax.max(bmin.max(1)))
            } else {
                bpref.max(1)
            } as usize;

            // sample type from the first output channel
            let mut ci = AsioChannelInfo {
                channel: 0,
                is_input: 0,
                ..Default::default()
            };
            ((*driver).vtbl().get_channel_info)(driver, &mut ci);
            let sample_type = ci.sample_type;

            // channel base offsets (clamped so base+channels fits)
            let in_base = (config.input_channel as i32).min((n_in - channels as i32).max(0));
            let out_base = (config.output_channel as i32).min((n_out - channels as i32).max(0));

            // buffer infos: `channels` inputs then `channels` outputs
            let mut infos: Vec<AsioBufferInfo> = Vec::with_capacity(channels * 2);
            for c in 0..channels {
                infos.push(AsioBufferInfo {
                    is_input: 1,
                    channel_num: in_base + c as i32,
                    buffers: [std::ptr::null_mut(); 2],
                });
            }
            for c in 0..channels {
                infos.push(AsioBufferInfo {
                    is_input: 0,
                    channel_num: out_base + c as i32,
                    buffers: [std::ptr::null_mut(); 2],
                });
            }

            let err = ((*driver).vtbl().create_buffers)(
                driver,
                infos.as_mut_ptr(),
                (channels * 2) as i32,
                buffer_size as i32,
                &CALLBACKS,
            );
            if err != ASE_OK {
                let msg = driver_error(driver);
                ((*driver).vtbl().release)(driver);
                return Err(EngineError::Backend(format!(
                    "ASIO createBuffers failed ({err}): {msg}"
                )));
            }

            let in_ptrs: Vec<[*mut c_void; 2]> =
                infos[..channels].iter().map(|i| i.buffers).collect();
            let out_ptrs: Vec<[*mut c_void; 2]> =
                infos[channels..].iter().map(|i| i.buffers).collect();

            let output_ready = ((*driver).vtbl().output_ready)(driver) == ASE_OK;

            let (mut in_lat, mut out_lat) = (0i32, 0i32);
            ((*driver).vtbl().get_latencies)(driver, &mut in_lat, &mut out_lat);

            let fmt = StreamFormat {
                sample_rate: rate as u32,
                channels: channels as u16,
                sample_format: sample_format_of(sample_type),
                valid_bits: valid_bits_of(sample_type),
            };
            let info = StreamInfo {
                backend: Backend::Asio,
                share_mode: ShareMode::Exclusive,
                capture_format: if config.output_only { None } else { Some(fmt) },
                render_format: fmt,
                channels: channels as u16,
                period_frames: buffer_size as u32,
                capture_period_frames: if config.output_only {
                    0
                } else {
                    buffer_size as u32
                },
            };
            eprintln!(
                "[prism-llae] ASIO: {} in / {} out ch, buffer {} frames, driver latency in:{} out:{}",
                n_in, n_out, buffer_size, in_lat, out_lat
            );

            let mut runtime = Box::new(AsioRuntime {
                driver,
                channels,
                sample_type,
                buffer_size,
                in_ptrs,
                out_ptrs,
                in_f32: vec![0.0; buffer_size * channels],
                out_f32: vec![0.0; buffer_size * channels],
                proc: std::mem::replace(&mut processor, Box::new(NullProc)),
                output_ready,
                output_only: config.output_only,
                stats: stats.clone(),
            });
            runtime.proc.on_start(&info);
            let rt_ptr = Box::into_raw(runtime);

            // publish before starting so callbacks find it
            CURRENT.store(rt_ptr, Ordering::Release);
            if ((*driver).vtbl().start)(driver) != ASE_OK {
                let msg = driver_error(driver);
                CURRENT.store(std::ptr::null_mut(), Ordering::Release);
                drop(Box::from_raw(rt_ptr));
                ((*driver).vtbl().dispose_buffers)(driver);
                ((*driver).vtbl().release)(driver);
                return Err(EngineError::Backend(format!("ASIO start failed: {msg}")));
            }

            Ok((info, rt_ptr, driver))
        })();

        match result {
            Err(e) => {
                let _ = setup_tx.send(Err(e));
                CoUninitialize();
                IN_USE.store(false, Ordering::SeqCst);
                return;
            }
            Ok((info, rt_ptr, driver)) => {
                let _ = setup_tx.send(Ok(info));

                // park until stop; the driver's own thread runs audio
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }

                ((*driver).vtbl().stop)(driver);
                CURRENT.store(std::ptr::null_mut(), Ordering::Release);
                // let any in-flight callback finish
                std::thread::sleep(Duration::from_millis(20));
                ((*driver).vtbl().dispose_buffers)(driver);
                ((*driver).vtbl().release)(driver);
                drop(Box::from_raw(rt_ptr));
                CoUninitialize();
                IN_USE.store(false, Ordering::SeqCst);
            }
        }
    }
}

struct NullProc;
impl Processor for NullProc {
    fn process(&mut self, _i: &[f32], _o: &mut [f32], _c: usize) {}
}

// retarget the driver's hardware sample rate without starting a stream
pub fn set_sample_rate(sample_rate: u32, driver_hint: Option<&str>) -> Result<u32> {
    if IN_USE.swap(true, Ordering::SeqCst) {
        return Err(EngineError::Backend(
            "an ASIO stream is active (cannot change rate)".into(),
        ));
    }

    let hint = driver_hint.map(str::to_string);
    let (tx, rx) = mpsc::channel();

    let handle = std::thread::Builder::new()
        .name("asio-rate".into())
        .spawn(move || {
            let result = unsafe { set_sample_rate_inner(sample_rate, hint.as_deref()) };
            let _ = tx.send(result);
            IN_USE.store(false, Ordering::SeqCst);
        })
        .map_err(|e| EngineError::Backend(format!("spawn asio rate: {e}")))?;

    let result = rx
        .recv()
        .unwrap_or(Err(EngineError::Backend("asio rate thread aborted".into())));
    let _ = handle.join();
    result
}

unsafe fn set_sample_rate_inner(rate: u32, hint: Option<&str>) -> Result<u32> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let result = (|| -> Result<u32> {
        let config = EngineConfig {
            render_device: hint.map(String::from),
            ..Default::default()
        };
        let (_name, clsid) = pick_driver(&config).ok_or(EngineError::NoDevice)?;

        let mut raw: *mut c_void = std::ptr::null_mut();
        CoCreateInstance(&clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &clsid, &mut raw)
            .ok()?;
        let driver = raw as *mut IAsio;

        if ((*driver).vtbl().init)(driver, std::ptr::null_mut()) != ASIO_TRUE {
            let msg = driver_error(driver);
            ((*driver).vtbl().release)(driver);
            return Err(EngineError::Backend(format!("ASIO init failed: {msg}")));
        }

        let want = rate as f64;
        if ((*driver).vtbl().can_sample_rate)(driver, want) != ASE_OK {
            ((*driver).vtbl().release)(driver);
            return Err(EngineError::FormatNotSupported(format!(
                "ASIO driver does not support {rate} Hz"
            )));
        }
        if ((*driver).vtbl().set_sample_rate)(driver, want) != ASE_OK {
            ((*driver).vtbl().release)(driver);
            return Err(EngineError::Backend(format!(
                "ASIO setSampleRate({rate}) failed"
            )));
        }
        let mut actual = 0.0f64;
        ((*driver).vtbl().get_sample_rate)(driver, &mut actual);
        ((*driver).vtbl().release)(driver);
        Ok(actual as u32)
    })();

    CoUninitialize();
    result
}

pub fn start(config: &EngineConfig, processor: Box<dyn Processor>) -> Result<Stream> {
    if IN_USE.swap(true, Ordering::SeqCst) {
        return Err(EngineError::Backend(
            "an ASIO stream is already active (only one allowed)".into(),
        ));
    }

    let stats = Arc::new(StreamStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    let cfg = config.clone();
    let stats_t = stats.clone();
    let stop_t = stop.clone();
    let handle = std::thread::Builder::new()
        .name("asio-control".into())
        .spawn(move || run_control(cfg, processor, stats_t, tx, stop_t))
        .map_err(|e| EngineError::Backend(format!("spawn asio control: {e}")))?;

    match rx.recv() {
        Ok(Ok(info)) => Ok(Stream::new(stop, vec![handle], stats, info)),
        Ok(Err(e)) => {
            let _ = handle.join();
            IN_USE.store(false, Ordering::SeqCst);
            Err(e)
        }
        Err(_) => {
            let _ = handle.join();
            IN_USE.store(false, Ordering::SeqCst);
            Err(EngineError::Backend("asio control thread aborted".into()))
        }
    }
}
