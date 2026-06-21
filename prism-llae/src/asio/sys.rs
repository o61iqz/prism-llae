//! Raw ASIO (Steinberg) FFI: the `IASIO` vtable and structs. x64 only; the IID
//! equals the CLSID, so we describe the vtable by hand and call through it.

use std::ffi::c_void;

use windows::core::{GUID, HRESULT};

pub const ASE_OK: i32 = 0; // most methods return 0 on success
pub const ASIO_TRUE: i32 = 1; // init() returns an ASIOBool

// ASIOSampleType values (little-endian Windows variants).
pub const ASIOST_INT16_LSB: i32 = 16;
pub const ASIOST_INT24_LSB: i32 = 17;
pub const ASIOST_INT32_LSB: i32 = 18;
pub const ASIOST_FLOAT32_LSB: i32 = 19;
pub const ASIOST_INT32_LSB16: i32 = 24;
pub const ASIOST_INT32_LSB18: i32 = 25;
pub const ASIOST_INT32_LSB20: i32 = 26;
pub const ASIOST_INT32_LSB24: i32 = 27;

// asioMessage selectors.
pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
pub const K_ASIO_RESET_REQUEST: i32 = 3;
pub const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
pub const K_ASIO_RESYNC_REQUEST: i32 = 5;
pub const K_ASIO_LATENCIES_CHANGED: i32 = 6;
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;

pub const CLSCTX_INPROC_SERVER: u32 = 1;

// 64-bit split integer (ASIOSamples / ASIOTimeStamp)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AsioInt64 {
    pub hi: u32,
    pub lo: u32,
}

#[repr(C)]
pub struct AsioBufferInfo {
    pub is_input: i32,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2], // filled by createBuffers (ping-pong)
}

#[repr(C)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: i32,
    pub is_active: i32,
    pub channel_group: i32,
    pub sample_type: i32,
    pub name: [u8; 32],
}

impl Default for AsioChannelInfo {
    fn default() -> Self {
        unsafe { std::mem::zeroed() } // all-zero is valid
    }
}

#[repr(C)]
pub struct AsioCallbacks {
    pub buffer_switch: extern "system" fn(double_buffer_index: i32, direct_process: i32),
    pub sample_rate_did_change: extern "system" fn(s_rate: f64),
    pub asio_message:
        extern "system" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    pub buffer_switch_time_info:
        extern "system" fn(params: *mut c_void, double_buffer_index: i32, direct_process: i32)
            -> *mut c_void,
}

#[repr(C)]
pub struct IAsio {
    pub vtbl: *const IAsioVtbl,
}

// IASIO vtable; first three entries are IUnknown
#[repr(C)]
pub struct IAsioVtbl {
    pub query_interface:
        unsafe extern "system" fn(*mut IAsio, *const GUID, *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(*mut IAsio) -> u32,
    pub release: unsafe extern "system" fn(*mut IAsio) -> u32,

    pub init: unsafe extern "system" fn(*mut IAsio, sys_handle: *mut c_void) -> i32,
    pub get_driver_name: unsafe extern "system" fn(*mut IAsio, name: *mut u8),
    pub get_driver_version: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub get_error_message: unsafe extern "system" fn(*mut IAsio, string: *mut u8),
    pub start: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub stop: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub get_channels:
        unsafe extern "system" fn(*mut IAsio, num_in: *mut i32, num_out: *mut i32) -> i32,
    pub get_latencies:
        unsafe extern "system" fn(*mut IAsio, in_lat: *mut i32, out_lat: *mut i32) -> i32,
    pub get_buffer_size: unsafe extern "system" fn(
        *mut IAsio,
        min: *mut i32,
        max: *mut i32,
        preferred: *mut i32,
        granularity: *mut i32,
    ) -> i32,
    pub can_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: f64) -> i32,
    pub get_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: *mut f64) -> i32,
    pub set_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: f64) -> i32,
    pub get_clock_sources:
        unsafe extern "system" fn(*mut IAsio, clocks: *mut c_void, num: *mut i32) -> i32,
    pub set_clock_source: unsafe extern "system" fn(*mut IAsio, reference: i32) -> i32,
    pub get_sample_position:
        unsafe extern "system" fn(*mut IAsio, pos: *mut AsioInt64, ts: *mut AsioInt64) -> i32,
    pub get_channel_info: unsafe extern "system" fn(*mut IAsio, info: *mut AsioChannelInfo) -> i32,
    pub create_buffers: unsafe extern "system" fn(
        *mut IAsio,
        infos: *mut AsioBufferInfo,
        num_channels: i32,
        buffer_size: i32,
        callbacks: *const AsioCallbacks,
    ) -> i32,
    pub dispose_buffers: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub control_panel: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub future: unsafe extern "system" fn(*mut IAsio, selector: i32, opt: *mut c_void) -> i32,
    pub output_ready: unsafe extern "system" fn(*mut IAsio) -> i32,
}

impl IAsio {
    #[inline]
    pub unsafe fn vtbl(&self) -> &IAsioVtbl {
        &*self.vtbl
    }
}

#[link(name = "ole32")]
extern "system" {
    // raw CoCreateInstance — ASIO uses its CLSID as the IID
    pub fn CoCreateInstance(
        rclsid: *const GUID,
        punkouter: *mut c_void,
        dwclscontext: u32,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT;
}
