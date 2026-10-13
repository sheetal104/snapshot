// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES.
// SPDX-License-Identifier: Apache-2.0

//! Registers a test frontend with the core directly, without the preloaded frontend. The
//! test supplies the resolver, so it can hide or wrap the driver symbols the core uses.
//! Tracked calls must then go through the returned backend table.

use crate::shim::artifacts;
use crate::sys::CUresult;
use cuinterpose_abi::{ABI_VERSION, BackendAbi, FrontendAbi, Resolve};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

pub type Initialize = unsafe extern "C" fn(*const FrontendAbi, *mut *const BackendAbi) -> CUresult;

/// The core's registration entry point.
pub fn initialize() -> Initialize {
    static INITIALIZE: OnceLock<Initialize> = OnceLock::new();
    *INITIALIZE.get_or_init(|| {
        let path = CString::new(artifacts().core.as_os_str().as_bytes()).unwrap();
        let core = open(&path);
        let address = unsafe { libc::dlsym(core, c"cuinterpose_core_init".as_ptr()) };
        assert!(
            !address.is_null(),
            "the core does not export cuinterpose_core_init"
        );
        // SAFETY: the core exports this entry point with the ABI's signature.
        unsafe { std::mem::transmute::<*mut c_void, Initialize>(address) }
    })
}

pub fn frontend(resolve: Resolve) -> FrontendAbi {
    FrontendAbi {
        version: ABI_VERSION,
        size: size_of::<FrontendAbi>() as u32,
        resolve,
    }
}

/// Registers a frontend that resolves symbols with `resolve` and returns the core's
/// backend table.
pub fn register(resolve: Resolve) -> &'static BackendAbi {
    let mut backend = std::ptr::null();
    let status = unsafe { initialize()(&frontend(resolve), &mut backend) };
    assert_eq!(status, CUresult::CUDA_SUCCESS, "register a test frontend");
    // SAFETY: the core's table remains valid until process exit.
    unsafe { &*backend }
}

/// The real driver's definition of `name`.
pub fn driver_symbol(name: &CStr) -> *mut c_void {
    static DRIVER: OnceLock<usize> = OnceLock::new();
    let driver = *DRIVER.get_or_init(|| open(c"libcuda.so.1") as usize);
    unsafe { libc::dlsym(driver as *mut c_void, name.as_ptr()) }
}

/// A resolver that returns the real driver's definitions.
///
/// # Safety
/// `name` must be a NUL-terminated string.
pub unsafe extern "C" fn resolve_driver(name: *const c_char) -> *mut c_void {
    driver_symbol(unsafe { CStr::from_ptr(name) })
}

fn open(path: &CStr) -> *mut c_void {
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "cannot load {path:?}");
    handle
}
