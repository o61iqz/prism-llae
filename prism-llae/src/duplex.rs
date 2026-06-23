//! Shared full-duplex plumbing: capture bytes -> ring -> processor -> render bytes.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::format::{f32_to_native, native_to_f32, remap_channels, StreamFormat};
use crate::ring::{ring, Consumer, Producer};
use crate::{Processor, StreamStats};

// capture-thread end: device bytes -> ring
pub struct CaptureSink {
    prod: Producer,
    cap_fmt: StreamFormat,
    proc_ch: usize,
    f32_buf: Vec<f32>,
    remap_buf: Vec<f32>,
    stats: Arc<StreamStats>,
}

impl CaptureSink {
    pub fn submit(&mut self, native: &[u8], frames: usize, discontinuity: bool) {
        let cap_ch = self.cap_fmt.channels as usize;
        debug_assert_eq!(native.len(), frames * self.cap_fmt.frame_bytes());

        self.f32_buf.clear();
        native_to_f32(native, self.cap_fmt.sample_format, &mut self.f32_buf);

        let pushed_src;
        if cap_ch == self.proc_ch {
            pushed_src = self.prod.push(&self.f32_buf);
        } else {
            self.remap_buf.clear();
            remap_channels(&self.f32_buf, cap_ch, &mut self.remap_buf, self.proc_ch);
            pushed_src = self.prod.push(&self.remap_buf);
        }
        let wanted = frames * self.proc_ch;
        if pushed_src < wanted {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
        if discontinuity {
            self.stats.glitches.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.capture_calls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .capture_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

}

// render-thread end: ring -> processor -> device bytes
pub struct RenderSource {
    cons: Consumer,
    proc: Box<dyn Processor>,
    render_fmt: StreamFormat,
    proc_ch: usize,
    in_buf: Vec<f32>,
    out_buf: Vec<f32>,
    stats: Arc<StreamStats>,
}

impl RenderSource {
    pub fn fill(&mut self, dst: &mut [u8], frames: usize) {
        let n = frames * self.proc_ch;
        if self.in_buf.len() != n {
            self.in_buf.resize(n, 0.0);
            self.out_buf.resize(n, 0.0);
        }

        let got = self.cons.pop(&mut self.in_buf);
        if got < n {
            for s in &mut self.in_buf[got..] {
                *s = 0.0;
            }
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }

        for s in &mut self.out_buf {
            *s = 0.0;
        }
        self.proc.process(&self.in_buf, &mut self.out_buf, self.proc_ch);

        f32_to_native(&self.out_buf, self.render_fmt.sample_format, dst);
        self.stats.render_calls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .render_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub fn on_start(&mut self, info: &crate::StreamInfo) {
        self.proc.on_start(info);
    }
}

// `prime_frames` of silence priming the ring is real latency — keep it minimal
pub fn make_duplex(
    cap_fmt: StreamFormat,
    render_fmt: StreamFormat,
    proc_ch: usize,
    ring_frames: usize,
    prime_frames: usize,
    proc: Box<dyn Processor>,
    stats: Arc<StreamStats>,
) -> (CaptureSink, RenderSource) {
    let (prod, cons) = ring(ring_frames * proc_ch);
    if prime_frames > 0 {
        let silence = vec![0.0f32; prime_frames * proc_ch];
        prod.push(&silence);
    }
    (
        CaptureSink {
            prod,
            cap_fmt,
            proc_ch,
            f32_buf: Vec::with_capacity(ring_frames * proc_ch),
            remap_buf: Vec::with_capacity(ring_frames * proc_ch),
            stats: stats.clone(),
        },
        RenderSource {
            cons,
            proc,
            render_fmt,
            proc_ch,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            stats,
        },
    )
}
