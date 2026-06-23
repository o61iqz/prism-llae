//! WASAPI backend: shared mode (`IAudioClient3`) and event-driven exclusive mode.

use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, EDataFlow,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::duplex::{make_duplex, CaptureSink, RenderSource};
use crate::error::{EngineError, Result};
use crate::format::{SampleFormat, StreamFormat};
use crate::thread_prio::ProAudioGuard;
use crate::{wfx, DeviceInfo, EngineConfig, Processor, ShareMode, Stream, StreamInfo, StreamStats};

// init COM (MTA) on the current thread; S_FALSE is fine
fn co_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    co_init();
    let e: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    Ok(e)
}

fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let pw = device.GetId()?;
        let s = pw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pw.0 as *const _));
        Ok(s)
    }
}

fn device_name(device: &IMMDevice) -> String {
    unsafe {
        if let Ok(store) = device.OpenPropertyStore(STGM_READ) {
            if let Ok(val) = store.GetValue(&PKEY_Device_FriendlyName) {
                return format!("{val}");
            }
        }
    }
    "<unknown>".to_string()
}

fn pick_device(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    id: Option<&str>,
) -> Result<IMMDevice> {
    unsafe {
        match id {
            None => Ok(enumerator.GetDefaultAudioEndpoint(flow, eConsole)?),
            Some(want) => {
                let coll = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
                let count = coll.GetCount()?;
                for i in 0..count {
                    let dev = coll.Item(i)?;
                    if device_id(&dev)? == want {
                        return Ok(dev);
                    }
                }
                Err(EngineError::NoDevice)
            }
        }
    }
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let en = enumerator()?;
    let mut out = Vec::new();
    unsafe {
        for (flow, is_capture) in [(eRender, false), (eCapture, true)] {
            let coll = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            for i in 0..coll.GetCount()? {
                let dev = coll.Item(i)?;
                out.push(DeviceInfo {
                    id: device_id(&dev)?,
                    name: device_name(&dev),
                    is_capture,
                });
            }
        }
    }
    Ok(out)
}

#[inline]
fn frames_to_hns(frames: u32, rate: u32) -> i64 {
    ((frames as i128 * 10_000_000 + (rate as i128 / 2)) / rate as i128) as i64
}

struct Endpoint {
    client: IAudioClient,
    event: HANDLE,
    format: StreamFormat,
    buffer_frames: u32,
    exclusive: bool,
}

fn make_event() -> Result<HANDLE> {
    let h = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    Ok(h)
}

fn activate(device: &IMMDevice) -> Result<IAudioClient> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    Ok(client)
}

// shared-mode endpoint at the minimum (or requested) engine period
fn setup_shared(device: &IMMDevice, requested_period: u32) -> Result<Endpoint> {
    unsafe {
        let client = activate(device)?;
        let mix = client.GetMixFormat()?;
        let format = wfx::parse(mix as *const WAVEFORMATEX)
            .ok_or_else(|| EngineError::FormatNotSupported("mix format".into()))?;

        let client3: IAudioClient3 = client.cast()?;
        let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        client3.GetSharedModeEnginePeriod(mix, &mut def, &mut fund, &mut min, &mut max)?;

        let period = if requested_period == 0 {
            min
        } else {
            // round to a valid multiple of the fundamental period
            let f = fund.max(1);
            let rounded = ((requested_period + f / 2) / f) * f;
            rounded.clamp(min, max)
        };

        let event = make_event()?;
        client3.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            period,
            mix,
            None,
        )?;
        client.SetEventHandle(event)?;
        let buffer_frames = client.GetBufferSize()?;
        CoTaskMemFree(Some(mix as *const _));

        Ok(Endpoint {
            client,
            event,
            format,
            buffer_frames,
            exclusive: false,
        })
    }
}

// find an exclusive-mode format the endpoint accepts at `rate`/`channels`
fn negotiate_exclusive(
    client: &IAudioClient,
    rate: u32,
    pref_ch: u16,
    mix_ch: u16,
    force: Option<SampleFormat>,
) -> Option<(StreamFormat, u16)> {
    let chans = if pref_ch != 0 && pref_ch != mix_ch {
        vec![pref_ch, mix_ch]
    } else {
        vec![mix_ch]
    };
    // (sample_format, valid_bits), highest fidelity first; 24-in-32 streamed as I32
    let all = [
        (SampleFormat::I24, 24u16),
        (SampleFormat::I32, 24),
        (SampleFormat::I32, 32),
        (SampleFormat::I16, 16),
        (SampleFormat::F32, 32),
    ];
    let candidates: Vec<(SampleFormat, u16)> = match force {
        Some(f) => all.into_iter().filter(|(sf, _)| *sf == f).collect(),
        None => all.to_vec(),
    };
    for ch in chans {
        for (sf, valid) in candidates.iter().copied() {
            let candidate = StreamFormat {
                sample_rate: rate,
                channels: ch,
                sample_format: sf,
                valid_bits: valid,
            };
            let ext = wfx::build_extensible_ex(candidate, valid);
            let hr = unsafe {
                client.IsFormatSupported(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    std::ptr::addr_of!(ext.Format),
                    None,
                )
            };
            if hr.is_ok() {
                return Some((candidate, valid));
            }
        }
    }
    None
}

// exclusive event-driven endpoint at the device minimum (or requested) period
fn setup_exclusive(
    device: &IMMDevice,
    config: &EngineConfig,
    mix_rate: u32,
    mix_ch: u16,
) -> Result<Endpoint> {
    unsafe {
        let client = activate(device)?;
        let rate = if config.sample_rate != 0 {
            config.sample_rate
        } else {
            mix_rate
        };
        let (format, valid_bits) =
            negotiate_exclusive(&client, rate, config.channels, mix_ch, config.force_format)
                .ok_or_else(|| {
                    EngineError::FormatNotSupported(format!("no exclusive PCM format at {rate} Hz"))
                })?;

        let (mut def_p, mut min_p) = (0i64, 0i64);
        client.GetDevicePeriod(Some(&mut def_p), Some(&mut min_p))?;
        let mut period_hns = if config.buffer_frames != 0 {
            frames_to_hns(config.buffer_frames, rate).max(min_p)
        } else {
            min_p
        };

        let ext = wfx::build_extensible_ex(format, valid_bits);
        let event = make_event()?;

        let mut hr = client.Initialize(
            AUDCLNT_SHAREMODE_EXCLUSIVE,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            period_hns,
            period_hns,
            std::ptr::addr_of!(ext.Format),
            None,
        );

        // retry once with the device-aligned buffer size
        if let Err(e) = &hr {
            if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                let aligned = client.GetBufferSize().unwrap_or(0);
                if aligned > 0 {
                    period_hns = frames_to_hns(aligned, rate);
                    client.Reset()?;
                    hr = client.Initialize(
                        AUDCLNT_SHAREMODE_EXCLUSIVE,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        period_hns,
                        period_hns,
                        std::ptr::addr_of!(ext.Format),
                        None,
                    );
                }
            }
        }
        hr?;

        client.SetEventHandle(event)?;
        let buffer_frames = client.GetBufferSize()?;
        Ok(Endpoint {
            client,
            event,
            format,
            buffer_frames,
            exclusive: true,
        })
    }
}

// == realtime thread contexts (COM confined to each thread) ==

struct CaptureCtx {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    sink: CaptureSink,
    frame_bytes: usize,
    silent: Vec<u8>,
    first_packet: bool,
}
// SAFETY: each context is used only by the thread it is moved into (COM MTA)
unsafe impl Send for CaptureCtx {}

struct RenderCtx {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    source: RenderSource,
    frame_bytes: usize,
    buffer_frames: u32,
    exclusive: bool,
    info: StreamInfo,
}
unsafe impl Send for RenderCtx {}

fn capture_loop(mut ctx: CaptureCtx, stop: Arc<AtomicBool>) {
    co_init();
    let _mmcss = ProAudioGuard::enter();
    let mut run = || -> Result<()> {
        unsafe {
            ctx.client.Start()?;
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(ctx.event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                loop {
                    let packet = ctx.capture.GetNextPacketSize()?;
                    if packet == 0 {
                        break;
                    }
                    let mut pdata: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    ctx.capture
                        .GetBuffer(&mut pdata, &mut frames, &mut flags, None, None)?;
                    let nbytes = frames as usize * ctx.frame_bytes;
                    let disc = flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0 && !ctx.first_packet;
                    ctx.first_packet = false;
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || pdata.is_null() {
                        if ctx.silent.len() < nbytes {
                            ctx.silent.resize(nbytes, 0);
                        }
                        ctx.sink.submit(&ctx.silent[..nbytes], frames as usize, disc);
                    } else {
                        let slice = slice::from_raw_parts(pdata, nbytes);
                        ctx.sink.submit(slice, frames as usize, disc);
                    }
                    ctx.capture.ReleaseBuffer(frames)?;
                }
            }
            ctx.client.Stop()?;
            let _ = ctx.client.Reset();
        }
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("[prism-llae] capture thread error: {e}");
    }
}

fn render_loop(mut ctx: RenderCtx, stop: Arc<AtomicBool>) {
    co_init();
    let _mmcss = ProAudioGuard::enter();
    ctx.source.on_start(&ctx.info);
    let mut run = || -> Result<()> {
        unsafe {
            // pre-roll one silent buffer
            let _ = ctx.render.GetBuffer(ctx.buffer_frames)?;
            ctx.render
                .ReleaseBuffer(ctx.buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;

            ctx.client.Start()?;
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(ctx.event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                let frames = if ctx.exclusive {
                    ctx.buffer_frames
                } else {
                    ctx.buffer_frames - ctx.client.GetCurrentPadding()?
                };
                if frames == 0 {
                    continue;
                }
                let ptr = ctx.render.GetBuffer(frames)?;
                let nbytes = frames as usize * ctx.frame_bytes;
                let dst = slice::from_raw_parts_mut(ptr, nbytes);
                ctx.source.fill(dst, frames as usize);
                ctx.render.ReleaseBuffer(frames, 0)?;
            }
            ctx.client.Stop()?;
            let _ = ctx.client.Reset();
        }
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("[prism-llae] render thread error: {e}");
    }
}

pub fn start(config: &EngineConfig, processor: Box<dyn Processor>) -> Result<Stream> {
    let en = enumerator()?;
    let render_dev = pick_device(&en, eRender, config.render_device.as_deref())?;
    let capture_dev = pick_device(&en, eCapture, config.capture_device.as_deref())?;

    let render = match config.share_mode {
        ShareMode::Shared => setup_shared(&render_dev, config.buffer_frames)?,
        ShareMode::Exclusive => {
            let (rate, ch) = mix_rate_channels(&render_dev)?;
            setup_exclusive(&render_dev, config, rate, ch)?
        }
    };
    let capture = match config.share_mode {
        ShareMode::Shared => setup_shared(&capture_dev, config.buffer_frames)?,
        ShareMode::Exclusive => {
            let (rate, ch) = mix_rate_channels(&capture_dev)?;
            setup_exclusive(&capture_dev, config, rate, ch)?
        }
    };

    if capture.format.sample_rate != render.format.sample_rate {
        eprintln!(
            "[prism-llae] warning: capture {} Hz != render {} Hz; duplex will drift",
            capture.format.sample_rate, render.format.sample_rate
        );
    }

    let proc_ch = render.format.channels as usize;
    let info = StreamInfo {
        backend: crate::Backend::Wasapi,
        share_mode: config.share_mode,
        capture_format: capture.format,
        render_format: render.format,
        channels: render.format.channels,
        period_frames: render.buffer_frames,
        capture_period_frames: capture.buffer_frames,
    };

    let stats = Arc::new(StreamStats::default());
    // a few periods of ring slack absorb capture/render jitter
    let ring_frames =
        (render.buffer_frames.max(capture.buffer_frames) as usize * 4).next_power_of_two();
    let prime_frames = render.buffer_frames as usize;
    let (sink, source) = make_duplex(
        capture.format,
        render.format,
        proc_ch,
        ring_frames,
        prime_frames,
        processor,
        stats.clone(),
    );

    let capture_ctx = CaptureCtx {
        capture: unsafe { capture.client.GetService::<IAudioCaptureClient>()? },
        frame_bytes: capture.format.frame_bytes(),
        silent: vec![0u8; capture.buffer_frames as usize * capture.format.frame_bytes()],
        client: capture.client,
        event: capture.event,
        sink,
        first_packet: true,
    };
    let render_ctx = RenderCtx {
        render: unsafe { render.client.GetService::<IAudioRenderClient>()? },
        frame_bytes: render.format.frame_bytes(),
        buffer_frames: render.buffer_frames,
        exclusive: render.exclusive,
        client: render.client,
        event: render.event,
        source,
        info: info.clone(),
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let stop_r = stop.clone();
    let t_cap = std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || capture_loop(capture_ctx, stop_c))
        .map_err(|e| EngineError::Backend(format!("spawn capture: {e}")))?;
    let t_ren = std::thread::Builder::new()
        .name("audio-render".into())
        .spawn(move || render_loop(render_ctx, stop_r))
        .map_err(|e| EngineError::Backend(format!("spawn render: {e}")))?;

    Ok(Stream::new(stop, vec![t_cap, t_ren], stats, info))
}

// current shared-mode mix rate of an endpoint (the device's configured rate)
pub fn endpoint_rate(render: bool, device: Option<&str>) -> Result<u32> {
    let en = enumerator()?;
    let flow = if render { eRender } else { eCapture };
    let dev = pick_device(&en, flow, device)?;
    Ok(mix_rate_channels(&dev)?.0)
}

fn mix_rate_channels(device: &IMMDevice) -> Result<(u32, u16)> {
    unsafe {
        let client = activate(device)?;
        let mix = client.GetMixFormat()?;
        let f = wfx::parse(mix as *const WAVEFORMATEX)
            .ok_or_else(|| EngineError::FormatNotSupported("mix format".into()))?;
        CoTaskMemFree(Some(mix as *const _));
        Ok((f.sample_rate, f.channels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, EngineConfig, Processor, ShareMode};

    struct Silence;
    impl Processor for Silence {
        fn process(&mut self, _: &[f32], o: &mut [f32], _: usize) {
            o.fill(0.0);
        }
    }

    fn resolve(name: &str, capture: bool) -> Option<String> {
        list_devices()
            .ok()?
            .into_iter()
            .find(|d| d.is_capture == capture && d.name.to_lowercase().contains(&name.to_lowercase()))
            .map(|d| d.id)
    }

    // after ASIO retargets the hardware rate, exclusive should open at that rate
    #[test]
    fn exclusive_follows_asio_rate_change() {
        for rate in [96_000u32, 44_100] {
            crate::asio::set_sample_rate(rate, Some("MiniFuse")).unwrap_or_else(|e| {
                panic!("asio set_sample_rate({rate}): {e}");
            });
            std::thread::sleep(std::time::Duration::from_millis(500));

            let out = resolve("Virtual 7/8", false).expect("render device after rate change");
            let inp = resolve("Loopback Main 7/8", true).expect("capture device after rate change");

            let mix = endpoint_rate(true, Some(&out)).unwrap();
            eprintln!("after ASIO {rate}: endpoint_rate={mix}");

            let wasapi = EngineConfig {
                backend: Backend::Wasapi,
                share_mode: ShareMode::Exclusive,
                sample_rate: rate,
                channels: 2,
                render_device: Some(out),
                capture_device: Some(inp),
                ..Default::default()
            };
            match crate::start(&wasapi, Box::new(Silence)) {
                Ok(stream) => {
                    let act = stream.info().render_format.sample_rate;
                    stream.stop();
                    eprintln!("WASAPI exclusive req {rate} act {act}");
                    assert_eq!(act, rate, "WASAPI should match ASIO-set rate");
                }
                Err(e) => panic!("WASAPI exclusive at {rate} after ASIO set: {e}"),
            }
        }
    }
}
