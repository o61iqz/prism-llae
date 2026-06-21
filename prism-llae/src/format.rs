//! Native PCM sample formats and conversion to/from interleaved `f32` [-1, 1].

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    I16,
    I24, // packed 3 bytes, little-endian
    I32,
    F32,
}

impl SampleFormat {
    pub const fn bytes(self) -> usize {
        match self {
            SampleFormat::I16 => 2,
            SampleFormat::I24 => 3,
            SampleFormat::I32 => 4,
            SampleFormat::F32 => 4,
        }
    }

    pub const fn bits(self) -> u16 {
        match self {
            SampleFormat::I16 => 16,
            SampleFormat::I24 => 24,
            SampleFormat::I32 => 32,
            SampleFormat::F32 => 32,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, SampleFormat::F32)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
    pub valid_bits: u16, // may be < container, e.g. 24-in-32
}

impl StreamFormat {
    pub const fn new(sample_rate: u32, channels: u16, sample_format: SampleFormat) -> Self {
        StreamFormat {
            sample_rate,
            channels,
            sample_format,
            valid_bits: sample_format.bits(),
        }
    }

    pub const fn frame_bytes(self) -> usize {
        self.channels as usize * self.sample_format.bytes()
    }

    pub fn describe(self) -> String {
        let kind = match self.sample_format {
            SampleFormat::F32 => "f32",
            SampleFormat::I16 => "i16",
            SampleFormat::I24 => "i24",
            SampleFormat::I32 => "i32",
        };
        if self.valid_bits != self.sample_format.bits() {
            format!(
                "{} Hz, {} ch, {}/{}",
                self.sample_rate, self.channels, kind, self.valid_bits
            )
        } else {
            format!("{} Hz, {} ch, {}", self.sample_rate, self.channels, kind)
        }
    }
}

#[inline]
fn i24_to_f32(b: [u8; 3]) -> f32 {
    let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
    let signed = (raw << 8) >> 8; // sign-extend
    signed as f32 / 8_388_608.0 // 2^23
}

#[inline]
fn f32_to_i24(v: f32) -> [u8; 3] {
    let clamped = v.clamp(-1.0, 1.0);
    let scaled = (clamped * 8_388_607.0) as i32;
    [scaled as u8, (scaled >> 8) as u8, (scaled >> 16) as u8]
}

// native samples -> interleaved f32, appended into `out`
pub fn native_to_f32(src: &[u8], fmt: SampleFormat, out: &mut Vec<f32>) {
    match fmt {
        SampleFormat::F32 => {
            for c in src.chunks_exact(4) {
                out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        SampleFormat::I16 => {
            for c in src.chunks_exact(2) {
                let s = i16::from_le_bytes([c[0], c[1]]);
                out.push(s as f32 / 32_768.0);
            }
        }
        SampleFormat::I24 => {
            for c in src.chunks_exact(3) {
                out.push(i24_to_f32([c[0], c[1], c[2]]));
            }
        }
        SampleFormat::I32 => {
            for c in src.chunks_exact(4) {
                let s = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                out.push(s as f32 / 2_147_483_648.0);
            }
        }
    }
}

// interleaved f32 -> native bytes written into `dst`
pub fn f32_to_native(src: &[f32], fmt: SampleFormat, dst: &mut [u8]) {
    match fmt {
        SampleFormat::F32 => {
            for (s, d) in src.iter().zip(dst.chunks_exact_mut(4)) {
                d.copy_from_slice(&s.to_le_bytes());
            }
        }
        SampleFormat::I16 => {
            for (s, d) in src.iter().zip(dst.chunks_exact_mut(2)) {
                let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
                d.copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::I24 => {
            for (s, d) in src.iter().zip(dst.chunks_exact_mut(3)) {
                d.copy_from_slice(&f32_to_i24(*s));
            }
        }
        SampleFormat::I32 => {
            for (s, d) in src.iter().zip(dst.chunks_exact_mut(4)) {
                let v = (s.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32;
                d.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

// remap channels: mono->N duplicates, N->mono averages, else copy + zero-fill
pub fn remap_channels(src: &[f32], src_ch: usize, dst: &mut Vec<f32>, dst_ch: usize) {
    if src_ch == dst_ch {
        dst.extend_from_slice(src);
        return;
    }
    let frames = src.len() / src_ch.max(1);
    for f in 0..frames {
        let base = f * src_ch;
        if src_ch == 1 {
            let v = src[base];
            for _ in 0..dst_ch {
                dst.push(v);
            }
        } else if dst_ch == 1 {
            let mut acc = 0.0f32;
            for c in 0..src_ch {
                acc += src[base + c];
            }
            dst.push(acc / src_ch as f32);
        } else {
            for c in 0..dst_ch {
                dst.push(if c < src_ch { src[base + c] } else { 0.0 });
            }
        }
    }
}
