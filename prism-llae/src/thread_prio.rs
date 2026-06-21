//! MMCSS ("Pro Audio") thread registration. Failure is non-fatal.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
    AVRT_PRIORITY_HIGH,
};

// registers MMCSS for the guard's lifetime, reverting on drop
pub struct ProAudioGuard {
    handle: HANDLE,
}

impl ProAudioGuard {
    pub fn enter() -> Option<ProAudioGuard> {
        let mut task_index: u32 = 0;
        let handle = unsafe {
            AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task_index)
        };
        match handle {
            Ok(h) if !h.is_invalid() => {
                unsafe {
                    let _ = AvSetMmThreadPriority(h, AVRT_PRIORITY_HIGH);
                }
                Some(ProAudioGuard { handle: h })
            }
            _ => None,
        }
    }
}

impl Drop for ProAudioGuard {
    fn drop(&mut self) {
        // SAFETY: handle came from AvSetMmThreadCharacteristicsW on this thread.
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}
