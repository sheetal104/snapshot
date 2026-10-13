// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES.
// SPDX-License-Identifier: Apache-2.0

//! Peer access grants on converted mallocs through enable, disable, and teardown.

use cudarc::runtime::sys::{self as runtime, cudaError};
use cuinterpose_gpu_tests::process::run;
use cuinterpose_gpu_tests::sys::{self, CUcontext, CUdeviceptr, CUresult};
use cuinterpose_gpu_tests::{cu, cuda};
use std::sync::Barrier;

fn malloc() -> CUdeviceptr {
    let mut address = 0;
    cu!(cuMemAlloc_v2(&mut address, 4));
    address
}

fn zero(address: CUdeviceptr) {
    cuda::write_bytes(address, &0u32.to_le_bytes());
}

/// Initializes CUDA and creates a context on each GPU.
fn created_contexts() -> [CUcontext; 2] {
    cu!(cuInit(0));
    [0, 1].map(|device| {
        let mut context = std::ptr::null_mut();
        cu!(cuCtxCreate_v4(
            &mut context,
            std::ptr::null_mut(),
            0,
            device
        ));
        context
    })
}

#[test]
fn cudart_peer_enable_grants_access_to_existing_mallocs() {
    run(|| {
        cuda::primary_contexts(1);
        let address = malloc();
        zero(address);
        // PyTorch enables peer access through cudart, which must dispatch into the shim.
        assert_eq!(unsafe { runtime::cudaSetDevice(1) }, cudaError::cudaSuccess);
        assert_eq!(
            unsafe { runtime::cudaDeviceEnablePeerAccess(0, 0) },
            cudaError::cudaSuccess
        );
        let mut device = 0;
        cu!(cuCtxGetDevice(&mut device));
        assert_eq!(device, 1, "the increment would not run on GPU 1");
        cuda::increment(address);
        assert_eq!(cuda::read_u32(address), 1, "runtime peer access");
    });
}

#[test]
fn mallocs_after_failed_or_disabled_peer_enable_get_no_peer_access() {
    run(|| {
        let contexts = created_contexts();
        cu!(cuCtxSetCurrent(contexts[1]));
        let status = unsafe { sys::cuCtxEnablePeerAccess(contexts[0], 1) };
        assert_eq!(status, CUresult::CUDA_ERROR_INVALID_VALUE);
        cu!(cuCtxSetCurrent(contexts[0]));
        let after_failure = malloc();
        assert_eq!(cuda::get_access(after_failure, 1), cuda::NONE);
        cu!(cuCtxSetCurrent(contexts[1]));
        cu!(cuCtxEnablePeerAccess(contexts[0], 0));
        cu!(cuCtxDisablePeerAccess(contexts[0]));
        cu!(cuCtxSetCurrent(contexts[0]));
        let after_disable = malloc();
        assert_eq!(cuda::get_access(after_disable, 1), cuda::NONE);
        zero(after_disable);
        cu!(cuCtxSetCurrent(contexts[1]));
        cu!(cuCtxEnablePeerAccess(contexts[0], 0));
        cuda::increment(after_disable);
        assert_eq!(cuda::read_u32(after_disable), 1, "re-enabled peer access");
    });
}

#[test]
fn mallocs_after_peer_context_destroyed_get_no_peer_access() {
    run(|| {
        let contexts = created_contexts();
        cu!(cuCtxSetCurrent(contexts[1]));
        cu!(cuCtxEnablePeerAccess(contexts[0], 0));
        cu!(cuCtxDestroy_v2(contexts[1]));
        cu!(cuCtxSetCurrent(contexts[0]));
        let address = malloc();
        assert_eq!(cuda::get_access(address, 1), cuda::NONE);
    });
}

#[test]
fn mallocs_concurrent_with_peer_enable_get_peer_access() {
    run(|| {
        let contexts = cuda::primary_contexts(2);
        let barrier = Barrier::new(2);
        // Threads cannot share raw context pointers, so the allocating thread gets an integer.
        let owner = contexts[0] as usize;
        let addresses = std::thread::scope(|scope| {
            let allocations = scope.spawn(|| {
                cu!(cuCtxSetCurrent(owner as CUcontext));
                barrier.wait();
                (0..20)
                    .map(|_| {
                        let address = malloc();
                        zero(address);
                        address
                    })
                    .collect::<Vec<_>>()
            });
            cu!(cuCtxSetCurrent(contexts[1]));
            barrier.wait();
            cu!(cuCtxEnablePeerAccess(contexts[0], 0));
            allocations.join().unwrap()
        });
        for address in addresses {
            cuda::increment(address);
            assert_eq!(cuda::read_u32(address), 1, "malloc racing peer enable");
        }
    });
}
