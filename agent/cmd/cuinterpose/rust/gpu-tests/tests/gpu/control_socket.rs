// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES.
// SPDX-License-Identifier: Apache-2.0

//! When and how the frontend's cuInit starts the process's control socket.

use cuinterpose_gpu_tests::process::{self, Options, run, run_with, socket_dir};
use cuinterpose_gpu_tests::sys::CUresult;
use cuinterpose_gpu_tests::{fixtures, shim};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, connect, listen, socket};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::{Duration, Instant};

const SUCCESS: CUresult = CUresult::CUDA_SUCCESS;

fn endpoint() -> PathBuf {
    cuinterpose_protocol::socket_path(&socket_dir(), std::process::id())
}

/// Resolves `name` in the global scope, where the preloaded frontend comes first.
fn global<F: Copy>(name: &CStr) -> F {
    let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    assert!(!address.is_null(), "missing {name:?}");
    // SAFETY: callers name a CUDA entry point with the signature F.
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) }
}

fn cu_init(flags: u32) -> CUresult {
    let initialize: unsafe extern "C" fn(u32) -> CUresult = global(c"cuInit");
    unsafe { initialize(flags) }
}

fn assert_no_records() {
    assert_eq!(shim::records(std::process::id()), []);
}

/// The address that a cuGetProcAddress variant reports for `name`.
fn proc_address(query: &CStr, name: &CStr) -> *mut c_void {
    type Query = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, u64) -> CUresult;
    type QueryV2 =
        unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, u64, *mut c_int) -> CUresult;
    let mut address = std::ptr::null_mut();
    let status = if query == c"cuGetProcAddress" {
        unsafe { global::<Query>(query)(name.as_ptr(), &mut address, 13010, 0) }
    } else {
        let mut found = 0;
        unsafe { global::<QueryV2>(query)(name.as_ptr(), &mut address, 13010, 0, &mut found) }
    };
    assert_eq!(status, SUCCESS, "{query:?}({name:?})");
    address
}

fn frontend_address(name: &CStr) -> *mut c_void {
    let path = CString::new(shim::artifacts().frontend.as_os_str().as_bytes()).unwrap();
    let frontend = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    assert!(!frontend.is_null(), "the frontend is not loaded");
    unsafe { libc::dlsym(frontend, name.as_ptr()) }
}

/// Runs `body` in a fork child and requires it to return.
fn in_fork_child(body: fn()) {
    match unsafe { libc::fork() } {
        0 => {
            let code = std::panic::catch_unwind(body).map_or(1, |()| 0);
            unsafe { libc::_exit(code) }
        }
        pid => {
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "fork child failed"
            );
        }
    }
}

#[test]
fn cuinit_creates_socket_with_mode_0600_under_umask_0() {
    run(|| {
        assert!(!endpoint().exists());
        // Under umask 0, only an explicit chmod makes the socket private.
        unsafe { libc::umask(0) };
        assert_eq!(cu_init(0), SUCCESS);
        let metadata = fs::metadata(endpoint()).unwrap();
        let mode = metadata.permissions().mode() & 0o7777;
        assert!(
            metadata.file_type().is_socket() && mode == 0o600,
            "{mode:o}"
        );
        assert_eq!(
            unsafe { libc::umask(0) },
            0,
            "runtime startup changed the umask"
        );
        assert_no_records();
    });
}

#[test]
fn failed_cuinit_creates_no_socket() {
    run(|| {
        assert_eq!(cu_init(1), CUresult::CUDA_ERROR_INVALID_VALUE);
        assert!(!endpoint().exists());
        assert_eq!(cu_init(0), SUCCESS);
        assert_no_records();
    });
}

#[test]
fn relative_ld_preload_finds_core_after_chdir() {
    let frontend = &shim::artifacts().frontend;
    let name = frontend.file_name().unwrap().to_str().unwrap();
    let options = Options::default()
        .env("LD_PRELOAD", format!("./{name}"))
        .cwd(frontend.parent().unwrap());
    run_with(options, || {
        std::env::set_current_dir(process::work_dir()).unwrap();
        // The relative preload no longer resolves, so only $ORIGIN can locate the core.
        assert!(!Path::new(&std::env::var("LD_PRELOAD").unwrap()).exists());
        assert_eq!(cu_init(0), SUCCESS);
        assert_no_records();
    });
}

// The first query loads the driver, so each variant runs in its own process.

#[test]
fn proc_address_query_before_cuinit_returns_wrapper_without_creating_socket() {
    query_before_cuinit(c"cuGetProcAddress");
}

#[test]
fn proc_address_v2_query_before_cuinit_returns_wrapper_without_creating_socket() {
    query_before_cuinit(c"cuGetProcAddress_v2");
}

#[test]
fn proc_address_v2_ptsz_query_before_cuinit_returns_wrapper_without_creating_socket() {
    query_before_cuinit(c"cuGetProcAddress_v2_ptsz");
}

fn query_before_cuinit(query: &'static CStr) {
    run(move || {
        assert_eq!(
            proc_address(query, c"cuMemCreate"),
            frontend_address(c"cuMemCreate")
        );
        assert!(!endpoint().exists());
        assert_eq!(cu_init(0), SUCCESS);
    });
}

#[test]
fn concurrent_cuinit_starts_one_runtime_and_one_socket() {
    run(|| {
        let barrier = Barrier::new(16);
        let results: Vec<_> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cu_init(0)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        assert_eq!(results, [SUCCESS; 16]);
        let inode = fs::metadata(endpoint()).unwrap().ino();
        for _ in 0..16 {
            assert_eq!(cu_init(0), SUCCESS);
        }
        assert_eq!(fs::metadata(endpoint()).unwrap().ino(), inode);
        let entries: Vec<_> = fs::read_dir(socket_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries, [endpoint()]);
        assert_no_records();
        // Workers of losing candidates exit asynchronously after their callers return.
        let deadline = Instant::now() + Duration::from_secs(10);
        while shim_threads() != 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shim_threads(), 2);
    });
}

/// Counts the shim's control and peer threads by exact name. A thread's comm holds the
/// first 15 bytes of its name, and the packaged test binary's own threads show as
/// `cuinterpose-gpu`, which a prefix match would count.
fn shim_threads() -> usize {
    fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| matches!(name.trim_end(), "cuinterpose-con" | "cuinterpose-pee"))
        .count()
}

#[test]
fn cuinit_from_library_constructor_does_not_deadlock() {
    let library = fixtures::cuinit_constructor();
    run(move || {
        let path = CString::new(library.as_os_str().as_bytes()).unwrap();
        // The constructor calls cuInit while holding the loader lock.
        assert!(!unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) }.is_null());
        assert_no_records();
    });
}

#[test]
fn child_forked_before_cuinit_gets_its_own_socket() {
    run(|| {
        in_fork_child(initialize_endpoint);
        initialize_endpoint();
    });
}

fn initialize_endpoint() {
    assert_eq!(cu_init(0), SUCCESS);
    assert_no_records();
}

#[test]
fn child_forked_after_cuinit_gets_not_initialized() {
    let stderr = run(|| {
        assert_eq!(cu_init(0), SUCCESS);
        in_fork_child(use_parent_runtime);
        assert_no_records();
    });
    assert!(
        stderr.contains("runtime belongs to another process"),
        "{stderr}"
    );
}

fn use_parent_runtime() {
    // The driver rejects cuInit in a fork child before the shim runs. A tracked call
    // reaches the shim, whose ownership check writes the diagnostic.
    let release: unsafe extern "C" fn(u64) -> CUresult = global(c"cuMemRelease");
    assert_eq!(cu_init(0), CUresult::CUDA_ERROR_NOT_INITIALIZED);
    assert_eq!(unsafe { release(42) }, CUresult::CUDA_ERROR_NOT_INITIALIZED);
    assert!(!endpoint().exists());
}

#[test]
fn cuinit_after_exec_replaces_stale_socket_at_same_pid() {
    if process::role().as_deref() == Some("after-exec") {
        // The old image's listener closed on exec and left a stale socket at this path.
        assert!(fs::metadata(endpoint()).unwrap().file_type().is_socket());
        assert_eq!(cu_init(0), SUCCESS);
        assert_no_records();
        return;
    }
    run(|| {
        assert_eq!(cu_init(0), SUCCESS);
        process::exec("after-exec");
    });
}

#[derive(Clone, Copy)]
enum Existing {
    RegularFile,
    SymlinkToStaleSocket,
    LiveListener,
    ListenerWithFullBacklog,
}

#[test]
fn cuinit_fails_without_touching_existing_regular_file() {
    cuinit_fails_without_touching(Existing::RegularFile);
}

#[test]
fn cuinit_fails_without_touching_symlink_to_stale_socket() {
    cuinit_fails_without_touching(Existing::SymlinkToStaleSocket);
}

#[test]
fn cuinit_fails_without_touching_live_listener() {
    cuinit_fails_without_touching(Existing::LiveListener);
}

#[test]
fn cuinit_fails_without_touching_listener_with_full_backlog() {
    cuinit_fails_without_touching(Existing::ListenerWithFullBacklog);
}

fn cuinit_fails_without_touching(existing: Existing) {
    let stderr = run(move || {
        let path = endpoint();
        let _sockets = occupy(&path, existing);
        let before = socket_dir_entries();
        assert_eq!(cu_init(0), CUresult::CUDA_ERROR_OPERATING_SYSTEM);
        assert_eq!(socket_dir_entries(), before);
        if let Existing::RegularFile = existing {
            assert_eq!(fs::read_to_string(&path).unwrap(), "preserve this file");
        }
        // Procedure queries do not depend on the failed runtime.
        let query = proc_address(c"cuGetProcAddress_v2", c"cuMemCreate");
        assert_eq!(query, frontend_address(c"cuMemCreate"));
        fs::remove_file(&path).unwrap();
        // The startup failure is permanent even once the path is free.
        assert_eq!(cu_init(0), CUresult::CUDA_ERROR_NOT_INITIALIZED);
        assert!(!path.exists());
    });
    assert!(stderr.contains("bind control socket"), "{stderr}");
    assert!(
        stderr.contains(&format!("os error {}", libc::EADDRINUSE)),
        "{stderr}"
    );
}

/// Puts `existing` at `path` and returns the sockets that must stay open.
fn occupy(path: &Path, existing: Existing) -> Vec<rustix::fd::OwnedFd> {
    let listener = |path: &Path, backlog| {
        let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        bind(&fd, &SocketAddrUnix::new(path).unwrap()).unwrap();
        listen(&fd, backlog).unwrap();
        fd
    };
    match existing {
        Existing::RegularFile => {
            fs::write(path, "preserve this file").unwrap();
            vec![]
        }
        Existing::SymlinkToStaleSocket => {
            // The link points at a stale socket, so following it would make the path
            // look replaceable.
            let target = path.with_extension("target");
            drop(listener(&target, 1));
            std::os::unix::fs::symlink(&target, path).unwrap();
            vec![]
        }
        Existing::LiveListener => vec![listener(path, 1)],
        Existing::ListenerWithFullBacklog => {
            // A blocking probe would hang here while holding the installation lock.
            let live = listener(path, 0);
            let queued = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
            connect(&queued, &SocketAddrUnix::new(path).unwrap()).unwrap();
            vec![live, queued]
        }
    }
}

/// Maps each entry to its own inode and mode, so a replaced path or link shows up.
fn socket_dir_entries() -> BTreeMap<PathBuf, (u64, u32)> {
    fs::read_dir(socket_dir())
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            (path, (metadata.ino(), metadata.mode()))
        })
        .collect()
}
