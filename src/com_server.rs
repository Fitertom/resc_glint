//! Opening a file from Explorer without starting a process: the resident viewer serves the
//! "open" verb itself, as a COM local server (`DelegateExecute`).
//!
//! **Why.** With a plain command line, a double click makes Explorer start `glint.exe`, which
//! only finds the resident copy and hands it the path: a whole process start for a message.
//! With `DelegateExecute`, Explorer asks COM for our class; the running viewer has registered
//! it, so the call lands straight in its message loop.
//!
//! The objects are written by hand, as `com.rs` calls interfaces: a class factory, and a
//! command object with IExecuteCommand and IObjectWithSelection (the two Explorer uses).

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::core::GUID;

/// Our command's class. Written into the registry by `assoc::set_default`.
pub const CLSID: GUID = GUID::from_u128(0x5a2f9c3e_7b41_4d8e_9c6a_1e2b3c4d5f60);
pub const CLSID_STR: &str = "{5A2F9C3E-7B41-4D8E-9C6A-1E2B3C4D5F60}";

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
const IID_IEXECUTE_COMMAND: GUID = GUID::from_u128(0x7f9185b0_cb92_43c5_80a9_92277a4f7b54);
const IID_IOBJECT_WITH_SELECTION: GUID = GUID::from_u128(0x1c9cd5bb_98e9_4491_a60f_31aacc72b83c);

const S_OK: i32 = 0;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110u32 as i32;
const SIGDN_FILESYSPATH: i32 = 0x8005_8000u32 as i32;

type Hr = i32;
type This = *mut c_void;

fn eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// ------------------------------------------------------------------------------------------
// The command object: IExecuteCommand at offset 0, IObjectWithSelection at offset 8.

#[repr(C)]
struct ExecVtbl {
    query: unsafe extern "system" fn(This, *const GUID, *mut This) -> Hr,
    add_ref: unsafe extern "system" fn(This) -> u32,
    release: unsafe extern "system" fn(This) -> u32,
    set_key_state: unsafe extern "system" fn(This, u32) -> Hr,
    set_parameters: unsafe extern "system" fn(This, *const u16) -> Hr,
    set_position: unsafe extern "system" fn(This, Point) -> Hr,
    set_show_window: unsafe extern "system" fn(This, i32) -> Hr,
    set_no_show_ui: unsafe extern "system" fn(This, i32) -> Hr,
    set_directory: unsafe extern "system" fn(This, *const u16) -> Hr,
    execute: unsafe extern "system" fn(This) -> Hr,
}

#[repr(C)]
struct SelVtbl {
    query: unsafe extern "system" fn(This, *const GUID, *mut This) -> Hr,
    add_ref: unsafe extern "system" fn(This) -> u32,
    release: unsafe extern "system" fn(This) -> u32,
    set_selection: unsafe extern "system" fn(This, This) -> Hr,
    get_selection: unsafe extern "system" fn(This, *const GUID, *mut This) -> Hr,
}

#[repr(C)]
struct Command {
    exec: &'static ExecVtbl,
    sel: &'static SelVtbl,
    refs: AtomicU32,
    path: std::cell::RefCell<Option<PathBuf>>,
}

static EXEC_VTBL: ExecVtbl = ExecVtbl {
    query: cmd_query,
    add_ref: cmd_add_ref,
    release: cmd_release,
    set_key_state: ignore_u32,
    set_parameters: ignore_ptr,
    set_position: ignore_point,
    set_show_window: ignore_i32,
    set_no_show_ui: ignore_i32,
    set_directory: ignore_ptr,
    execute: cmd_execute,
};

static SEL_VTBL: SelVtbl = SelVtbl { query: sel_query, add_ref: sel_add_ref, release: sel_release, set_selection: sel_set, get_selection: sel_get };

/// The object from either of its interface pointers.
unsafe fn from_exec(this: This) -> &'static Command {
    // SAFETY: `this` points at `Command::exec`, the first field.
    unsafe { &*(this as *const Command) }
}
unsafe fn from_sel(this: This) -> &'static Command {
    // SAFETY: `this` points at `Command::sel`, one pointer into the object.
    unsafe { &*((this as *const u8).sub(size_of::<usize>()) as *const Command) }
}

unsafe fn command_query(c: &Command, iid: *const GUID, out: *mut This) -> Hr {
    // SAFETY: COM's contract: `iid` and `out` are valid.
    unsafe {
        if out.is_null() {
            return E_POINTER;
        }
        let iid = &*iid;
        let base = c as *const Command as *mut u8;
        let p = if eq(iid, &IID_IUNKNOWN) || eq(iid, &IID_IEXECUTE_COMMAND) {
            base
        } else if eq(iid, &IID_IOBJECT_WITH_SELECTION) {
            base.add(size_of::<usize>())
        } else {
            *out = std::ptr::null_mut();
            return E_NOINTERFACE;
        };
        c.refs.fetch_add(1, Ordering::Relaxed);
        *out = p.cast();
        S_OK
    }
}

unsafe fn command_release(c: &Command) -> u32 {
    let n = c.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        // SAFETY: made by `Box::into_raw` in `factory_create`; this was the last reference.
        drop(unsafe { Box::from_raw(c as *const Command as *mut Command) });
    }
    n
}

unsafe extern "system" fn cmd_query(this: This, iid: *const GUID, out: *mut This) -> Hr {
    unsafe { command_query(from_exec(this), iid, out) }
}
unsafe extern "system" fn cmd_add_ref(this: This) -> u32 {
    unsafe { from_exec(this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}
unsafe extern "system" fn cmd_release(this: This) -> u32 {
    unsafe { command_release(from_exec(this)) }
}
unsafe extern "system" fn sel_query(this: This, iid: *const GUID, out: *mut This) -> Hr {
    unsafe { command_query(from_sel(this), iid, out) }
}
unsafe extern "system" fn sel_add_ref(this: This) -> u32 {
    unsafe { from_sel(this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}
unsafe extern "system" fn sel_release(this: This) -> u32 {
    unsafe { command_release(from_sel(this)) }
}
unsafe extern "system" fn ignore_u32(_: This, _: u32) -> Hr {
    S_OK
}
unsafe extern "system" fn ignore_i32(_: This, _: i32) -> Hr {
    S_OK
}
unsafe extern "system" fn ignore_ptr(_: This, _: *const u16) -> Hr {
    S_OK
}
/// POINT, passed by value as the header has it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

unsafe extern "system" fn ignore_point(_: This, _: Point) -> Hr {
    S_OK
}
unsafe extern "system" fn sel_get(_: This, _: *const GUID, out: *mut This) -> Hr {
    if !out.is_null() {
        // SAFETY: COM's contract.
        unsafe { *out = std::ptr::null_mut() };
    }
    E_NOINTERFACE
}

/// Explorer hands over what was double-clicked: the first item's file path is read at once
/// (IShellItemArray::GetItemAt is slot 8, IShellItem::GetDisplayName slot 5).
unsafe extern "system" fn sel_set(this: This, array: This) -> Hr {
    // SAFETY: `array` is a live IShellItemArray for the length of the call; what we take from
    // it is released by `Com`, the name freed with CoTaskMemFree.
    unsafe {
        crate::trace("[com] SetSelection");
        if array.is_null() {
            return S_OK;
        }
        let arr = crate::com::Com(array);
        let mut item = std::ptr::null_mut();
        let got = arr.slot::<unsafe extern "system" fn(This, u32, *mut This) -> Hr>(8)(array, 0, &mut item);
        std::mem::forget(arr); // borrowed, not ours to release
        if got < 0 || item.is_null() {
            return S_OK;
        }
        let item = crate::com::Com(item);
        let mut name: *mut u16 = std::ptr::null_mut();
        if item.slot::<unsafe extern "system" fn(This, i32, *mut *mut u16) -> Hr>(5)(item.raw(), SIGDN_FILESYSPATH, &mut name) >= 0 && !name.is_null() {
            let n = (0..).take_while(|&i| *name.add(i) != 0).count();
            let path = PathBuf::from(<std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(std::slice::from_raw_parts(name, n)));
            windows_sys::Win32::System::Com::CoTaskMemFree(name.cast());
            *from_sel(this).path.borrow_mut() = Some(path);
        }
        S_OK
    }
}

/// Open it: the same path a hand-over from another launch takes.
unsafe extern "system" fn cmd_execute(this: This) -> Hr {
    // SAFETY: a live object of ours.
    let path = unsafe { from_exec(this) }.path.borrow_mut().take();
    crate::trace("[com] Execute");
    crate::win::open_from_com(path);
    S_OK
}

// ------------------------------------------------------------------------------------------
// The class factory: one static object, registered with COM for the life of the process.

#[repr(C)]
struct FactoryVtbl {
    query: unsafe extern "system" fn(This, *const GUID, *mut This) -> Hr,
    add_ref: unsafe extern "system" fn(This) -> u32,
    release: unsafe extern "system" fn(This) -> u32,
    create: unsafe extern "system" fn(This, This, *const GUID, *mut This) -> Hr,
    lock: unsafe extern "system" fn(This, i32) -> Hr,
}

#[repr(C)]
struct Factory {
    vtbl: &'static FactoryVtbl,
}

static FACTORY_VTBL: FactoryVtbl = FactoryVtbl { query: factory_query, add_ref: static_ref, release: static_ref, create: factory_create, lock: factory_lock };
static FACTORY: Factory = Factory { vtbl: &FACTORY_VTBL };

unsafe extern "system" fn static_ref(_: This) -> u32 {
    1
}
unsafe extern "system" fn factory_lock(_: This, _: i32) -> Hr {
    S_OK
}
unsafe extern "system" fn factory_query(this: This, iid: *const GUID, out: *mut This) -> Hr {
    // SAFETY: COM's contract.
    unsafe {
        if out.is_null() {
            return E_POINTER;
        }
        if eq(&*iid, &IID_IUNKNOWN) || eq(&*iid, &IID_ICLASS_FACTORY) {
            *out = this;
            return S_OK;
        }
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}
unsafe extern "system" fn factory_create(_: This, outer: This, iid: *const GUID, out: *mut This) -> Hr {
    crate::trace("[com] CreateInstance");
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let c = Box::into_raw(Box::new(Command { exec: &EXEC_VTBL, sel: &SEL_VTBL, refs: AtomicU32::new(1), path: std::cell::RefCell::new(None) }));
    // SAFETY: a fresh object of ours; the initial reference is dropped after the query.
    unsafe {
        let hr = command_query(&*c, iid, out);
        command_release(&*c);
        hr
    }
}

/// Serve our class from this process, on this (single-threaded apartment) thread: calls come
/// through its message loop.
pub fn register() {
    use windows_sys::Win32::System::Com::{CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoInitializeEx, CoRegisterClassObject, REGCLS_MULTIPLEUSE};
    let mut cookie = 0;
    // SAFETY: the factory is static and lives as long as the process.
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        CoRegisterClassObject(&CLSID, &FACTORY as *const Factory as *mut c_void, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE as u32, &mut cookie);
    }
}
