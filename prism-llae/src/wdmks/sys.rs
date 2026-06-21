//! Raw KS FFI structs/helpers not exposed constructibly by windows-rs.

use std::ffi::c_void;

use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::IO::DeviceIoControl;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;

// KSPROPERTY — base property request (no pin id)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KsProperty {
    pub set: GUID,
    pub id: u32,
    pub flags: u32,
}

// KSP_PIN — pin-scoped property request
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KsPPin {
    pub set: GUID,
    pub id: u32,
    pub flags: u32,
    pub pin_id: u32,
    pub reserved: u32,
}

// KSIDENTIFIER (interface/medium)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KsIdentifier {
    pub set: GUID,
    pub id: u32,
    pub flags: u32,
}

// KSPIN_CONNECT flattened (no nested unions)
#[repr(C)]
pub struct KsPinConnect {
    pub interface: KsIdentifier,
    pub medium: KsIdentifier,
    pub pin_id: u32,
    pub pin_to_handle: *mut c_void,
    pub priority_class: u32,
    pub priority_sub_class: u32,
}

// KSDATAFORMAT + contiguous WAVEFORMATEX
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KsDataFormatWfx {
    pub format_size: u32,
    pub flags: u32,
    pub sample_size: u32,
    pub reserved: u32,
    pub major_format: GUID,
    pub sub_format: GUID,
    pub specifier: GUID,
    pub wfx: WAVEFORMATEX,
}

// KSDATARANGE_AUDIO overlay for read-only parsing
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KsDataRangeAudio {
    pub format_size: u32,
    pub flags: u32,
    pub sample_size: u32,
    pub reserved: u32,
    pub major_format: GUID,
    pub sub_format: GUID,
    pub specifier: GUID,
    pub max_channels: u32,
    pub min_bits_per_sample: u32,
    pub max_bits_per_sample: u32,
    pub min_sample_frequency: u32,
    pub max_sample_frequency: u32,
}

pub const KSPROPERTY_TYPE_GET: u32 = 1;
pub const KSPROPERTY_TYPE_SET: u32 = 2;

pub const GUID_NULL: GUID = GUID::from_u128(0); // KS wildcard

// SAFETY (all helpers): `handle` must be a valid KS control handle.
// fixed-size KS GET, returns the value
pub unsafe fn ks_get_property<P: Copy, V: Copy + Default>(
    handle: HANDLE,
    prop: &P,
) -> windows::core::Result<V> {
    let mut value = V::default();
    let mut returned = 0u32;
    DeviceIoControl(
        handle,
        crate::wdmks::IOCTL_KS_PROPERTY,
        Some(prop as *const P as *const c_void),
        std::mem::size_of::<P>() as u32,
        Some(&mut value as *mut V as *mut c_void),
        std::mem::size_of::<V>() as u32,
        Some(&mut returned),
        None,
    )?;
    Ok(value)
}

// variable-size KS GET into `buf`, returns bytes written
pub unsafe fn ks_get_property_var<P: Copy>(
    handle: HANDLE,
    prop: &P,
    buf: &mut [u8],
) -> windows::core::Result<u32> {
    let mut returned = 0u32;
    DeviceIoControl(
        handle,
        crate::wdmks::IOCTL_KS_PROPERTY,
        Some(prop as *const P as *const c_void),
        std::mem::size_of::<P>() as u32,
        Some(buf.as_mut_ptr() as *mut c_void),
        buf.len() as u32,
        Some(&mut returned),
        None,
    )?;
    Ok(returned)
}

// required size of a variable KS property (NULL buffer reports it)
pub unsafe fn ks_get_property_size<P: Copy>(
    handle: HANDLE,
    prop: &P,
) -> windows::core::Result<u32> {
    let mut returned = 0u32;
    let _ = DeviceIoControl(
        handle,
        crate::wdmks::IOCTL_KS_PROPERTY,
        Some(prop as *const P as *const c_void),
        std::mem::size_of::<P>() as u32,
        None,
        0,
        Some(&mut returned),
        None,
    );
    Ok(returned)
}

// fixed-size KS SET
pub unsafe fn ks_set_property<P: Copy, V: Copy>(
    handle: HANDLE,
    prop: &P,
    value: &V,
) -> windows::core::Result<()> {
    let mut returned = 0u32;
    DeviceIoControl(
        handle,
        crate::wdmks::IOCTL_KS_PROPERTY,
        Some(prop as *const P as *const c_void),
        std::mem::size_of::<P>() as u32,
        Some(value as *const V as *mut c_void),
        std::mem::size_of::<V>() as u32,
        Some(&mut returned),
        None,
    )
}
