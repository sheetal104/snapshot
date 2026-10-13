// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES.
// SPDX-License-Identifier: Apache-2.0

//! Multicast bindings through a native checkpoint, and rejected V1 bindings.

use cuinterpose_gpu_tests::process::{self, channel, run, spawn};
use cuinterpose_gpu_tests::sys::{self, CUdeviceptr, CUmemAllocationType, CUresult};
use cuinterpose_gpu_tests::{cu, cuda, shim};
use cuinterpose_protocol::{BindingSource, BindingVersion, MemberRange, Record};
use std::collections::BTreeMap;
use std::ffi::CStr;

const MEMBER_VALUES: [u32; 2] = [0x1234_5678, 0x1111_1111];

fn pid() -> u32 {
    std::process::id()
}

#[derive(Clone, Copy)]
enum Source {
    /// cuMulticastBindMem with a tracked VMM member.
    Memory,
    /// cuMulticastBindAddr with untracked pool memory.
    Address,
}

#[test]
fn bind_mem_restores_through_native_checkpoint() {
    bindings_restore_through_native_checkpoint(Source::Memory, BindingVersion::V1);
}

#[test]
fn bind_mem_v2_restores_through_native_checkpoint() {
    bindings_restore_through_native_checkpoint(Source::Memory, BindingVersion::V2);
}

#[test]
fn bind_addr_restores_through_native_checkpoint() {
    bindings_restore_through_native_checkpoint(Source::Address, BindingVersion::V1);
}

#[test]
fn bind_addr_v2_restores_through_native_checkpoint() {
    bindings_restore_through_native_checkpoint(Source::Address, BindingVersion::V2);
}

fn bindings_restore_through_native_checkpoint(source: Source, version: BindingVersion) {
    if process::role().as_deref() == Some("bound") {
        return bound_multicast(source, version);
    }
    shim::require_multicast();
    let child = spawn("bound");
    assert_eq!(child.recv::<String>(), "ready");
    shim::checkpoint(&[child.pid()], true);
    child.send(&"continue");
    child.wait();
}

/// Binds one member per GPU and reads their sum through the multicast mapping before
/// and after the checkpoint.
fn bound_multicast(source: Source, version: BindingVersion) {
    let contexts = cuda::primary_contexts(2);
    let (group, size) = cuda::multicast_group(&[0, 1]);
    let (mut members, mut addresses) = (Vec::new(), Vec::new());
    for (device, value) in (0..).zip(MEMBER_VALUES) {
        cu!(cuCtxSetCurrent(contexts[device as usize]));
        let (member, address) = match source {
            Source::Memory => {
                let properties = cuda::allocation_properties(device, false);
                let (member, address, _) = cuda::create_mapped(&properties, Some(size));
                members.push((member, address));
                (member, address)
            }
            // The shim does not track pool memory. CUDA restores it natively.
            Source::Address => (0, pool_address(device, size)),
        };
        addresses.push(address);
        cu!(cuMemsetD32_v2(address, value, size / 4));
        cu!(cuCtxSynchronize());
        match (source, version) {
            (Source::Memory, BindingVersion::V1) => {
                cu!(cuMulticastBindMem(group, 0, member, 0, size, 0))
            }
            (Source::Memory, BindingVersion::V2) => {
                cu!(cuMulticastBindMem_v2(group, device, 0, member, 0, size, 0))
            }
            (Source::Address, BindingVersion::V1) => {
                cu!(cuMulticastBindAddr(group, 0, address, size, 0))
            }
            (Source::Address, BindingVersion::V2) => {
                cu!(cuMulticastBindAddr_v2(group, device, 0, address, size, 0))
            }
        }
    }
    let records = shim::records(pid());
    let expected: BTreeMap<_, _> = match source {
        Source::Memory => {
            let tracked: BTreeMap<_, _> = records
                .iter()
                .filter_map(|record| match record {
                    Record::Allocation {
                        allocation,
                        location,
                        ..
                    } => Some((location.id, *allocation)),
                    _ => None,
                })
                .collect();
            let member = |device| MemberRange {
                allocation: tracked[&device],
                offset: 0,
            };
            (0..2)
                .map(|device| (device, BindingSource::Memory(member(device))))
                .collect()
        }
        Source::Address => (0..)
            .zip(addresses)
            .map(|(device, address)| {
                let source = BindingSource::Address {
                    address,
                    tracked_member: None,
                };
                (device, source)
            })
            .collect(),
    };
    let bindings = multicast_bindings(&records);
    assert_eq!(bindings.len(), 2, "{bindings:?}");
    let sources: BTreeMap<_, _> = bindings
        .iter()
        .filter_map(|binding| match binding {
            Record::MulticastBinding { device, source, .. } => Some((*device, *source)),
            _ => None,
        })
        .collect();
    assert_eq!(sources, expected, "{bindings:?}");
    assert!(
        bindings.iter().all(|binding| matches!(
            binding,
            Record::MulticastBinding { version: recorded, .. } if *recorded == version
        )),
        "{bindings:?}"
    );
    cu!(cuCtxSetCurrent(contexts[0]));
    let multicast_address = cuda::map(group, size, cuda::device(0));
    let sum = MEMBER_VALUES.iter().sum();
    assert_eq!(multimem_sum(multicast_address), sum);
    channel().send(&"ready");
    assert_eq!(channel().recv::<String>(), "continue");
    assert_eq!(multicast_bindings(&shim::records(pid())), bindings);
    assert_eq!(multimem_sum(multicast_address), sum);
    // Cleanup after restore checks the restored bindings, mappings and handles.
    for device in 0..2 {
        cu!(cuMulticastUnbind(group, device, 0, size));
    }
    assert_eq!(multicast_bindings(&shim::records(pid())), []);
    for (member, address) in members {
        cuda::unmap(address, size);
        cu!(cuMemRelease(member));
    }
    cuda::unmap(multicast_address, size);
    cu!(cuMemRelease(group));
}

fn multicast_bindings(records: &[Record]) -> Vec<Record> {
    records
        .iter()
        .filter(|record| matches!(record, Record::MulticastBinding { .. }))
        .cloned()
        .collect()
}

#[test]
fn bind_mem_v1_rejects_host_numa_member_without_recording() {
    shim::require_multicast();
    shim::require_host_numa();
    run(|| {
        cuda::primary_contexts(1);
        let (group, size) = cuda::multicast_group(&[0, 1]);
        let properties = cuda::allocation_properties(0, true);
        // A smaller member would be rejected for its size before its location is checked.
        let member_size = size.next_multiple_of(cuda::granularity(&properties));
        let mut member = 0;
        cu!(cuMemCreate(&mut member, member_size, &properties, 0));
        let before = shim::records(pid());
        // cuMulticastBindMem binds a member on the GPU it resides on, which a host member
        // lacks.
        let status = unsafe { sys::cuMulticastBindMem(group, 0, member, 0, size, 0) };
        assert_eq!(status, CUresult::CUDA_ERROR_NOT_SUPPORTED);
        assert_eq!(
            shim::records(pid()),
            before,
            "rejected HOST_NUMA bind changed tracked state"
        );
    });
}

/// A size-aligned address in a native stream-ordered pool on `device`.
fn pool_address(device: i32, size: usize) -> CUdeviceptr {
    // SAFETY: CUmemPoolProps is plain data, and zero is the default of each field.
    let mut properties: sys::CUmemPoolProps = unsafe { std::mem::zeroed() };
    properties.allocType = CUmemAllocationType::CU_MEM_ALLOCATION_TYPE_PINNED;
    properties.handleTypes = cuda::POSIX_FD;
    properties.location = cuda::device(device);
    let mut pool = std::ptr::null_mut();
    cu!(cuMemPoolCreate(&mut pool, &properties));
    let mut stream = std::ptr::null_mut();
    cu!(cuStreamCreate(&mut stream, 0));
    let mut base = 0;
    cu!(cuMemAllocFromPoolAsync(&mut base, 2 * size, pool, stream));
    cu!(cuStreamSynchronize(stream));
    base.next_multiple_of(size as u64)
}

// Ordinary loads from a multicast address are undefined, so read through multimem.
const MULTIMEM_SUM: &CStr = c"
.version 8.1
.target sm_90
.address_size 64
.visible .entry multimem_sum(.param .u64 source, .param .u64 destination) {
    .reg .b64 %source, %destination;
    .reg .b32 %value;
    ld.param.u64 %source, [source];
    ld.param.u64 %destination, [destination];
    multimem.ld_reduce.acquire.sys.global.add.u32 %value, [%source];
    st.global.u32 [%destination], %value;
    ret;
}
";

/// The sum over every bound member of the u32 at a multicast address.
fn multimem_sum(address: CUdeviceptr) -> u32 {
    let mut output = 0;
    cu!(cuMemAlloc_v2(&mut output, 4));
    cuda::launch(MULTIMEM_SUM, c"multimem_sum", &[address, output]);
    let sum = cuda::read_u32(output);
    cu!(cuMemFree_v2(output));
    sum
}
