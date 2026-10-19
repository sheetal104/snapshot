# Scale-dependent CRIU capture latency: where the time goes

**Bottom line (capture).** The data is enough to justify work on *scale-dependent CRIU capture latency*: latency grows with model size, the dominant delays are named, and at least two are fixable in product (not “CRIU is slow” in the abstract).

On the best **matrix** path (agent git **`a3cb9545`**, direct I/O, 96 Gi agent, PageBroker off), a 14B capture spends **~19.5 s in `criu_dump`**, split into **~11 s writing the image** and **~8 s after the image is complete**. That post-write block is **kernel reap of the killed engine** (`wait4`), identical on every storage arm and scaling **~0.15 s/GB**. A diagnostic build that skips the kill cut **`criu_dump` from 19.77 s to 11.42 s** with the same bytes and write rate — so **~8 s (~40%) is removable** without faster disks.

On **upstream `e73a6112`** (matched operator/agent/PageBroker upgrade), the stock agent still published through a **staged copy (~19 s)** until wired to **`PrepareDirectCheckpoint`** (**`e73a6112-direct`** image, commit `485f2671`). That drops publish to a rename and cuts **checkpoint wall time from 57.7 s to 24.2 s** (single run `0ce4d248b731`). After the patch, **~21 s `criu_dump` + ~3 s CUDA** remain; the reap tail was not remeasured on `e73a6112` but nothing in direct prepare moves it off the critical path.

| Headline | Value |
|---|---|
| 14B post-write tail, kill vs leave-running | 7.96 s → 0.01 s |
| 14B `criu_dump`, kill vs leave-running | 19.77 s → 11.42 s (−42%) |
| Tail growth with image size | ~0.15 s/GB, linear across 5 sizes |
| `e73a6112` capture: staged publish vs `e73a6112-direct` | 57.7 s → 24.2 s (publish 18.8 s → ~1 ms) |
| 14B writeback penalty vs direct, 4Gi → 96Gi agent limit | 11.6 s → 3.1 s |

---

## 1. Context and aim

Capture latency grows with model size (issue: *Reduce scale-dependent CRIU capture latency*). The aim was to locate **capture** bottlenecks inside each path so the issue can be prioritized on measured upside, not to pick a winning storage configuration.

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
| Repeats | 3 per model per arm on the matrix build; root-cause and HEAD follow-up runs n=1 unless noted |
| Held fixed | `autoDedup` off, `lazyPages` off, leave-running off (except one diagnostic build on `a3cb9545`) |

### Agent builds used in this report

External readers only need these names; sections below refer to them explicitly.

| Label | Git / image | What was deployed | Where it appears |
|---|---|---|---|
| **Matrix build** | Agent **`a3cb9545`** (fork branch `feat/capture-timing-breakdown`; chart **`snapshot-0.1.0`**, image tag `v0.1.0`) | Four-arm capture matrix, tail probes, leave-running A/B, scaling sweep. Runs **2026-10-14 – 2026-10-16** under `benchmarks/results/20261014-*` … `20261016-*`. | §4–§7 (table values = **median of 3 runs** per arm) |
| **Upstream HEAD** | **`e73a6112`** (`origin/main` at upgrade time) | Matched **`ghcr.io/ai-dynamo/snapshot/{operator,agent,pagebroker}:e73a6112`**, Helm revision 5, node `ip-100-66-183-52`. PageBroker mandatory; `restoreMode: direct`, `imageIoMode: direct`, agent limit 96 Gi. Stock agent still calls **`PrepareCheckpoint` (staged publish)**. | §9 column “`e73a6112` staged prepare” (n=1, run **`94e8c6e09e71`**, capture 57.7 s) |
| **HEAD + direct prepare** | **`e73a6112-direct`** (same tree as `e73a6112` + `prepareCheckpoint` → `PrepareDirectCheckpoint`, on branch `exp/criu-capture-latency` commit **`485f2671`**) | Local image imported to node containerd; DaemonSet agent tag `e73a6112-direct`. | §9 column “`e73a6112-direct`” (n=1, run **`0ce4d248b731`**, capture 24.2 s) |

Benchmark: `snapshot_benchmarks run` in namespace **`shsriram-dev`**, PVC **`snapshot-pvc`**, engine image digest-pinned (see table above). Result JSON for HEAD runs: `benchmarks/results/20261019-2f70fe3d-0c7df784/` (gitignored except this report).

### Knobs varied

| Knob | Values | Where it lives | What it changes |
|---|---|---|---|
| `imageIoMode` | direct, writeback | `snapshot-config` ConfigMap | O_DIRECT image writes vs buffered through page cache |
| PageBroker | off, on | config `pageBroker.enabled` **and** pod annotation `nvidia.com/snapshot-pagebroker=true` | Stage the image in a memory emptyDir, then publish to the PVC |
| Agent memory limit | 4Gi, 96Gi | DaemonSet container limits | Page-cache budget for buffered writes |
| `LeaveRunning` (diagnostic only) | false, true | Hardcoded at `agent/internal/criu/dump.go:56` | Whether CRIU kills and reaps the source at the end of the dump |

## 3. How to read the results

A 14B dump on the **matrix build · direct · 96 Gi** arm, from full-debug `dump.log` timestamps:

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
| Commit / `remove_old_version_and_switch` | Publish from staging to PVC, or rename when writing beside destination | Agent log phase |
| Restore total | Restore pod created → readiness probe passes (benchmark); compare to capture wall time | Benchmark timestamps |

## 4. Results by path (capture vs restore)

![14B capture time by path](1-phases-14b.png)

**Agent git `a3cb9545`**, Helm release `snapshot`, namespace `shsriram-dev`. Values are **medians of three runs** per arm (2026-10-14 – 2026-10-16).

| Arm | 0.6B criu | 0.6B rate | 14B criu | 14B write | 14B rate | 14B tail | 14B commit | 14B restore |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **direct · 96Gi** | 4.72 s | 2.19 GB/s | **19.50 s** | 10.8 s | 4.74 GB/s | 8.65 s | rename | 36 s |
| direct · 4Gi | 5.20 s | 2.06 GB/s | 20.18 s | 11.2 s | 4.57 GB/s | 8.81 s | rename | — |
| writeback · 96Gi | 4.77 s | 2.19 GB/s | 22.58 s | 13.8 s | 3.71 GB/s | 8.61 s | rename | 36 s |
| writeback · 4Gi | 8.12 s | 1.13 GB/s | 31.80 s | 22.8 s | 2.24 GB/s | 8.82 s | rename | — |
| PageBroker · direct | 7.01 s | 1.38 GB/s | 35.36 s | 26.6 s | 1.92 GB/s | 8.76 s | 20.2 s | 64 s |
| PageBroker · writeback | 7.01 s | 1.33 GB/s | 35.50 s | 26.4 s | 1.94 GB/s | 8.72 s | 20.2 s | 63 s |

`14B criu` is agent `criu_dump` phase time (capture). `14B restore` is pod Ready after restore (includes vLLM wake), so it sits **above** agent restore phases — useful to see capture vs end-to-end restore scale, not to attribute restore bottlenecks (see §8). Tail column from image-growth sampling; `dump.log` equivalent 7.95–8.00 s on every non-broker arm. Restore totals for the 4Gi arms predate the current restore flow and are omitted.

## 5. Trends

![Tail vs image size](2-tail-vs-size.png)

- **The tail is flat across every path** (8.6–8.8 s at 14B) and grows linearly with size at ~0.15 s/GB. No storage setting moves it.
- **Direct I/O is limit-insensitive**: ~4.7 GB/s at 14B whether the agent has 4Gi or 96Gi.
- **Writeback is limit-sensitive**: 2.24 GB/s at 4Gi vs 3.71 GB/s at 96Gi. Buffered writes charge page cache to the agent cgroup, and a 4Gi limit forces reclaim throughout a 51 GB dump.
- **PageBroker roughly halves the write rate** on the matrix build (staging emptyDir vs O_DIRECT to PVC), then adds ~20 s publish. At 14B: capture ~35 s and restore ~64 s vs **direct · 96Gi** ~20 s / ~36 s — broker hurts both legs on `a3cb9545`.
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

The control build reproduces the matrix agent (19.77 s vs **19.50 s** median on **direct · 96Gi**, §4), so the swap isn't a confound.

**Not proven:** which kernel function does the work. A zombie's stack isn't readable, so the evidence is the amount, location and scaling of the CPU. That fits address-space teardown but does not name a frame.

## 7. Anomalies and confounds found along the way

| Finding | Effect | Status |
|---|---|---|
| Agent capped at 4Gi in the first two arms, 96Gi in the PageBroker arms | Inflated the 14B writeback penalty from 3.1 s to 11.6 s | Both arms rerun at 96Gi |
| PageBroker needs a pod annotation as well as the config flag | The first "PageBroker on" arm silently ran unbrokered | Benchmark now annotates; arm relabeled |
| Stock `ns-bind-mount` lacks the `mount-pagebroker-fd` role | Every PageBroker restore failed | Helper rebuilt from `a3cb9545` |
| `exec` without `container=` on the two-container agent pod | HTTP 400; `dump.log` and artifact bytes missing from PageBroker results | Fixed in `run.py` |
| One 14B writeback restore took 1677 s | Outlier during matrix | Excluded; capture arms unaffected |

## 8. Capture bottlenecks and what to do (issue scope)

This section is the capture-side case for prioritizing the issue. Restore is out of scope here.

| Bottleneck | ~14B cost (evidence) | Optimizable? | Recommended action |
|---|---|---|---|
| **Post-write kernel reap** | **~8 s**; ~0.15 s/GB; flat across storage (§4–§6) | **Yes — largest remaining win on direct path** | Return from capture before `wait4` / reap asynchronously; leave-running A/B proves bound (−8 s `criu_dump`) |
| **Staged publish copy** | **~19 s** on stock **`e73a6112`**; **~20 s** PageBroker commit on **`a3cb9545`** (§4, §9) | **Yes — wiring, not new I/O** | Ship `PrepareDirectCheckpoint` (done on `exp/criu-capture-latency`, commit **`485f2671`**) |
| **Image write (O_DIRECT)** | **~11 s** @ ~4.7 GB/s, matrix **direct · 96Gi** (§4) | Harder | Already near PVC throughput; wins need smaller images or faster storage, not more tuning on this node |
| **`cuda_checkpoint`** | **~2.5–3.6 s** (matrix §4; **`e73a6112-direct`** §9) | Secondary | Optimize after reap and publish |
| **Writeback + 4 Gi agent limit** | **+~9 s** vs direct at 14B (§4–§5) | Config | Default direct and/or size agent memory for buffered I/O |

**What not to chase for capture latency:** storage mode does not move the tail; GPU is already at 0 MiB before CRIU freeze (so the reap is not “holding the GPU” — it still blocks **snapshot ready**).

### Optional lever (workload, not snapshot)

Engine RSS is mostly **4 KiB shmem memfd** (179 mappings, no THP). That matches heavy PTE work during teardown (~95 MiB `VmPTE`). **Action only if targeting faster reap without async exit:** larger shmem pages or fewer mappings in vLLM — not a CRIU knob.

## 9. Upstream follow-up: `e73a6112` vs `e73a6112-direct`

Same cluster node and workload as §2. Upgrade details and run IDs are in **Agent builds used in this report**. Compared to the matrix **direct · 96Gi** column (median of 3 runs on **`a3cb9545`**).

| Phase | Matrix `a3cb9545` · direct · 96Gi (median, n=3) | `e73a6112` staged prepare (n=1, run `94e8c6e09e71`) | `e73a6112-direct` (n=1, run `0ce4d248b731`) |
|---|---:|---:|---:|
| `cuda_checkpoint` | ~3.5 s | 3.6 s | 2.6 s |
| `criu_dump` | 19.5 s | 34.8 s (CRIU wrote staging emptyDir) | 21.2 s (CRIU beside PVC destination) |
| Publish (`remove_old_version_and_switch`) | rename (~0 s) | 18.8 s copy | **~1 ms** rename |
| Checkpoint total (agent summary) | — | 57.7 s | **24.2 s** |
| Restore total (pod Ready, benchmark) | 36 s | 27 s | **25 s** |

Stock **`e73a6112`** still called **`PrepareCheckpoint` (staged)** although PageBroker already exposes **`PrepareDirectCheckpoint`**. The **`e73a6112-direct`** image only changes that call site (branch `exp/criu-capture-latency`, commit **`485f2671`**). After direct prepare, **~21 s `criu_dump` remains**, so the scale-dependent capture issue still applies. Post-write tail was not re-run on `e73a6112`; expect the same reap class as §6 (matrix **`a3cb9545`**) until measured otherwise.

## 10. Next steps (capture)

1. **Product:** reap off the critical path (CRIU or agent lifecycle) — validates issue goal at scale.
2. **Product:** merge and ship direct checkpoint prepare (already implemented on experiment branch).
3. **Policy:** direct I/O + agent memory sizing for any writeback deployment.
4. **Measure (optional):** repeat **`e73a6112-direct`** at 14B (n≥3) to firm medians; remeasure tail on `e73a6112` if stakeholders need parity with §6 (**`a3cb9545`**).

## 11. Caveats

- Leave-running is **diagnostic only** (proves the reap bound; not a supported production mode).
- Tail and leave-running probes are n=1; the 8 s → 0.01 s delta is far outside run-to-run spread on `criu_dump`.
- §4: agent git **`a3cb9545`**, three repeats per arm. §9: **`e73a6112`** and **`e73a6112-direct`**, one run each unless noted.
- Explore sampler logs live under `benchmarks/results/explore-probe/` (local, not committed).

---

**Data.** `benchmarks/results/`: `20261014-*` … `20261016-*` (matrix build **`a3cb9545`**), `20261019-*` (runs **`94e8c6e09e71`**, **`0ce4d248b731`** on **`e73a6112` / `e73a6112-direct`** — JSON gitignored except this report), `tail-probe/`, `leaverunning-control/`, `leaverunning-test/`, each with an `ARM.txt` where applicable. Sampler logs sit next to the probe results. Regenerate the plots with `uv run --no-project --with matplotlib python make_plots.py`.
