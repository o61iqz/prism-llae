# prism-llae - a low-latency audio engine for Windows

A small Rust audio engine focused on **low latency** and **stability**, with four
backends and an example CLI for live monitoring and roundtrip latency tests.

```
prism/
├─ prism-llae/      library crate
└─ latency-test/    example CLI app
```

## Backends

- **WASAPI shared:** event-driven, `IAudioClient3` minimum period.
- **WASAPI exclusive:** event-driven, device minimum period, 16/24/32-bit PCM.
- **WDM/KS:** raw kernel-streaming pins, overlapped `IOCTL_KS_{READ,WRITE}_STREAM`.
- **ASIO:** native full-duplex; the `Processor` runs straight in `bufferSwitch`.

All backends drive a synchronous full-duplex loop. The user supplies a `Processor`:

```rust
pub trait Processor: Send {
    fn process(&mut self, input: &[f32], output: &mut [f32], channels: usize);
    fn on_start(&mut self, info: &StreamInfo) {}
}
```

Each cycle consumes as many input frames as it produces, so input and output share
one frame timeline, which makes the latency measurement honest.

## Build

```
cargo build --release
```

Requires the MSVC toolchain (`x86_64-pc-windows-msvc`).

## Usage

Devices are never guessed: every streaming command needs `--in`/`--out` (or
`--asio <driver>`). Run `latency_test help` for all flags.

```
# Example: enumerate endpoints
latency_test list --backend wasapi

# Example: live monitoring (USE HEADPHONES to avoid feedback)
# prints a live meter plus running cb/under/over/glitch counters
latency_test monitor --backend wasapi --in "Mic" --out "Speakers"

# Example: roundtrip latency (needs a loopback path)
latency_test latency --backend wasapi --mode exclusive --out "Virtual 7/8" --in "Loopback Main 7/8"
latency_test latency --backend ks --frames 32 --out pcm_out_01_v_02 --in pcm_in_01_v_02
latency_test latency --backend asio --asio MiniFuse --out-ch 7 --in-ch 7
```

The latency probe emits a single sample click and counts frames until it reappears
on the input, so you must route output back to input via a loopback cable, the
interface's digital loopback, or speaker next to mic. The reported figure is the
*total* round trip (output buffering + DAC + path + ADC + input buffering).

> The probe outputs **full scale clicks**. Start with the volume low.

## Benchmark sweep

`latency_test sweep` benchmarks the whole matrix: backend × rate (44.1–192 kHz) ×
format × block size (8…2048) Then writes the full log to CSV (`--csv`).
Unsupported combinations are recorded as `error` rows, so the CSV doubles as a
capability matrix.

Backends are opt-in by their device flags: WASAPI needs `--out`/`--in`, KS needs
`--ks-out`/`--ks-in`, ASIO needs `--asio`. A multi-rate sweep requires `--asio`
(only the ASIO driver can retune the interface on-the-fly); otherwise pass a single `--rate`.

Add `--report <file>.xlsx` for a beautified spreadsheet of the successful runs
(trimmed columns, merged config cells, colour scales, and an auto-filled
system information block). The CSV stays the complete log.

```
# Example: full sweep with CSV and XLSX report outputs
latency_test sweep --asio MiniFuse --out "Virtual 7/8" --in "Loopback Main 7/8" \
             --ks-out pcm_out_01_v_02 --ks-in pcm_in_01_v_02 --out-ch 7 \
             --csv sweep.csv --report "report.xlsx"
```

## Results (Arturia MiniFuse 2, digital loopback)

| Backend / mode   | Best config      | Roundtrip  |
|------------------|------------------|------------|
| WASAPI shared    | 48k f32          | 56.8 ms    |
| WASAPI exclusive | 48k i32/24, 144f | 9.48 ms    |
| WDM/KS           | 48k i24, 32f     | 4.81 ms    |
| ASIO             | 48k i32, 8f      | 1.65 ms    |

Figures are deterministic to the sample; all run glitch free (0 underruns, 0
glitches, 100% delivery). Each backend exposes only a subset of supported
rates/formats.

## Notes

- Capture and render must run at the **same sample rate** (true for one interface).
- **KS pins are exclusive.** If one is held by the Windows mixer, `KsCreatePin`
  fails; pick a free endpoint. The two pins must form a real loopback path.
- ASIO drivers are read from `HKLM\SOFTWARE\ASIO`; one driver is the whole
  full-duplex device. `--out-ch`/`--in-ch` pick channels (`AUDIO_DEBUG=1` prints
  the channel map). Only one ASIO stream may be active at a time.
- **Stream health** is exposed via `Stream::stats()` and printed by `monitor`
  and `latency`: `underruns` (render starved the ring), `overruns` (capture
  outran it), and `glitches` (a capture discontinuity — flagged from the WASAPI
  discontinuity flag, or inferred from packet timestamps / a wall-clock drift
  check on KS, which has no native flag). These are ring-based counters; ASIO
  runs the `Processor` straight in `bufferSwitch` with no ring, so it reports 0.
- `latency` prints `N% delivered`: the fraction of render periods that pulled a
  full buffer of real audio, computed exactly as `(render_calls − underruns) /
  render_calls`. Anything under 100% means the input side isn't sustaining full
  rate. (Render-only streams report 0%, since nothing is captured.)
- **Output-only mode.** Capture is optional: set `EngineConfig.output_only` (or just
  let a missing default capture device fall back gracefully) to open a render-only
  stream. No capture endpoint is created, the `Processor` receives silence as its
  input, and `StreamInfo.capture_format` is `None` (with `capture_period_frames` 0).
  This lets players, synths, machines without a microphone, or any playback-only
  applications open a stream. A *named* capture device that fails to open is still an error.

## License

MIT License

Copyright (c) 2026 o61iqz.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.