//! Example/benchmark app for the `prism-llae` crate.
//!
//! Subcommands:
//!   list                     enumerate endpoints for the chosen backend
//!   monitor                  pass the microphone through to the speakers live
//!   latency                  measure roundtrip latency (needs a loopback path)
//!   sweep                    benchmark the matrix to CSV (and optionally XLSX)

mod report;
mod sysinfo;

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prism_llae::{
    Backend, EngineConfig, EngineError, Processor, Result, SampleFormat, ShareMode, StreamInfo,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    let opts = Opts::parse(&args);

    let result = match cmd {
        "list" => list(opts),
        "monitor" => monitor(opts),
        "latency" => latency(opts),
        "sweep" => sweep(opts),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "usage: latency_test <list|monitor|latency|sweep> [options]\n\
         \n\
         commands:\n\
         \x20 list       enumerate endpoints for the chosen backend\n\
         \x20 monitor    pass the microphone through to the speakers live\n\
         \x20 latency    measure roundtrip latency (needs a loopback path)\n\
         \x20 sweep      benchmark every backend/mode x rate x format x frames/block size\n\
         \n\
         devices (required for monitor/latency/sweep):\n\
         \x20 --in / --out NAME|ID          input/output device\n\
         \x20 --ks-in / --ks-out NAME|ID    KS input/output pins (for sweep mode)\n\
         \x20 --asio NAME                   ASIO driver name (for ASIO backend and sweep mode)\n\
         \x20 --in-ch / --out-ch N          first ASIO channel to test\n\
         \n\
         options:\n\
         \x20 --backend wasapi|ks|asio backend (default wasapi)\n\
         \x20 --mode shared|exclusive  WASAPI sharing model (default shared)\n\
         \x20 --rate N                 sample rate, 0 = device default\n\
         \x20 --channels N             channel count, 0 = device default\n\
         \x20 --frames N               period in frames, 0 = minimum/preferred\n\
         \x20 --gain G                 monitor passthrough gain (default 1.0)\n\
         \x20 --trials N               latency/sweep trials per run (default 10)\n\
         \x20 --seconds N              auto-stop monitor after N seconds\n\
         \x20 --csv PATH               full log of every run (for sweep mode; default sweep.csv)\n\
         \x20 --report PATH            beautified .xlsx of the ok runs (for sweep mode)"
    );
}

struct Opts {
    backend: Backend,
    mode: ShareMode,
    rate: u32,
    channels: u16,
    frames: u32,
    gain: f32,
    trials: usize,
    seconds: u32,
    in_sel: Option<String>,
    out_sel: Option<String>,
    ks_in: Option<String>,
    ks_out: Option<String>,
    in_ch: u16,
    out_ch: u16,
    csv: String,
    report: Option<String>,
    asio_sel: Option<String>,
}

impl Opts {
    fn parse(args: &[String]) -> Opts {
        let mut o = Opts {
            backend: Backend::Wasapi,
            mode: ShareMode::Shared,
            rate: 0,
            channels: 0,
            frames: 0,
            gain: 1.0,
            trials: 10,
            seconds: 0,
            in_sel: None,
            out_sel: None,
            ks_in: None,
            ks_out: None,
            in_ch: 0,
            out_ch: 0,
            csv: "sweep.csv".to_string(),
            report: None,
            asio_sel: None,
        };
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let mut next = || {
                i += 1;
                args.get(i).cloned().unwrap_or_default()
            };
            match a.as_str() {
                "--backend" => {
                    o.backend = match next().as_str() {
                        "ks" | "wdmks" | "wdm" => Backend::WdmKs,
                        "asio" => Backend::Asio,
                        _ => Backend::Wasapi,
                    }
                }
                "--mode" => {
                    o.mode = match next().as_str() {
                        "exclusive" | "excl" => ShareMode::Exclusive,
                        _ => ShareMode::Shared,
                    }
                }
                "--rate" => o.rate = next().parse().unwrap_or(0),
                "--channels" => o.channels = next().parse().unwrap_or(0),
                "--frames" => o.frames = next().parse().unwrap_or(0),
                "--gain" => o.gain = next().parse().unwrap_or(1.0),
                "--trials" => o.trials = next().parse().unwrap_or(10),
                "--seconds" => o.seconds = next().parse().unwrap_or(0),
                "--in" => o.in_sel = Some(next()),
                "--out" => o.out_sel = Some(next()),
                "--ks-in" => o.ks_in = Some(next()),
                "--ks-out" => o.ks_out = Some(next()),
                "--in-ch" => o.in_ch = next().parse().unwrap_or(0),
                "--out-ch" => o.out_ch = next().parse().unwrap_or(0),
                "--csv" => o.csv = next(),
                "--report" => o.report = Some(next()),
                "--asio" => o.asio_sel = Some(next()),
                _ => {}
            }
            i += 1;
        }
        o
    }

    fn config(&self) -> EngineConfig {
        let (capture_device, render_device) = self.resolve_devices();
        // ASIO is one driver for both in and out
        let render_device = match self.backend {
            Backend::Asio => self.asio_sel.clone().or(render_device),
            _ => render_device,
        };
        EngineConfig {
            backend: self.backend,
            share_mode: self.mode,
            sample_rate: self.rate,
            channels: self.channels,
            buffer_frames: self.frames,
            capture_device,
            render_device,
            input_channel: self.in_ch,
            output_channel: self.out_ch,
            force_format: None,
        }
    }

    fn resolve_devices(&self) -> (Option<String>, Option<String>) {
        if self.in_sel.is_none() && self.out_sel.is_none() {
            return (None, None);
        }
        let devices = prism_llae::list_devices(self.backend).unwrap_or_default();
        let find = |sel: &Option<String>, capture: bool| -> Option<String> {
            let want = sel.as_ref()?.to_lowercase();
            let hit = devices.iter().find(|d| {
                d.is_capture == capture
                    && (d.id.to_lowercase().contains(&want)
                        || d.name.to_lowercase().contains(&want))
            });
            match hit {
                Some(d) => {
                    eprintln!(
                        "selected {} device: {}",
                        if capture { "input" } else { "output" },
                        d.name
                    );
                    Some(d.id.clone())
                }
                None => {
                    eprintln!(
                        "warning: no {} device matching {:?}",
                        if capture { "input" } else { "output" },
                        sel.as_ref().unwrap()
                    );
                    None
                }
            }
        };
        (find(&self.in_sel, true), find(&self.out_sel, false))
    }

    fn require_devices(&self) -> Result<()> {
        match self.backend {
            Backend::Asio => {
                if self.asio_sel.is_none() && self.out_sel.is_none() {
                    return Err(EngineError::Backend(
                        "ASIO needs a driver name: pass --asio <driver> (or --out <driver>)".into(),
                    ));
                }
            }
            _ => {
                if self.in_sel.is_none() || self.out_sel.is_none() {
                    return Err(EngineError::Backend(
                        "specify both --in and --out device names".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Wasapi => "WASAPI",
        Backend::WdmKs => "WDM/KS",
        Backend::Asio => "ASIO",
    }
}

fn list(opts: Opts) -> Result<()> {
    let devices = prism_llae::list_devices(opts.backend)?;
    println!("{} endpoints:", backend_name(opts.backend));
    for d in devices {
        let dir = if d.is_capture { "in " } else { "out" };
        println!("  [{dir}] {}\n        id: {}", d.name, d.id);
    }
    Ok(())
}

fn print_stream_banner(info: &StreamInfo) {
    let mode = match info.share_mode {
        ShareMode::Shared => "shared",
        ShareMode::Exclusive => "exclusive",
    };
    let rate = info.render_format.sample_rate as f32;
    let one_way_ms = info.nominal_buffer_frames() as f32 / rate * 1000.0;
    println!(
        "engine: {} ({mode})\n  capture: {}\n  render : {}\n  period : {} frames (render), {} frames (capture)\n  engine buffering: ~{:.2} ms one-way",
        backend_name(info.backend),
        info.capture_format.describe(),
        info.render_format.describe(),
        info.period_frames,
        info.capture_period_frames,
        one_way_ms,
    );
}

// == Monitor =================================================

struct Monitor {
    gain: f32,
    peak: Arc<AtomicU32>,
}

impl Processor for Monitor {
    fn process(&mut self, input: &[f32], output: &mut [f32], _channels: usize) {
        let mut peak = 0.0f32;
        for (o, i) in output.iter_mut().zip(input.iter()) {
            let s = *i * self.gain;
            *o = s;
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        let prev = f32::from_bits(self.peak.load(Ordering::Relaxed));
        if peak > prev {
            self.peak.store(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

fn monitor(opts: Opts) -> Result<()> {
    opts.require_devices()?;
    let peak = Arc::new(AtomicU32::new(0));
    let proc = Box::new(Monitor {
        gain: opts.gain,
        peak: peak.clone(),
    });

    println!("Starting live monitor");
    println!("WARNING: use headphones to avoid acoustic feedback.\n");

    let stream = prism_llae::start(&opts.config(), proc)?;
    print_stream_banner(stream.info());
    println!("\nMonitoring... press Enter to stop.\n");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_in = stop.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        stop_in.store(true, Ordering::SeqCst);
    });

    let start = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if opts.seconds > 0 && start.elapsed() >= Duration::from_secs(opts.seconds as u64) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        let p = f32::from_bits(peak.swap(0, Ordering::Relaxed));
        let stats = stream.stats().snapshot();
        let db = if p > 0.0 { 20.0 * p.log10() } else { -99.0 };
        let bars = ((p * 40.0) as usize).min(40);
        print!(
            "\r[{:<40}] {:6.1} dBFS  cb:{} under:{} over:{} glitch:{}   ",
            "#".repeat(bars),
            db,
            stats.render_calls,
            stats.underruns,
            stats.overruns,
            stats.glitches
        );
        let _ = std::io::stdout().flush();
    }

    println!("\nstopping...");
    stream.stop();
    Ok(())
}

// == Latency =================================================

#[derive(Default)]
struct LatencyShared {
    results_frames: Mutex<Vec<u32>>,
    misses: AtomicUsize,
    done: AtomicBool,
    noise_floor: AtomicU32,
    threshold: AtomicU32,
}

enum Phase {
    Warmup,
    Arm,
    Listen { out_frame: u64, deadline: u64 },
    Cooldown { until: u64 },
}

struct LatencyProbe {
    shared: Arc<LatencyShared>,
    rate: u32,
    channels: usize,
    frame_pos: u64,
    phase: Phase,
    trials_left: usize,
    warmup_frames: u64,
    timeout_frames: u64,
    cooldown_frames: u64,
    noise_floor: f32,
    threshold: f32,
}

impl LatencyProbe {
    const IMPULSE: f32 = 0.95;

    fn new(shared: Arc<LatencyShared>, trials: usize) -> Self {
        LatencyProbe {
            shared,
            rate: 48_000,
            channels: 2,
            frame_pos: 0,
            phase: Phase::Warmup,
            trials_left: trials,
            warmup_frames: 0,
            timeout_frames: 0,
            cooldown_frames: 0,
            noise_floor: 0.0,
            threshold: 0.1,
        }
    }

    fn scan(&self, input: &[f32]) -> (f32, usize) {
        let mut max = 0.0f32;
        let mut at = 0usize;
        for (idx, &s) in input.iter().enumerate() {
            let a = s.abs();
            if a > max {
                max = a;
                at = idx;
            }
        }
        (max, at / self.channels.max(1))
    }
}

impl Processor for LatencyProbe {
    fn on_start(&mut self, info: &StreamInfo) {
        self.rate = info.render_format.sample_rate;
        self.channels = info.channels as usize;
        self.warmup_frames = self.rate as u64 / 2; // 0.5 s
        self.timeout_frames = self.rate as u64 / 2; // 0.5 s per trial
        self.cooldown_frames = self.rate as u64 / 5; // 0.2 s
    }

    fn process(&mut self, input: &[f32], output: &mut [f32], _channels: usize) {
        for o in output.iter_mut() {
            *o = 0.0;
        }
        let frames = output.len() / self.channels.max(1);
        let (peak, peak_at) = self.scan(input);

        match self.phase {
            Phase::Warmup => {
                if peak > self.noise_floor {
                    self.noise_floor = peak;
                }
                if self.frame_pos >= self.warmup_frames {
                    self.threshold = (self.noise_floor * 8.0).max(0.06);
                    self.shared
                        .noise_floor
                        .store(self.noise_floor.to_bits(), Ordering::Relaxed);
                    self.shared
                        .threshold
                        .store(self.threshold.to_bits(), Ordering::Relaxed);
                    self.phase = Phase::Arm;
                }
            }
            Phase::Arm => {
                for c in 0..self.channels {
                    output[c] = Self::IMPULSE;
                }
                self.phase = Phase::Listen {
                    out_frame: self.frame_pos,
                    deadline: self.frame_pos + self.timeout_frames,
                };
            }
            Phase::Listen { out_frame, deadline } => {
                if peak >= self.threshold {
                    let in_frame = self.frame_pos + peak_at as u64;
                    let latency = in_frame.saturating_sub(out_frame);
                    if let Ok(mut v) = self.shared.results_frames.try_lock() {
                        v.push(latency as u32);
                    }
                    self.trials_left = self.trials_left.saturating_sub(1);
                    self.phase = Phase::Cooldown {
                        until: self.frame_pos + self.cooldown_frames,
                    };
                } else if self.frame_pos >= deadline {
                    self.shared.misses.fetch_add(1, Ordering::Relaxed);
                    self.trials_left = self.trials_left.saturating_sub(1);
                    self.phase = Phase::Cooldown {
                        until: self.frame_pos + self.cooldown_frames,
                    };
                }
            }
            Phase::Cooldown { until } => {
                if self.frame_pos >= until {
                    if self.trials_left == 0 {
                        self.shared.done.store(true, Ordering::SeqCst);
                    } else {
                        self.phase = Phase::Arm;
                    }
                }
            }
        }

        self.frame_pos += frames as u64;
    }
}

struct LatencyOutcome {
    info: StreamInfo,
    hits: Vec<u32>,
    misses: usize,
    delivery: f64,
    underruns: u64,
    overruns: u64,
    glitches: u64,
    noise: f32,
    threshold: f32,
}

fn run_latency(config: &EngineConfig, trials: usize) -> Result<LatencyOutcome> {
    let shared = Arc::new(LatencyShared::default());
    let trials = trials.max(1);
    let proc = Box::new(LatencyProbe::new(shared.clone(), trials));

    let stream = prism_llae::start(config, proc)?;
    let info = stream.info().clone();

    let start = Instant::now();
    let budget = Duration::from_secs_f32(2.0 + trials as f32 * 0.8);
    while !shared.done.load(Ordering::SeqCst) {
        if start.elapsed() > budget {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let stats = stream.stats().snapshot();
    stream.stop();

    let hits = shared.results_frames.lock().unwrap().clone();
    let delivery = if stats.render_calls > 0 {
        stats.render_calls.saturating_sub(stats.underruns) as f64 / stats.render_calls as f64
    } else {
        0.0
    };
    Ok(LatencyOutcome {
        info,
        hits,
        misses: shared.misses.load(Ordering::Relaxed),
        delivery,
        underruns: stats.underruns,
        overruns: stats.overruns,
        glitches: stats.glitches,
        noise: f32::from_bits(shared.noise_floor.load(Ordering::Relaxed)),
        threshold: f32::from_bits(shared.threshold.load(Ordering::Relaxed)),
    })
}

fn hit_stats(frames: &[u32]) -> Option<(u32, u32, f64, u32)> {
    if frames.is_empty() {
        return None;
    }
    let mut v = frames.to_vec();
    v.sort_unstable();
    let n = v.len();
    let mean = v.iter().map(|&f| f as f64).sum::<f64>() / n as f64;
    Some((v[0], v[n / 2], mean, v[n - 1]))
}

fn latency(opts: Opts) -> Result<()> {
    opts.require_devices()?;
    let trials = opts.trials.max(1);
    println!("Roundtrip latency measurement");
    println!(
        "Route output to input first: a loopback cable (out->in) for an electrical\n\
         measurement, or place the speaker next to the mic for an acoustic one.\n"
    );

    let config = opts.config();
    let out = run_latency(&config, trials)?;
    print_stream_banner(&out.info);
    let rate = out.info.render_format.sample_rate as f32;

    println!(
        "input noise floor: {:.4} ({:.1} dBFS), detect threshold: {:.4}",
        out.noise,
        if out.noise > 0.0 { 20.0 * out.noise.log10() } else { -99.0 },
        out.threshold
    );
    println!(
        "engine activity: {:.0}% delivered, under:{} over:{} glitch:{}",
        out.delivery * 100.0,
        out.underruns,
        out.overruns,
        out.glitches
    );

    let stats_opt = hit_stats(&out.hits);
    if stats_opt.is_none() {
        if out.delivery < 0.85 {
            println!(
                "\nThe input is only delivering {:.0}% of the frames the output consumes, so\n\
                 the captured stream is mostly silence and no impulse can be detected.\n\
                 For KS this is a capture-pin limitation on some devices; otherwise check routing.",
                out.delivery * 100.0
            );
        } else {
            println!(
                "\nNo impulses detected ({} misses), but both directions were streaming at full\n\
                 rate. The output is most likely not routed back to the input. Provide a loopback\n\
                 path (cable or the interface's loopback input) via --out / --in / --out-ch.",
                out.misses
            );
        }
        return Ok(());
    }

    let (min, median, mean, max) = stats_opt.unwrap();
    let to_ms = |f: f32| f / rate * 1000.0;
    println!(
        "\nroundtrip latency over {} hits ({} misses):",
        out.hits.len(),
        out.misses
    );
    println!("  min    : {:>6} frames  {:>7.2} ms", min, to_ms(min as f32));
    println!(
        "  median : {:>6} frames  {:>7.2} ms",
        median,
        to_ms(median as f32)
    );
    println!("  mean   : {:>9.1} frames  {:>7.2} ms", mean, to_ms(mean as f32));
    println!("  max    : {:>6} frames  {:>7.2} ms", max, to_ms(max as f32));
    Ok(())
}

// == Sweep ===================================================

fn fmt_label(sf: SampleFormat, valid: u16) -> String {
    let kind = match sf {
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::I32 => "i32",
        SampleFormat::F32 => "f32",
    };
    let bits = match sf {
        SampleFormat::I16 => 16,
        SampleFormat::I24 => 24,
        SampleFormat::I32 => 32,
        SampleFormat::F32 => 32,
    };
    if valid != bits {
        format!("{kind}/{valid}")
    } else {
        kind.to_string()
    }
}

fn fmt_from_str(s: &str) -> Option<SampleFormat> {
    match s {
        "i16" => Some(SampleFormat::I16),
        "i24" => Some(SampleFormat::I24),
        "i32" => Some(SampleFormat::I32),
        "f32" => Some(SampleFormat::F32),
        _ => None, // "auto"
    }
}

fn resolve_device(backend: Backend, name: &str, capture: bool) -> Option<String> {
    let want = name.to_lowercase();
    prism_llae::list_devices(backend)
        .ok()?
        .into_iter()
        .find(|d| d.is_capture == capture && d.name.to_lowercase().contains(&want))
        .map(|d| d.id)
}

struct Record {
    backend: &'static str,
    mode: &'static str,
    req_rate: u32,
    req_format: String,
    req_block: u32,
    status: &'static str,
    act_rate: u32,
    act_format: String,
    act_block: u32,
    channels: u16,
    trials: usize,
    hits: usize,
    misses: usize,
    delivery: f64,
    underruns: u64,
    overruns: u64,
    glitches: u64,
    min_f: u32,
    median_f: u32,
    mean_f: f64,
    max_f: u32,
    min_ms: f64,
    median_ms: f64,
    mean_ms: f64,
    max_ms: f64,
    jitter: u32,
    detail: String,
}

struct Job {
    backend: Backend,
    mode: &'static str,
    req_rate: u32,
    req_format: &'static str,
    req_block: u32,
    config: EngineConfig,
}

fn record_job(
    idx: usize,
    total: usize,
    job: &Job,
    trials: usize,
    out: &mut Vec<Record>,
    seen: &mut std::collections::HashSet<String>,
) {
    eprint!(
        "[{idx:>3}/{total}] {}/{} {}Hz {} blk{} ... ",
        backend_name(job.backend),
        job.mode,
        job.req_rate,
        job.req_format,
        job.req_block
    );
    match run_latency(&job.config, trials) {
        Ok(res) => {
            let info = &res.info;
            let act_rate = info.render_format.sample_rate;
            let act_fmt = fmt_label(info.render_format.sample_format, info.render_format.valid_bits);
            let act_block = info.period_frames;

            // Collapse runs that landed on the same actual config
            let key = format!(
                "{}|{}|{act_rate}|{act_fmt}|{act_block}",
                backend_name(job.backend),
                job.mode
            );
            if !seen.insert(key) {
                eprintln!("dup (skip)");
                return;
            }

            let (min, median, mean, max) = hit_stats(&res.hits).unwrap_or((0, 0, 0.0, 0));
            let to_ms = |f: f64| f / act_rate.max(1) as f64 * 1000.0;
            let status = if res.hits.is_empty() { "no-detect" } else { "ok" };
            out.push(Record {
                backend: backend_name(job.backend),
                mode: job.mode,
                req_rate: job.req_rate,
                req_format: job.req_format.to_string(),
                req_block: job.req_block,
                status,
                act_rate,
                act_format: act_fmt.clone(),
                act_block,
                channels: info.channels,
                trials,
                hits: res.hits.len(),
                misses: res.misses,
                delivery: res.delivery * 100.0,
                underruns: res.underruns,
                overruns: res.overruns,
                glitches: res.glitches,
                min_f: min,
                median_f: median,
                mean_f: mean,
                max_f: max,
                min_ms: to_ms(min as f64),
                median_ms: to_ms(median as f64),
                mean_ms: to_ms(mean),
                max_ms: to_ms(max as f64),
                jitter: max.saturating_sub(min),
                detail: String::new(),
            });
            eprintln!(
                "{status} {act_rate}Hz {act_fmt} blk{act_block} -> {} ({:.2} ms median)",
                res.hits.len(),
                to_ms(median as f64)
            );
        }
        Err(e) => {
            let detail = e.to_string().replace([',', '\n'], " ");
            out.push(Record {
                backend: backend_name(job.backend),
                mode: job.mode,
                req_rate: job.req_rate,
                req_format: job.req_format.to_string(),
                req_block: job.req_block,
                status: "error",
                act_rate: 0,
                act_format: String::new(),
                act_block: 0,
                channels: job.config.channels,
                trials,
                hits: 0,
                misses: 0,
                delivery: 0.0,
                underruns: 0,
                overruns: 0,
                glitches: 0,
                min_f: 0,
                median_f: 0,
                mean_f: 0.0,
                max_f: 0,
                min_ms: 0.0,
                median_ms: 0.0,
                mean_ms: 0.0,
                max_ms: 0.0,
                jitter: 0,
                detail,
            });
            eprintln!("unsupported");
        }
    }
}

const CSV_HEADER: &str =
    "backend,mode,req_rate,req_format,req_block,status,act_rate,act_format,act_period_frames,\
     channels,trials,hits,misses,delivery_pct,underruns,overruns,glitches,min_frames,median_frames,\
     mean_frames,max_frames,min_ms,median_ms,mean_ms,max_ms,jitter_frames,detail";

fn csv_line(r: &Record) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{},{}",
        r.backend, r.mode, r.req_rate, r.req_format, r.req_block, r.status, r.act_rate,
        r.act_format, r.act_block, r.channels, r.trials, r.hits, r.misses, r.delivery,
        r.underruns, r.overruns, r.glitches, r.min_f, r.median_f, r.mean_f, r.max_f, r.min_ms,
        r.median_ms, r.mean_ms, r.max_ms, r.jitter, r.detail,
    )
}

/// Benchmark the requested backends across sample rates, formats, and block
/// sizes, always stereo over a digital loopback. A backend is included only if
/// its device flags are present; rates are stepped by the ASIO driver.
fn sweep(opts: Opts) -> Result<()> {
    let trials = opts.trials.max(1);
    let channels = 2u16;

    let wasapi_out = opts.out_sel.clone();
    let wasapi_in = opts.in_sel.clone();
    let ks_out = opts.ks_out.clone();
    let ks_in = opts.ks_in.clone();
    let asio_driver = opts.asio_sel.clone();
    let asio_ch = opts.out_ch;

    let do_wasapi = wasapi_out.is_some() && wasapi_in.is_some();
    let do_ks = ks_out.is_some() && ks_in.is_some();
    let do_asio = asio_driver.is_some();
    if !do_wasapi && !do_ks && !do_asio {
        return Err(EngineError::Backend(
            "nothing to sweep: pass --out/--in (WASAPI), --ks-out/--ks-in (KS), and/or --asio".into(),
        ));
    }

    // Rates: a single --rate, else the full set (which needs ASIO to retune)
    let full = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
    let rates: Vec<u32> = if opts.rate != 0 {
        vec![opts.rate]
    } else if do_asio {
        full.to_vec()
    } else {
        return Err(EngineError::Backend(
            "multi-rate sweep needs --asio to change the device rate; or pass --rate for one rate"
                .into(),
        ));
    };

    eprintln!(
        "sweep: WASAPI {} | KS {} | ASIO {}",
        if do_wasapi { "on" } else { "off" },
        if do_ks { "on" } else { "off" },
        if do_asio { "on" } else { "off" },
    );

    // Blocks/frames sizes to be test per backend
    let shared_blocks = [0u32];
    let excl_blocks = [0u32, 128, 192, 256, 384, 512, 768, 1024];
    let ks_blocks = [16u32, 32, 64, 128, 256, 512, 1024, 2048];
    let asio_blocks = [8u32, 16, 32, 64, 128, 256, 512, 1024, 2048];

    let per_rate = (if do_wasapi { shared_blocks.len() + 4 * excl_blocks.len() } else { 0 })
        + (if do_ks { 3 * ks_blocks.len() } else { 0 })
        + (if do_asio { asio_blocks.len() } else { 0 });
    let total = rates.len() * per_rate;
    eprintln!("sweeping up to {total} configurations, {trials} trials each...\n");

    let mut records: Vec<Record> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut idx = 0usize;

    for &rate in &rates {
        if let Some(ref driver) = asio_driver {
            eprint!("--- setting {rate} Hz via ASIO ... ");
            match prism_llae::asio::set_sample_rate(rate, Some(driver)) {
                Ok(act) => eprintln!("ok ({act} Hz)"),
                Err(e) => {
                    eprintln!("failed ({e}); skipping group");
                    continue;
                }
            }
            // let the driver settle
            std::thread::sleep(Duration::from_millis(400));
        }

        // Resolve WASAPI ids by name
        let (w_out, w_in) = if do_wasapi {
            let o = resolve_device(Backend::Wasapi, wasapi_out.as_ref().unwrap(), false);
            let i = resolve_device(Backend::Wasapi, wasapi_in.as_ref().unwrap(), true);
            if o.is_none() || i.is_none() {
                eprintln!(
                    "warning: WASAPI loopback not found at {rate} Hz (out={} in={}); skipping WASAPI",
                    o.is_some(),
                    i.is_some()
                );
            } else if let Some(ref out_id) = o {
                match prism_llae::wasapi::endpoint_rate(true, Some(out_id)) {
                    Ok(d) if d == rate => eprintln!("WASAPI mix rate confirmed at {d} Hz"),
                    Ok(d) => eprintln!("note: WASAPI reports {d} Hz (expected {rate})"),
                    Err(e) => eprintln!("could not read WASAPI mix rate: {e}"),
                }
            }
            (o, i)
        } else {
            (None, None)
        };

        if w_out.is_some() && w_in.is_some() {
            let base_wasapi = |share: ShareMode, req_rate: u32, fmt: &str, block: u32| EngineConfig {
                backend: Backend::Wasapi,
                share_mode: share,
                sample_rate: req_rate,
                channels,
                buffer_frames: block,
                capture_device: w_in.clone(),
                render_device: w_out.clone(),
                force_format: fmt_from_str(fmt),
                ..Default::default()
            };

            for &block in &shared_blocks {
                idx += 1;
                let job = Job {
                    backend: Backend::Wasapi,
                    mode: "shared",
                    req_rate: rate,
                    req_format: "auto",
                    req_block: block,
                    config: base_wasapi(ShareMode::Shared, 0, "auto", block),
                };
                record_job(idx, total, &job, trials, &mut records, &mut seen);
            }
            for fmt in ["i16", "i24", "i32", "f32"] {
                for &block in &excl_blocks {
                    idx += 1;
                    let job = Job {
                        backend: Backend::Wasapi,
                        mode: "exclusive",
                        req_rate: rate,
                        req_format: fmt,
                        req_block: block,
                        config: base_wasapi(ShareMode::Exclusive, rate, fmt, block),
                    };
                    record_job(idx, total, &job, trials, &mut records, &mut seen);
                }
            }
        }

        if do_ks {
            for fmt in ["i16", "i24", "i32"] {
                for &block in &ks_blocks {
                    idx += 1;
                    let job = Job {
                        backend: Backend::WdmKs,
                        mode: "-",
                        req_rate: rate,
                        req_format: fmt,
                        req_block: block,
                        config: EngineConfig {
                            backend: Backend::WdmKs,
                            sample_rate: rate,
                            channels,
                            buffer_frames: block,
                            capture_device: ks_in.clone(),
                            render_device: ks_out.clone(),
                            force_format: fmt_from_str(fmt),
                            ..Default::default()
                        },
                    };
                    record_job(idx, total, &job, trials, &mut records, &mut seen);
                }
            }
        }

        if let Some(ref driver) = asio_driver {
            for &block in &asio_blocks {
                idx += 1;
                let job = Job {
                    backend: Backend::Asio,
                    mode: "-",
                    req_rate: rate,
                    req_format: "auto",
                    req_block: block,
                    config: EngineConfig {
                        backend: Backend::Asio,
                        sample_rate: rate,
                        channels,
                        buffer_frames: block,
                        input_channel: asio_ch,
                        output_channel: asio_ch,
                        render_device: Some(driver.clone()),
                        ..Default::default()
                    },
                };
                record_job(idx, total, &job, trials, &mut records, &mut seen);
            }
        }
    }

    // Full log -> CSV
    let mut csv = String::from(CSV_HEADER);
    csv.push_str("\r\n");
    for r in &records {
        csv.push_str(&csv_line(r));
        csv.push_str("\r\n");
    }
    std::fs::write(&opts.csv, csv)
        .map_err(|e| EngineError::Backend(format!("write {}: {e}", opts.csv)))?;
    eprintln!("\nwrote {} rows to {}", records.len(), opts.csv);

    if let Some(ref path) = opts.report {
        let device = asio_driver
            .clone()
            .or_else(|| wasapi_out.clone())
            .or_else(|| ks_out.clone())
            .unwrap_or_else(|| "unknown".into());
        let rows: Vec<report::Row> = records
            .iter()
            .filter(|r| r.status == "ok")
            .map(|r| report::Row {
                rate: r.act_rate,
                backend: r.backend.to_string(),
                mode: r.mode.to_string(),
                req_format: r.req_format.clone(),
                act_format: r.act_format.clone(),
                req_block: r.req_block,
                act_block: r.act_block,
                min_f: r.min_f,
                median_f: r.median_f,
                mean_f: r.mean_f,
                max_f: r.max_f,
                min_ms: r.min_ms,
                median_ms: r.median_ms,
                mean_ms: r.mean_ms,
                max_ms: r.max_ms,
                jitter: r.jitter,
            })
            .collect();
        let notes = sysinfo::tested_on(&device);
        report::write(path, &rows, &notes)
            .map_err(|e| EngineError::Backend(format!("write {path}: {e}")))?;
        eprintln!("wrote {} ok rows to {}", rows.len(), path);
    }

    Ok(())
}
