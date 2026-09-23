//! Taking a patch into a running library.
//!
//! Compiled only under `--cfg=frustrate_hot_patch`, which
//! `frustrate_hot_patchable` sets for `-c dbg` builds; a build without it
//! contains none of this, and its generated entry points none of the routing.
//!
//! A patchable library routes each generated entry point `X` through a slot,
//! the exported static `frustrate_hot_slot_X`: while the slot is null, `X` runs
//! its own body; otherwise it forwards to the function the slot holds. A patch
//! is a second library, built by `//hotpatch` against this exact image, whose
//! descriptor names the slots it fills and the function for each. Applying one
//! validates everything first — the ABI, the image identity, the address the
//! patch was linked for, every slot name — and only then swaps the slots, so a
//! refused patch leaves the running code untouched.
//!
//! Work that already started keeps the code it started with: an actor job,
//! an executor task, a stored closure or a trait object created before the
//! patch runs the old function, as a Dart closure created before a hot reload
//! does. A patch library is never unloaded, so that code stays valid.
//!
//! The two exports below are rules_flutter's `flutter_native_library.hot_patch`
//! seam: the dev tool calls `flutter_hot_patch_abi` and then
//! `flutter_hot_patch_apply` with the delivered file, or with null to send
//! every call back to the launched code.

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

/// The patch ABI this runtime accepts. `//hotpatch` emits the same number.
pub const ABI: u32 = 1;

/// The function to call instead of `this`, if a patch installed one.
///
/// `this` is the address of the routed function itself, as the code asking
/// sees it. A patch carries its own copy of the entry point, and that copy
/// reads the same slot: when the slot names the copy asking, it runs its body
/// rather than forwarding to itself.
#[inline(always)]
pub fn route(slot: &AtomicPtr<c_void>, this: *const c_void) -> Option<*const c_void> {
    let target = slot.load(Ordering::Acquire);
    if target.is_null() || target as *const c_void == this {
        None
    } else {
        Some(target)
    }
}

/// What a patch library exports as `frustrate_hot_patch_descriptor`. The layout
/// is shared with `//hotpatch` (`patch::descriptor`).
#[repr(C)]
struct Descriptor {
    abi: u32,
    identity_len: u32,
    identity: [u8; 32],
    anchor: u64,
    count: u64,
    entries: *const Entry,
}

#[repr(C)]
struct Entry {
    slot: *const c_char,
    target: *const c_void,
}

/// The slots the current patch filled, so the next patch or a reset can empty
/// the ones it does not fill.
static FILLED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[no_mangle]
pub extern "C" fn flutter_hot_patch_abi() -> u32 {
    ABI
}

/// Load the patch at `patch_path` and route calls into it, or with a null path
/// route every call back to the launched code. Returns 0, or non-zero with the
/// reason written into `message` (NUL-terminated, truncated to fit).
///
/// # Safety
/// `patch_path` is null or a NUL-terminated path; `message` is null or points
/// to `message_capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn flutter_hot_patch_apply(
    patch_path: *const c_char,
    message: *mut c_char,
    message_capacity: usize,
) -> i32 {
    let anchor = flutter_hot_patch_apply as *const c_void;
    let result = std::panic::catch_unwind(|| unsafe { apply(patch_path, anchor) })
        .unwrap_or_else(|_| Err("applying the patch panicked".to_string()));
    match result {
        Ok(()) => 0,
        Err(reason) => {
            if !message.is_null() && message_capacity > 0 {
                let bytes = reason.as_bytes();
                let n = bytes.len().min(message_capacity - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), message as *mut u8, n);
                    *message.add(n) = 0;
                }
            }
            1
        }
    }
}

unsafe fn apply(patch_path: *const c_char, anchor: *const c_void) -> Result<(), String> {
    let mut filled = FILLED.lock().unwrap_or_else(|p| p.into_inner());
    if patch_path.is_null() {
        for &slot in filled.iter() {
            unsafe { &*(slot as *const AtomicPtr<c_void>) }.store(std::ptr::null_mut(), Ordering::Release);
        }
        filled.clear();
        return Ok(());
    }

    let own = sys::own_image(anchor)?;
    let path = unsafe { CStr::from_ptr(patch_path) };
    let handle = unsafe { sys::dlopen(path.as_ptr(), sys::RTLD_NOW | sys::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("could not load {}: {}", path.to_string_lossy(), sys::last_error()));
    }
    let descriptor = unsafe { sys::dlsym(handle, c"frustrate_hot_patch_descriptor".as_ptr()) } as *const Descriptor;
    if descriptor.is_null() {
        return Err(format!("{} is not a frustrate patch", path.to_string_lossy()));
    }
    let d = unsafe { &*descriptor };
    if d.abi != ABI {
        return Err(format!("the patch speaks patch ABI {}, this library speaks {ABI}", d.abi));
    }
    let identity = &d.identity[..(d.identity_len as usize).min(32)];
    if identity.is_empty() || identity != own.identity.as_slice() {
        return Err("the patch was built for a different build of this library".to_string());
    }
    if d.anchor != anchor as u64 {
        return Err(format!(
            "the patch was linked for this library loaded at a different address ({:#x}, running at {:#x}); it belongs to another process",
            d.anchor, anchor as u64
        ));
    }

    let entries: &[Entry] = if d.count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(d.entries, d.count as usize) }
    };
    let mut resolved = Vec::with_capacity(entries.len());
    for e in entries {
        let name = unsafe { CStr::from_ptr(e.slot) };
        if !name.to_bytes().starts_with(b"frustrate_hot_slot_") {
            return Err(format!("the patch names `{}`, which is not a slot", name.to_string_lossy()));
        }
        let slot = unsafe { sys::dlsym(own.handle, name.as_ptr()) } as *const AtomicPtr<c_void>;
        if slot.is_null() {
            return Err(format!("this library has no slot `{}`", name.to_string_lossy()));
        }
        resolved.push((slot as usize, e.target));
    }

    // Validated; swap. Slots the previous patch filled and this one does not go
    // back to the launched code.
    for &slot in filled.iter() {
        if !resolved.iter().any(|(s, _)| *s == slot) {
            unsafe { &*(slot as *const AtomicPtr<c_void>) }.store(std::ptr::null_mut(), Ordering::Release);
        }
    }
    for &(slot, target) in &resolved {
        unsafe { &*(slot as *const AtomicPtr<c_void>) }.store(target as *mut c_void, Ordering::Release);
    }
    *filled = resolved.into_iter().map(|(s, _)| s).collect();
    Ok(())
}

mod sys {
    use std::ffi::{c_char, c_int, c_void, CStr};

    pub const RTLD_NOW: c_int = 2;
    #[cfg(target_vendor = "apple")]
    pub const RTLD_LOCAL: c_int = 4;
    #[cfg(not(target_vendor = "apple"))]
    pub const RTLD_LOCAL: c_int = 0;
    #[cfg(target_vendor = "apple")]
    const RTLD_NOLOAD: c_int = 0x10;
    #[cfg(not(target_vendor = "apple"))]
    const RTLD_NOLOAD: c_int = 4;

    extern "C" {
        pub fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
    }

    #[repr(C)]
    struct DlInfo {
        fname: *const c_char,
        base: *mut c_void,
        sname: *const c_char,
        saddr: *mut c_void,
    }

    pub fn last_error() -> String {
        let e = unsafe { dlerror() };
        if e.is_null() {
            "unknown error".to_string()
        } else {
            unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
        }
    }

    pub struct OwnImage {
        pub handle: *mut c_void,
        pub identity: Vec<u8>,
    }

    /// The library `anchor` lives in: a handle to look its slots up by name,
    /// and its linker identity as loaded.
    pub fn own_image(anchor: *const c_void) -> Result<OwnImage, String> {
        let mut info = DlInfo {
            fname: std::ptr::null(),
            base: std::ptr::null_mut(),
            sname: std::ptr::null(),
            saddr: std::ptr::null_mut(),
        };
        if unsafe { dladdr(anchor, &mut info) } == 0 || info.fname.is_null() {
            return Err("could not find this library's own image".to_string());
        }
        let handle = unsafe { dlopen(info.fname, RTLD_NOW | RTLD_NOLOAD) };
        if handle.is_null() {
            return Err(format!("could not reopen this library: {}", last_error()));
        }
        let identity = identity(anchor, info.fname)?;
        Ok(OwnImage { handle, identity })
    }

    #[cfg(target_vendor = "apple")]
    fn identity(_anchor: *const c_void, fname: *const c_char) -> Result<Vec<u8>, String> {
        extern "C" {
            fn _dyld_image_count() -> u32;
            fn _dyld_get_image_name(i: u32) -> *const c_char;
            fn _dyld_get_image_header(i: u32) -> *const MachHeader64;
        }
        #[repr(C)]
        struct MachHeader64 {
            magic: u32,
            cputype: i32,
            cpusubtype: i32,
            filetype: u32,
            ncmds: u32,
            sizeofcmds: u32,
            flags: u32,
            reserved: u32,
        }
        const LC_UUID: u32 = 0x1b;
        let me = unsafe { CStr::from_ptr(fname) };
        for i in 0..unsafe { _dyld_image_count() } {
            let name = unsafe { _dyld_get_image_name(i) };
            if name.is_null() || unsafe { CStr::from_ptr(name) } != me {
                continue;
            }
            let header = unsafe { _dyld_get_image_header(i) };
            let mut cmd = unsafe { (header as *const u8).add(std::mem::size_of::<MachHeader64>()) };
            for _ in 0..unsafe { (*header).ncmds } {
                let (kind, size) = unsafe { (*(cmd as *const u32), *(cmd.add(4) as *const u32)) };
                if kind == LC_UUID {
                    return Ok(unsafe { std::slice::from_raw_parts(cmd.add(8), 16) }.to_vec());
                }
                cmd = unsafe { cmd.add(size as usize) };
            }
            return Err("this library carries no LC_UUID".to_string());
        }
        Err("this library is not in the loader's image list".to_string())
    }

    #[cfg(not(target_vendor = "apple"))]
    fn identity(anchor: *const c_void, _fname: *const c_char) -> Result<Vec<u8>, String> {
        #[repr(C)]
        struct PhdrInfo {
            addr: usize,
            name: *const c_char,
            phdr: *const Phdr,
            phnum: u16,
        }
        #[repr(C)]
        struct Phdr {
            p_type: u32,
            p_flags: u32,
            p_offset: u64,
            p_vaddr: u64,
            p_paddr: u64,
            p_filesz: u64,
            p_memsz: u64,
            p_align: u64,
        }
        extern "C" {
            fn dl_iterate_phdr(
                callback: unsafe extern "C" fn(*const PhdrInfo, usize, *mut c_void) -> c_int,
                data: *mut c_void,
            ) -> c_int;
        }
        const PT_LOAD: u32 = 1;
        const PT_NOTE: u32 = 4;
        const NT_GNU_BUILD_ID: u32 = 3;

        struct Search {
            anchor: usize,
            found: Option<Vec<u8>>,
            contained: bool,
        }

        unsafe extern "C" fn visit(info: *const PhdrInfo, _size: usize, data: *mut c_void) -> c_int {
            let search = unsafe { &mut *(data as *mut Search) };
            let info = unsafe { &*info };
            let phdrs = unsafe { std::slice::from_raw_parts(info.phdr, info.phnum as usize) };
            let contains = phdrs.iter().any(|p| {
                p.p_type == PT_LOAD && {
                    let start = info.addr + p.p_vaddr as usize;
                    (start..start + p.p_memsz as usize).contains(&search.anchor)
                }
            });
            if !contains {
                return 0;
            }
            search.contained = true;
            for p in phdrs.iter().filter(|p| p.p_type == PT_NOTE) {
                let mut at = info.addr + p.p_vaddr as usize;
                let end = at + p.p_memsz as usize;
                while at + 12 <= end {
                    let (namesz, descsz, kind) = unsafe {
                        (
                            *(at as *const u32) as usize,
                            *((at + 4) as *const u32) as usize,
                            *((at + 8) as *const u32),
                        )
                    };
                    let name = at + 12;
                    let desc = name + namesz.next_multiple_of(4);
                    if kind == NT_GNU_BUILD_ID && namesz == 4 {
                        let owner = unsafe { std::slice::from_raw_parts(name as *const u8, 4) };
                        if owner == b"GNU\0" {
                            search.found = Some(unsafe { std::slice::from_raw_parts(desc as *const u8, descsz) }.to_vec());
                            return 1;
                        }
                    }
                    at = desc + descsz.next_multiple_of(4);
                }
            }
            1
        }

        let mut search = Search {
            anchor: anchor as usize,
            found: None,
            contained: false,
        };
        unsafe { dl_iterate_phdr(visit, &mut search as *mut Search as *mut c_void) };
        match (search.contained, search.found) {
            (_, Some(id)) => Ok(id),
            (true, None) => Err("this library carries no GNU build-id".to_string()),
            (false, None) => Err("this library is not in the loader's image list".to_string()),
        }
    }
}
