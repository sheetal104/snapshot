// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES.
// SPDX-License-Identifier: Apache-2.0

//! CUDA symbol binding and resolution through the preloaded frontend. Scenarios call the
//! loader from the test binary, which glibc treats as the calling object.

use cuinterpose_gpu_tests::process::{self, Options, run, run_with};
use cuinterpose_gpu_tests::sys::{CUdeviceptr, CUdriverProcAddressQueryResult, CUresult};
use cuinterpose_gpu_tests::{cu, cuda, fixtures, shim};
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;

const SUCCESS: CUresult = CUresult::CUDA_SUCCESS;
const FOUND: c_int = CUdriverProcAddressQueryResult::CU_GET_PROC_ADDRESS_SUCCESS as c_int;
const NOT_FOUND: c_int =
    CUdriverProcAddressQueryResult::CU_GET_PROC_ADDRESS_SYMBOL_NOT_FOUND as c_int;

type Dlsym = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
// The old and new query entry points coexist in the installed driver.
type Query = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, u64) -> CUresult;
type QueryV2 =
    unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, u64, *mut c_int) -> CUresult;
type RuntimeQuery = unsafe extern "C" fn(*const c_char, *mut *mut c_void, u64, *mut c_int) -> c_int;
type RuntimeVersionQuery =
    unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_uint, u64, *mut c_int) -> c_int;
type Alloc = unsafe extern "C" fn(*mut CUdeviceptr, usize) -> CUresult;

#[test]
fn binary_linked_to_libcuda_calls_frontend_wrappers() {
    process::run_executable(&fixtures::direct(), &[], Options::default());
}

#[test]
fn dlsym_on_libcuda_handle_returns_wrappers_only_for_intercepted_apis() {
    run(|| {
        let driver = open_driver();
        take_dlerror();
        assert!(dlsym(driver, c"not_a_cuda_api").is_null());
        assert!(take_dlerror().is_some());
        let glibc = dlopen(c"libc.so.6", libc::RTLD_NOW);
        assert!(dlsym(glibc, c"cuMemCreate").is_null());
        assert_eq!(symbol(glibc, c"malloc"), original(glibc, c"malloc"));
        for name in [
            c"cuInit",
            c"cuMemCreate",
            c"cuCtxEnablePeerAccess",
            c"cuCtxDisablePeerAccess",
        ] {
            in_shim(symbol(driver, name), name);
        }
        assert_eq!(
            symbol(driver, c"cuDriverGetVersion"),
            original(driver, c"cuDriverGetVersion")
        );
    });
}

#[test]
fn dlsym_default_and_next_search_the_calling_library_scope() {
    let (plugin, dependency) = fixtures::scope_plugin();
    run(move || {
        // The plugin's dependency is private to it, so the plugin's RTLD_DEFAULT and
        // RTLD_NEXT lookups find the dependency only if glibc sees the plugin as the
        // caller.
        let plugin = open(&plugin, libc::RTLD_NOW | libc::RTLD_LOCAL);
        let dependency = open(&dependency, libc::RTLD_NOW | libc::RTLD_NOLOAD);
        let private_symbol = original(dependency, c"fixture_private_symbol");
        assert!(!private_symbol.is_null());
        assert!(dlsym(libc::RTLD_DEFAULT, c"fixture_private_symbol").is_null());
        type LookUp = unsafe extern "C" fn(*const c_char) -> *mut c_void;
        let look_up_default: LookUp = cast(symbol(plugin, c"fixture_scope_default"));
        let look_up_next: LookUp = cast(symbol(plugin, c"fixture_scope_next"));
        let name = c"fixture_private_symbol".as_ptr();
        assert_eq!(unsafe { look_up_default(name) }, private_symbol);
        assert_eq!(unsafe { look_up_next(name) }, private_symbol);
        in_shim(
            unsafe { look_up_default(c"cuMemCreate".as_ptr()) },
            c"cuMemCreate",
        );
        take_dlerror();
        assert!(unsafe { look_up_default(c"not_a_symbol".as_ptr()) }.is_null());
        assert!(take_dlerror().is_some());
    });
}

#[test]
fn proc_address_queries_return_wrapper_for_driver_selected_variant() {
    run(|| {
        let driver = open_driver();
        cuda::primary_contexts(1);
        let query: Query = cast(symbol(driver, c"cuGetProcAddress"));
        let native: Query = cast(original(driver, c"cuGetProcAddress"));
        let query_v2: QueryV2 = cast(symbol(driver, c"cuGetProcAddress_v2"));
        let native_v2: QueryV2 = cast(original(driver, c"cuGetProcAddress_v2"));
        let query_ptsz: QueryV2 = cast(symbol(libc::RTLD_DEFAULT, c"cuGetProcAddress_v2_ptsz"));
        for (name, version) in [
            (c"cuCtxEnablePeerAccess", 13000),
            (c"cuCtxDisablePeerAccess", 13000),
            (c"cuCtxDestroy", 2000),
            (c"cuCtxDestroy", 13000),
            (c"cuDevicePrimaryCtxRelease", 7000),
            (c"cuDevicePrimaryCtxRelease", 13000),
            (c"cuDevicePrimaryCtxReset", 7000),
            (c"cuDevicePrimaryCtxReset", 13000),
            (c"cuMemAlloc", 13000),
            (c"cuIpcOpenMemHandle", 13000),
            (c"cuMemCreate", 11000),
            (c"cuMemMap", 13000),
            (c"cuMulticastBindMem", 12010),
            (c"cuMulticastBindMem", 13010),
            (c"cuMulticastBindAddr", 12010),
            (c"cuMulticastBindAddr", 13010),
            (c"cuGetProcAddress", 11030),
            (c"cuGetProcAddress", 12000),
        ] {
            let expected = proc_address(native, name, version);
            wraps(proc_address(query, name, version), expected);
            let expected = proc_address_v2(native_v2, name, version);
            wraps(proc_address_v2(query_v2, name, version), expected);
        }
        allocate(proc_address(query, c"cuMemAlloc", 13000));
        // A query function returned by a query must also return wrappers.
        let nested: Query = cast(proc_address(query, c"cuGetProcAddress", 11030));
        allocate(proc_address(nested, c"cuMemAlloc", 13000));
        let nested: QueryV2 = cast(proc_address(query, c"cuGetProcAddress", 12000));
        allocate(proc_address_v2(nested, c"cuMemAlloc", 13000));
        let mut address = proc_address_v2(query_ptsz, c"cuMemAlloc", 13000);
        allocate(address);
        // A missing symbol overwrites the previous result and status.
        let mut status = FOUND;
        let name = c"not_a_cuda_api".as_ptr();
        assert_eq!(
            unsafe { query_v2(name, &mut address, 13000, 0, &mut status) },
            SUCCESS
        );
        assert!(address.is_null());
        assert_eq!(status, NOT_FOUND);
        assert_eq!(
            proc_address(query, c"cuDriverGetVersion", 13000),
            original(driver, c"cuDriverGetVersion")
        );
    });
}

#[test]
fn cudart_driver_entry_point_queries_return_wrappers() {
    run(|| {
        cuda::primary_contexts(1);
        let runtime = dlopen(c"libcudart.so.13", libc::RTLD_NOW | libc::RTLD_LOCAL);
        for (name, by_version) in [
            (c"cudaGetDriverEntryPoint", false),
            (c"cudaGetDriverEntryPoint_ptsz", false),
            (c"cudaGetDriverEntryPointByVersion", true),
            (c"cudaGetDriverEntryPointByVersion_ptsz", true),
        ] {
            let resolver = symbol(runtime, name);
            in_shim(resolver, name);
            allocate(entry_point(resolver, by_version, c"cuMemAlloc"));
            for peer in [c"cuCtxEnablePeerAccess", c"cuCtxDisablePeerAccess"] {
                in_shim(entry_point(resolver, by_version, peer), peer);
            }
        }
    });
}

#[test]
fn libcuda_stays_loaded_after_application_dlcloses_its_handle() {
    run(|| {
        // A lookup through the application's handle makes the frontend retain the
        // driver, so the driver stays loaded after that handle closes. cuInit makes
        // libcuda pin itself, so the handle must close before it.
        let driver = open_driver();
        in_shim(symbol(driver, c"cuMemAlloc_v2"), c"cuMemAlloc_v2");
        assert_eq!(unsafe { libc::dlclose(driver) }, 0);
        let driver = dlopen(c"libcuda.so.1", libc::RTLD_NOW | libc::RTLD_NOLOAD);
        cuda::primary_contexts(1);
        allocate(symbol(driver, c"cuMemAlloc_v2"));
    });
}

#[test]
fn wrappers_return_not_initialized_when_core_library_is_missing() {
    let frontend = process::work_dir().join("without-core/libcuinterpose.so");
    // The scenario process preloads this copy, so only the test process writes it.
    if process::role().is_none() {
        fs::create_dir(frontend.parent().unwrap()).unwrap();
        fs::copy(&shim::artifacts().frontend, &frontend).unwrap();
    }
    let options = Options::default().env("LD_PRELOAD", frontend.to_str().unwrap());
    run_with(options, || {
        let driver = open_driver();
        let address = symbol(driver, c"cuMemAlloc_v2");
        in_shim(address, c"cuMemAlloc_v2");
        let alloc: Alloc = cast(address);
        let mut output = 42;
        assert_eq!(
            unsafe { alloc(&mut output, 4096) },
            CUresult::CUDA_ERROR_NOT_INITIALIZED
        );
        assert_eq!(output, 42);
        let initialize: unsafe extern "C" fn(c_uint) -> CUresult = cast(symbol(driver, c"cuInit"));
        assert_eq!(
            unsafe { initialize(0) },
            CUresult::CUDA_ERROR_NOT_INITIALIZED
        );
    });
}

fn dlopen(name: &CStr, flags: c_int) -> *mut c_void {
    let handle = unsafe { libc::dlopen(name.as_ptr(), flags) };
    assert!(
        !handle.is_null(),
        "cannot load {name:?}: {:?}",
        take_dlerror()
    );
    handle
}

fn open(path: &Path, flags: c_int) -> *mut c_void {
    dlopen(&CString::new(path.as_os_str().as_bytes()).unwrap(), flags)
}

/// The application's own libcuda handle.
fn open_driver() -> *mut c_void {
    dlopen(c"libcuda.so.1", libc::RTLD_NOW | libc::RTLD_LOCAL)
}

/// Looks up `name` as the application does, through the frontend's dlsym.
fn dlsym(handle: *mut c_void, name: &CStr) -> *mut c_void {
    unsafe { libc::dlsym(handle, name.as_ptr()) }
}

fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
    let address = dlsym(handle, name);
    assert!(
        !address.is_null(),
        "missing required symbol {name:?}: {:?}",
        take_dlerror()
    );
    address
}

/// Looks up `name` with glibc's dlsym, which bypasses the frontend.
fn original(handle: *mut c_void, name: &CStr) -> *mut c_void {
    let version = c"GLIBC_2.34".as_ptr();
    let glibc: Dlsym = cast(unsafe { libc::dlvsym(libc::RTLD_NEXT, c"dlsym".as_ptr(), version) });
    unsafe { glibc(handle, name.as_ptr()) }
}

fn take_dlerror() -> Option<String> {
    let message = unsafe { libc::dlerror() };
    (!message.is_null()).then(|| {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Converts a symbol address to a function of type `F`.
fn cast<F: Copy>(address: *mut c_void) -> F {
    assert!(!address.is_null());
    // SAFETY: callers name a function with the signature F.
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) }
}

/// The file that contains `address` and the name of its symbol.
fn locate(address: *mut c_void) -> (String, Option<CString>) {
    // SAFETY: Dl_info is plain data.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    assert_ne!(
        unsafe { libc::dladdr(address, &mut info) },
        0,
        "dladdr({address:?})"
    );
    let file = unsafe { CStr::from_ptr(info.dli_fname) };
    let name = (!info.dli_sname.is_null()).then(|| unsafe { CStr::from_ptr(info.dli_sname) });
    (
        file.to_string_lossy().into_owned(),
        name.map(CStr::to_owned),
    )
}

fn in_shim(address: *mut c_void, name: &CStr) {
    let (file, symbol) = locate(address);
    assert!(
        file.contains("libcuinterpose.so"),
        "{name:?} resolved into {file}"
    );
    assert_eq!(symbol.as_deref(), Some(name), "{file}");
}

/// A query result must be the wrapper for the entry point the driver returned, which can
/// be a newer variant of the requested name, such as _v2.
fn wraps(actual: *mut c_void, expected: *mut c_void) {
    assert!(!expected.is_null());
    let (file, name) = locate(expected);
    in_shim(
        actual,
        &name.unwrap_or_else(|| panic!("unnamed entry point in {file}")),
    );
}

fn proc_address(query: Query, name: &CStr, version: c_int) -> *mut c_void {
    let mut address = null_mut();
    let status = unsafe { query(name.as_ptr(), &mut address, version, 0) };
    assert_eq!(status, SUCCESS, "{name:?} at {version}");
    address
}

/// Queries `name` through a cuGetProcAddress_v2 variant, which must find it.
fn proc_address_v2(query: QueryV2, name: &CStr, version: c_int) -> *mut c_void {
    let mut address = null_mut();
    let mut found = NOT_FOUND;
    let status = unsafe { query(name.as_ptr(), &mut address, version, 0, &mut found) };
    assert_eq!((status, found), (SUCCESS, FOUND), "{name:?} at {version}");
    address
}

/// Queries `name` through a cudaGetDriverEntryPoint variant, which must find it.
fn entry_point(resolver: *mut c_void, by_version: bool, name: &CStr) -> *mut c_void {
    let mut address = null_mut();
    let mut status = -1;
    let result = if by_version {
        let resolve: RuntimeVersionQuery = cast(resolver);
        unsafe { resolve(name.as_ptr(), &mut address, 13000, 0, &mut status) }
    } else {
        let resolve: RuntimeQuery = cast(resolver);
        unsafe { resolve(name.as_ptr(), &mut address, 0, &mut status) }
    };
    assert_eq!((result, status), (0, 0), "{name:?}");
    address
}

/// Allocates through a cuMemAlloc wrapper and uses the memory through the driver. The
/// wrapper needs the runtime and a current context.
fn allocate(function: *mut c_void) {
    in_shim(function, c"cuMemAlloc_v2");
    let alloc: Alloc = cast(function);
    let mut address = 0;
    assert_eq!(unsafe { alloc(&mut address, 4096) }, SUCCESS);
    assert_ne!(address, 0);
    cu!(cuMemsetD8_v2(address, 0x7b, 4096));
    assert_eq!(cuda::read_bytes(address, 1), [0x7b]);
    cu!(cuMemFree_v2(address));
}
