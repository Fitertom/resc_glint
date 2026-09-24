//! COM by hand: windows-sys has the functions but no interfaces, so the few methods used are
//! called through their vtable slots. Slot numbers come from the SDK headers.

use std::ffi::c_void;

pub struct Com(pub *mut c_void);

impl Com {
    /// Method `i` of the object's vtable, as the function type `F`.
    ///
    /// # Safety
    /// `F` must be the signature the header gives slot `i` of this interface.
    pub unsafe fn slot<F: Copy>(&self, i: usize) -> F {
        // SAFETY: a live COM object's first field is its vtable.
        unsafe {
            let vt = *(self.0 as *const *const *const c_void);
            std::mem::transmute_copy(&*vt.add(i))
        }
    }

    pub fn raw(&self) -> *mut c_void {
        self.0
    }

    /// Another interface of the same object (`IUnknown::QueryInterface`).
    pub fn query(&self, iid: &windows_sys::core::GUID) -> Option<Com> {
        let mut out = std::ptr::null_mut();
        // SAFETY: slot 0 of every interface.
        let hr = unsafe { self.slot::<unsafe extern "system" fn(*mut c_void, *const windows_sys::core::GUID, Out) -> i32>(0)(self.0, iid, &mut out) };
        (hr >= 0 && !out.is_null()).then_some(Com(out))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: IUnknown::Release is slot 2 of every interface.
            unsafe { self.slot::<unsafe extern "system" fn(*mut c_void) -> u32>(2)(self.0) };
        }
    }
}

pub fn check(hr: i32, what: &str) -> Result<(), String> {
    if hr < 0 { Err(format!("{what}: 0x{:08X}", hr as u32)) } else { Ok(()) }
}

pub type Out = *mut *mut c_void;
