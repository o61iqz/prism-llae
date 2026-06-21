//! `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` parsing and construction.

use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;

use crate::format::{SampleFormat, StreamFormat};

pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// SPEAKER_FRONT_* mask for common channel counts
pub fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,             // FRONT_CENTER
        2 => 0x1 | 0x2,       // FRONT_LEFT | FRONT_RIGHT
        4 => 0x1 | 0x2 | 0x10 | 0x20,
        6 => 0x3F,
        8 => 0xFF,
        _ => 0,
    }
}

// SAFETY: `ptr` must be a valid WAVEFORMATEX (+ WAVEFORMATEXTENSIBLE if extensible)
pub unsafe fn parse(ptr: *const WAVEFORMATEX) -> Option<StreamFormat> {
    let wfx = &*ptr;
    let container_bytes = (wfx.wBitsPerSample / 8) as usize;
    let (is_float, valid_container, valid_bits) = if wfx.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
        let ext = &*(ptr as *const WAVEFORMATEXTENSIBLE);
        let sub = ext.SubFormat; // copy out of the packed struct before comparing
        let float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        let pcm = sub == KSDATAFORMAT_SUBTYPE_PCM;
        if !float && !pcm {
            return None;
        }
        let valid = ext.Samples.wValidBitsPerSample;
        (float, container_bytes, valid)
    } else {
        match wfx.wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT => (true, container_bytes, wfx.wBitsPerSample),
            WAVE_FORMAT_PCM => (false, container_bytes, wfx.wBitsPerSample),
            _ => return None,
        }
    };

    let sample_format = match (is_float, valid_container) {
        (true, 4) => SampleFormat::F32,
        (false, 2) => SampleFormat::I16,
        (false, 3) => SampleFormat::I24,
        (false, 4) => SampleFormat::I32,
        _ => return None,
    };

    // Clamp valid bits to the container; 0 (some drivers) means "all".
    let container_bits = (container_bytes * 8) as u16;
    let valid_bits = if valid_bits == 0 || valid_bits > container_bits {
        container_bits
    } else {
        valid_bits
    };

    Some(StreamFormat {
        sample_rate: wfx.nSamplesPerSec,
        channels: wfx.nChannels,
        sample_format,
        valid_bits,
    })
}

// build WAVEFORMATEXTENSIBLE with explicit valid-bits (e.g. 24-in-32)
pub fn build_extensible_ex(sf: StreamFormat, valid_bits: u16) -> WAVEFORMATEXTENSIBLE {
    let bytes = sf.sample_format.bytes() as u16;
    let block_align = bytes * sf.channels;
    let sub = if sf.sample_format.is_float() {
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        KSDATAFORMAT_SUBTYPE_PCM
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: sf.channels,
            nSamplesPerSec: sf.sample_rate,
            nAvgBytesPerSec: sf.sample_rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: sf.sample_format.bits(),
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: channel_mask(sf.channels),
        SubFormat: sub,
    }
}
