# Scale-dependent CRIU capture latency: where the time goes

**Bottom line.** About 40% of a Qwen3 14B capture is not capture. After every byte of the image is written, CRIU kills the source process and blocks in `wait4` for ~8 s while the kernel tears down the dead process's 47 GiB address space on a single core. The cost is identical on every storage path. A diagnostic build that skips the kill cut `criu_dump` from 19.77 s to 11.42 s, with the same bytes written at the same rate.

| Headline | Value |
|---|---|
| 14B post-write tail, kill vs leave-running | 7.96 s → 0.01 s |
| 14B `criu_dump`, kill vs leave-running | 19.77 s → 11.42 s (−42%) |
| Tail growth with image size | ~0.15 s/GB, linear across 5 sizes |
| 14B writeback penalty vs direct, 4Gi → 96Gi agent limit | 11.6 s → 3.1 s |

---

## 1. Context and aim

Capture latency grows with model size (tracked in the existing issue *Reduce scale-dependent CRIU capture latency*). The aim was **not** to pick a winning path but to locate the bottleneck inside each existing path, so optimization effort lands where the time actually goes.

**Questions**
- Which phases of the dump scale with model size?
- Do image I/O mode (direct vs writeback) or PageBroker staging change those phases?
- What is the ~8 s post-write tail that no storage setting moves?

**Hypothesis (tested in §6).** The tail is CRIU waiting on the kernel to tear down the dumped process, not CRIU's own cleanup or image flushing.

## 2. Setup

| Item | Value |
|---|---|
| Cluster | `dynamo-aws-dev-02` → vCluster `shsriram-dev`, one GPU node, workloads pinned to `ip-100-66-183-52` |
| Agent | `snapshot-agent` at `a3cb9545`, privileged, hostPID, CRIU `logLevel: 4` |
| Workload | `vllm/vllm-openai:v0.27.1-ubuntu2404` (digest-pinned), `max_model_len=2048`, `gpu_memory_utilization=0.50` |
| Models | Qwen3 0.6B and 14B for the path matrix; 0.6B, 1.7B, 4B, 8B, 14B for the scaling sweep |
| Repeats | 3 per model per arm; root-cause probes n=1 each, checked against arm medians |
| Held fixed | `autoDedup` off, `lazyPages` off, leave-running off (except one diagnostic build) |
| Restore validity | Readiness requires the restored engine to wake, resume, health-check, generate text and start serving |

### Knobs varied

| Knob | Values | Where it lives | What it changes |
|---|---|---|---|
| `imageIoMode` | direct, writeback | `snapshot-config` ConfigMap | O_DIRECT image writes vs buffered through page cache |
| PageBroker | off, on | config `pageBroker.enabled` **and** pod annotation `nvidia.com/snapshot-pagebroker=true` | Stage the image in a memory emptyDir, then publish to the PVC |
| Agent memory limit | 4Gi, 96Gi | DaemonSet container limits | Page-cache budget for buffered writes |
| `LeaveRunning` (diagnostic only) | false, true | Hardcoded at `agent/internal/criu/dump.go:56` | Whether CRIU kills and reaps the source at the end of the dump |

## 3. How to read the results

A 14B dump on the shipped path, from full-debug `dump.log` timestamps:

```
0 s           0.46 s                                     11.80 s                      19.76 s
|-- freeze --|============ page write (O_DIRECT) ==========|---- kill -> wait4 reap ----|
 Dumping task                                     Unfreezing tasks into 2        Writing stats
```

| Metric | Meaning | Source |
|---|---|---|
| `criu_dump` | Wall time of the CRIU dump RPC as seen by the agent | Agent log phase |
| Write phase (to 99%) | Time until the image directory reaches 99% of final size | Image-growth sampler, ~200 ms |
| Write rate | Image bytes ÷ write phase | Derived |
| Post-write tail | `Unfreezing tasks` → `Writing stats`: image complete, CRIU not yet returned | `dump.log` markers |
| Commit | PageBroker publish from staging to the PVC (a rename when PageBroker is off) | Agent log phase |
| Restore total | Restore pod created → readiness probe passes | Benchmark timestamps |

## 4. Results by path

![14B capture time by path](1-phases-14b.png)

Medians of 3 runs:

| Arm | 0.6B criu | 0.6B rate | 14B criu | 14B write | 14B rate | 14B tail | 14B commit | 14B restore |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **direct · 96Gi** | 4.72 s | 2.19 GB/s | **19.50 s** | 10.8 s | 4.74 GB/s | 8.65 s | rename | 36 s |
| direct · 4Gi | 5.20 s | 2.06 GB/s | 20.18 s | 11.2 s | 4.57 GB/s | 8.81 s | rename | — |
| writeback · 96Gi | 4.77 s | 2.19 GB/s | 22.58 s | 13.8 s | 3.71 GB/s | 8.61 s | rename | 36 s |
| writeback · 4Gi | 8.12 s | 1.13 GB/s | 31.80 s | 22.8 s | 2.24 GB/s | 8.82 s | rename | — |
| PageBroker · direct | 7.01 s | 1.38 GB/s | 35.36 s | 26.6 s | 1.92 GB/s | 8.76 s | 20.2 s | 64 s |
| PageBroker · writeback | 7.01 s | 1.33 GB/s | 35.50 s | 26.4 s | 1.94 GB/s | 8.72 s | 20.2 s | 63 s |

The tail column comes from image-growth sampling; the `dump.log` equivalent is 7.95–8.00 s on every non-broker arm. Restore totals for the 4Gi arms predate the current restore flow and are not comparable.

## 5. Trends

![Tail vs image size](2-tail-vs-size.png)

- **The tail is flat across every path** (8.6–8.8 s at 14B) and grows linearly with size at ~0.15 s/GB. No storage setting moves it.
- **Direct I/O is limit-insensitive**: ~4.7 GB/s at 14B whether the agent has 4Gi or 96Gi.
- **Writeback is limit-sensitive**: 2.24 GB/s at 4Gi vs 3.71 GB/s at 96Gi. Buffered writes charge page cache to the agent cgroup, and a 4Gi limit forces reclaim throughout a 51 GB dump.
- **PageBroker roughly halves the write rate** (staging into the memory emptyDir is slower than O_DIRECT to the PVC on this node), then adds a ~20 s publish. A brokered 14B capture is ~55 s end to end, and its restore is ~64 s vs 36 s.
- **Small models write at ~2 GB/s on every path**: below ~10 GB, fixed per-dump overhead dominates.

## 6. Root cause of the tail

A sampler inside the agent read CRIU's and every dumped task's state, CPU time, kernel stack and `/proc/<pid>/io` every ~100 ms during a 14B dump.

![Tail probe](3-tail-probe.png)

**What the sampler shows**
- **CRIU is idle**: 0.00 s of CPU for the whole tail, kernel stack `do_wait → kernel_wait4` in 56 of 56 samples.
- **The killed engine process does the work**: 47.3 GiB RSS, 333 threads → a zombie with 2 threads left, burning 7.3–7.7 s of kernel CPU at 99% of exactly one core.
- The full-debug `dump.log` has **zero lines** between unseizing the last task and `Writing stats`.
- Same pattern at 0.6B: 0.68 s of kernel CPU in a 6.5 GiB engine while CRIU idles.
- The teardown rate of ~6 GiB/s reproduces the ~0.15 s/GB scaling slope.

**Causal test: remove the kill.** Two binaries were built from `a3cb9545`, differing only in `dump.go:56`.

![Leave-running A/B](4-leaverunning.png)

| 14B, direct, 96Gi | Control build | `LeaveRunning=true` |
|---|---:|---:|
| Bytes written | 47.67 GiB | 47.66 GiB |
| Write phase | 11.56 s | 10.83 s |
| Tail (`dump.log`) | 7.96 s | **0.01 s** |
| `criu_dump` | 19.77 s | **11.42 s** |
| Zombie tasks | all three | none |

The control build reproduces the deployed binary (19.77 s vs the 19.50 s three-run median), so the swap isn't a confound.

**Not proven:** which kernel function does the work. A zombie's stack isn't readable, so the evidence is the amount, location and scaling of the CPU. That fits address-space teardown but does not name a frame.

## 7. Anomalies and confounds found along the way

| Finding | Effect | Status |
|---|---|---|
| Agent capped at 4Gi in the first two arms, 96Gi in the PageBroker arms | Inflated the 14B writeback penalty from 3.1 s to 11.6 s | Both arms rerun at 96Gi |
| PageBroker needs a pod annotation as well as the config flag | The first "PageBroker on" arm silently ran unbrokered | Benchmark now annotates; arm relabeled |
| Stock `ns-bind-mount` lacks the `mount-pagebroker-fd` role | Every PageBroker restore failed | Helper rebuilt from `a3cb9545` |
| `exec` without `container=` on the two-container agent pod | HTTP 400; `dump.log` and artifact bytes missing from PageBroker results | Fixed in `run.py` |
| PageBroker restore ~64 s vs 36 s | Restore always stages through memory first (~21 s stage) | Oct 5 build; HEAD restores straight from the PVC |
| One 14B writeback restore took 1677 s | A single outlier | Excluded; did not reproduce |

## 8. Promising highlights

- **~8 s (40%) of every 14B capture is removable without touching storage.** The image is complete and closed before the reap begins.
- **Direct I/O is the strongest storage path** and is robust to agent memory sizing.
- **Writeback is within ~3 s of direct** once the agent's memory limit doesn't throttle page cache.
- **PageBroker's cost is concentrated** in the staging write rate and a ~20 s publish on the Oct 5 build. On HEAD, restore no longer copies through staging; capture publish is still a copy until the agent uses `PrepareDirectCheckpoint` (§10).

## 9. Next steps

**Implement**
- **Take the reap off the critical path.** Either patch CRIU to write stats and return before `wait4`, or keep the cgroup frozen, dump with leave-running, mark the snapshot ready, then SIGKILL and reap asynchronously. The second option needs validation that the source cannot run between unseize and kill.
- **Size the agent for buffered writes** (or default to direct). A 4Gi limit costs writeback ~9 s at 14B.

**Explore** — results are in §10.
- Page size, GPU release, and restore split: done on the Oct 5 agent.
- Matched HEAD deploy (`e73a6112`) and direct checkpoint publish: done in §10.
- Deeper profiling inside `cuda_restore` (~12–14 s): still open; the explore sampler was too slow.

## 10. Explore results

Three of the four items, from one 14B capture on the same direct / 96Gi / PageBroker-off setup. Raw logs are in `benchmarks/results/explore-probe/` (local, not committed).

### Page size: the 45 GiB is 4 KiB memfd, not THP and not pinned

Census of `VLLM::EngineCor` (333 threads), taken while it was running and confirmed on the restored process:

| Field | Value |
|---|---:|
| VmRSS | 45.1 GiB |
| RssShmem | 42.6 GiB |
| RssAnon | 1.8 GiB |
| RssFile | 0.7 GiB |
| AnonHugePages | 0 |
| HugetlbPages | 0 |
| VmLck / Locked | 16 KiB |
| VmPTE | 95 MiB |
| memfd mappings | 179, RSS = size, huge pages 0 |

The resident set is 179 `memfd:/dev/zero` mappings. Host THP is `madvise` for anonymous memory and `never` for shmem, and nothing in this process opted in: no huge mappings at all. The 95 MiB page tables match 4 KiB PTEs for that shmem (42.6 GiB / 4 KiB × 8 B ≈ 87 MiB, plus the rest of the address space). Locked memory is 16 KiB, so pinned host buffers are not the tail.

Turning on anonymous THP would not touch this. The pages that get torn down are shmem. Shrinking the tail without moving the reap means shmem THP or a 2 MiB memfd, and that is an application change, not a CRIU flag.

### GPU release: the reap does not hold the device

`nvidia-smi` index order inside the agent container moves between samples, so this is by UUID. The capture's GPU was `GPU-47409b11` (the CDI path the benchmark later bind-mounted for restore). sglang's two GPUs stayed at 70436 MiB the whole time.

| Moment | Engine state | `GPU-47409b11` used |
|---|---|---:|
| RSS crossed 25 GiB, 30 s before freeze | running | 26569 MiB, falling |
| 3 s before freeze | sleeping | 0 |
| CRIU freeze (`t`) through the zombie window | traced, then zombie | 0 |
| After restore, different GPU `GPU-2efb04e8` | restored engine | 40023 MiB |

The device is already free before CRIU freezes the tasks, so the 8 s reap does not delay the next pod. One `cuda-checkpoint-helper` sample during the drain was blocked in `anon_pipe_read`; the agent's own `cuda_checkpoint` phase on these runs is 3.5–3.9 s and finishes before `criu_dump`. The drain itself took longer than that phase, so this does not pin the whole drop on the helper.

### Restore split: reading the image, then CUDA

Agent restore summaries, PageBroker off, two 14B runs:

| Phase | Run 1 | Run 2 |
|---|---:|---:|
| `criu_restore` | 7.64 s | 8.74 s |
| `cuda_restore` | 12.56 s | 13.98 s |
| everything else | < 0.3 s | < 0.3 s |
| restore total | 20.5 s | 23.0 s |

`cuda_restore` is `RestoreAndUnlockProcessTree`: for each CUDA pid, `cuda-checkpoint-helper` restores, then unlocks, one pid at a time. The sampler was too slow (a `nvidia-smi` per sample) to profile those 14 s. The one useful restore sample is CRIU at the end of its read: 43.7 GiB already read, stack in `io_destroy`. So the ~8 s is reading the image back, and the ~14 s is the helper putting CUDA state back. Neither is broken down further yet.

### HEAD stack (`e73a6112`): matched deploy, direct restore, staged capture publish

Operator, agent, and PageBroker were upgraded together to git `e73a6112` on the same node (`imageIoMode: direct`, `pageBroker.restoreMode: direct`, agent memory 96Gi). PageBroker is mandatory on this revision; the pod annotation from the benchmark is harmless.

**Restore (n=1, Qwen3 14B).** PageBroker stage, mount, and commit are all under 2 ms. The time is CRIU read plus CUDA helper, same split as the Oct 5 direct path but faster end to end because nothing copies the image into the staging emptyDir first.

| Phase | Oct 5 direct (§10) | HEAD `e73a6112` |
|---|---:|---:|
| `criu_restore` | 7.6–8.7 s | 7.7 s |
| `cuda_restore` | 12.6–14.0 s | 12.5 s |
| PageBroker stage + mount + commit | n/a (broker off) | < 0.002 s |
| Agent restore total | 20.5–23.0 s | 20.4 s |
| Pod Ready (benchmark) | — | 25 s |

**Capture with default prepare (still staged publish).** One 14B run on the stock `e73a6112` agent before the direct-prepare patch:

| Phase | Oct 5 direct median (§4) | HEAD staged publish |
|---|---:|---:|
| `cuda_checkpoint` | ~3.5 s | 3.6 s |
| `criu_dump` | 19.5 s | 34.8 s |
| `remove_old_version_and_switch` (publish copy) | rename | 18.8 s |
| Checkpoint total | 19.5 s `criu_dump` wall* | 57.7 s |

\*Oct 5 table uses agent `criu_dump` phase only; HEAD rows use the agent checkpoint summary duration (57.7 s) for the staged run.

CRIU on HEAD was writing the memory staging directory, so the dump phase itself is slower than O_DIRECT to the PVC on the Oct 5 path, and commit still paid a single-threaded copy (~2.5 GB/s) like the old PageBroker publish.

### Direct checkpoint publish (`PrepareDirectCheckpoint`)

The PageBroker API already supported writing beside the destination and promoting with a rename; the agent still called `PrepareCheckpoint` (staged). Wiring `prepareCheckpoint` to `PrepareDirectCheckpoint` for non-GPU-custom-storage captures removes the publish copy. Deployed as image tag `e73a6112-direct` (same tree as `e73a6112` plus this one-line call-site change). One full 14B capture and restore (run `0ce4d248b731`, result under `benchmarks/results/20261019-2f70fe3d-0c7df784/`):

| Phase | HEAD staged publish | HEAD direct publish |
|---|---:|---:|
| `cuda_checkpoint` | 3.6 s | 2.6 s |
| `criu_dump` | 34.8 s | 21.2 s |
| `remove_old_version_and_switch` | 18.8 s | **0.001 s** |
| Checkpoint total | 57.7 s | **24.2 s** |
| Restore (agent) | 22.6 s | 20.4 s |

Artifact size unchanged (~47.7 GiB). The ~8 s post-write tail was not remeasured on HEAD; nothing in the direct-prepare change moves kernel reap off the critical path.

**Benchmark changes for HEAD restores.** The NRI plugin only installs `snapshot-restore-proxy` when `nvidia.com/restore-from` is present at container creation, so the benchmark no longer patches that annotation in after start. The compatibility gate still requires the source GPU CDI path (`/run/nvidia-container-devices/GPU-<uuid>`) to exist in the restore container; a privileged agent watcher creates it as soon as the proxy process appears.

## 11. Caveats

- Leave-running is **not a supported mode**: a CUDA-locked source is never resumed, and the operator expects the source to die. Its restore succeeded only because the benchmark deletes the source pod before restoring.
- Root-cause probes are n=1 per configuration. The 8 s → 0.01 s delta is far outside the ~±0.3 s run-to-run spread.
- §4 path matrix and tail probes are on the Oct 5 (`a3cb9545`) agent. §10 HEAD numbers are n=1 each unless noted.
- Diagnostic binaries may remain on the PVC (`/checkpoints/dev/snapshot-agent-{control,leaverunning}`); production pods use the container image entrypoint, not those paths.

---

**Data.** `benchmarks/results/`: `20261014-*`, `20261015-*`, `20261016-*`, `20261019-*` (HEAD direct run JSON only in gitignored trees except this report), `tail-probe/`, `leaverunning-control/`, `leaverunning-test/`, each with an `ARM.txt` where applicable. Sampler logs sit next to the probe results. Regenerate the plots with `uv run --no-project --with matplotlib python make_plots.py`.
