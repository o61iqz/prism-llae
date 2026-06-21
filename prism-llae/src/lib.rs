//! prism-llae — a low-latency full-duplex Windows audio engine.
//! Backends: WASAPI (shared/exclusive), WDM/KS, ASIO. Capture flows through a
//! lock-free ring into the realtime render thread, where a `Processor` maps
//! input frames to output frames on a shared frame timeline.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub mod error;
pub mod format;
pub mod ring;

mod duplex;
mod thread_prio;
mod wfx;
pub mod asio;
pub mod wasapi;
pub mod wdmks;

pub use error::{EngineError, Result};
pub use format::{SampleFormat, StreamFormat};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Wasapi,
    WdmKs,
    Asio,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareMode {
    Shared,
    Exclusive,
}

// zero fields mean "device default / minimum"
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub backend: Backend,
    pub share_mode: ShareMode,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub capture_device: Option<String>,
    pub render_device: Option<String>,
    pub input_channel: u16,  // ASIO only
    pub output_channel: u16, // ASIO only
    pub force_format: Option<SampleFormat>, // WASAPI exclusive + KS only
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            backend: Backend::Wasapi,
            share_mode: ShareMode::Shared,
            sample_rate: 0,
            channels: 0,
            buffer_frames: 0,
            capture_device: None,
            render_device: None,
            input_channel: 0,
            output_channel: 0,
            force_format: None,
        }
    }
}

// realtime callback; runs on the render thread (must not block/allocate/lock)
pub trait Processor: Send {
    fn process(&mut self, input: &[f32], output: &mut [f32], channels: usize);
    fn on_start(&mut self, _info: &StreamInfo) {}
}

// negotiated stream parameters, reported after start
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub backend: Backend,
    pub share_mode: ShareMode,
    pub capture_format: StreamFormat,
    pub render_format: StreamFormat,
    pub channels: u16,
    pub period_frames: u32,
    pub capture_period_frames: u32,
}

impl StreamInfo {
    // one-way engine buffering in frames (excludes converter/DAC/ADC delay)
    pub fn nominal_buffer_frames(&self) -> u32 {
        self.period_frames + self.capture_period_frames
    }
}

#[derive(Default)]
pub struct StreamStats {
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub render_calls: AtomicU64,
    pub capture_calls: AtomicU64,
    pub capture_frames: AtomicU64,
    pub render_frames: AtomicU64,
}

impl StreamStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            render_calls: self.render_calls.load(Ordering::Relaxed),
            capture_calls: self.capture_calls.load(Ordering::Relaxed),
            capture_frames: self.capture_frames.load(Ordering::Relaxed),
            render_frames: self.render_frames.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatsSnapshot {
    pub underruns: u64,
    pub overruns: u64,
    pub render_calls: u64,
    pub capture_calls: u64,
    pub capture_frames: u64,
    pub render_frames: u64,
}

// dropping stops and joins the audio threads
pub struct Stream {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    stats: Arc<StreamStats>,
    info: StreamInfo,
}

impl Stream {
    pub(crate) fn new(
        stop: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
        stats: Arc<StreamStats>,
        info: StreamInfo,
    ) -> Self {
        Stream {
            stop,
            threads,
            stats,
            info,
        }
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    pub fn stats(&self) -> &StreamStats {
        &self.stats
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.shutdown();
        }
    }
}

pub fn start(config: &EngineConfig, processor: Box<dyn Processor>) -> Result<Stream> {
    match config.backend {
        Backend::Wasapi => wasapi::start(config, processor),
        Backend::WdmKs => wdmks::start(config, processor),
        Backend::Asio => asio::start(config, processor),
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_capture: bool,
}

pub fn list_devices(backend: Backend) -> Result<Vec<DeviceInfo>> {
    match backend {
        Backend::Wasapi => wasapi::list_devices(),
        Backend::WdmKs => wdmks::list_devices(),
        Backend::Asio => asio::list_devices(),
    }
}
