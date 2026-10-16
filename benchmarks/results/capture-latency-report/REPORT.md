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
- **PageBroker's cost is concentrated** in the staging write rate and a 20 s publish, both on an Oct 5 build that HEAD has since changed.

## 9. Next steps

**Implement**
- **Take the reap off the critical path.** Either patch CRIU to write stats and return before `wait4`, or keep the cgroup frozen, dump with leave-running, mark the snapshot ready, then SIGKILL and reap asynchronously. The second option needs validation that the source cannot run between unseize and kill.
- **Size the agent for buffered writes** (or default to direct). A 4Gi limit costs writeback ~9 s at 14B.

**Explore**
- **Page size of the engine's memory.** Check `AnonHugePages` and pinned host buffers. 4 KiB pages make teardown far costlier than 2 MiB pages, so THP could shrink the tail even if the reap stays synchronous.
- **GPU release timing.** Check whether the GPU stays held until the reap finishes, which would also delay the next pod.
- **PageBroker on HEAD.** Parallel publish and PVC-direct restore should change both of its costs.
- **Restore-side breakdown.** Apply the same `/proc` sampler to `criu_restore` (~9 s) and `cuda_restore` (~13 s) at 14B, which remain unexplained.

## 10. Caveats

- Leave-running is **not a supported mode**: a CUDA-locked source is never resumed, and the operator expects the source to die. Its restore succeeded only because the benchmark deletes the source pod before restoring.
- Root-cause probes are n=1 per configuration. The 8 s → 0.01 s delta is far outside the ~±0.3 s run-to-run spread.
- PageBroker figures reflect the Oct 5 (`a3cb9545`) build, not HEAD.
- Nothing was committed. The diagnostic binaries remain on the PVC (`/checkpoints/dev/snapshot-agent-{control,leaverunning}`), and the agent runs the original build.

---

**Data.** `benchmarks/results/`: `20261014-*`, `20261015-*`, `20261016-*`, `tail-probe/`, `leaverunning-control/`, `leaverunning-test/`, each with an `ARM.txt`. Sampler logs sit next to the probe results. Regenerate the plots with `uv run --no-project --with matplotlib python make_plots.py`.
