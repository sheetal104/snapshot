# CRIU capture latency: where the time goes

Notes from local runs on one GPU node with vLLM (Qwen3). Goal: see **where capture seconds go** and **what might be worth optimizing**, not pick a single “best” storage setup.

**Issue:** Reduce scale-dependent CRIU capture latency.

---

## At a glance (14B model)

| What we measured | Result |
|---|---|
| Typical capture on the fast baseline path | **~19.5 s** (median of 3 runs) |
| Time writing the checkpoint image | **~11 s** |
| Time after the image is finished | **~8 s** (did not change when we changed disk settings) |
| That tail vs checkpoint size | **~0.15 s per GB** across 0.6B–14B |
| Skip killing the engine (diagnostic only) | Tail **8 s → 0 s**, total capture **20 s → 11 s** |
| Newer agent, staged copy to disk | **~58 s** total (**~19 s** copy after CRIU) |
| Same stack, write checkpoint directly on PVC | **~24 s** total (copy **~0 s**) |

Restore (pod ready again) was **~36 s** on the baseline path and **~25–27 s** on the newer stack. We cared about capture; restore is only there for scale comparison.

---

## Experiments in order

![Experiment roadmap](0-experiment-roadmap.png)

```mermaid
flowchart LR
  T1["Trial 1: path matrix<br/>6 configs, n=3"]
  T2["Trial 2: tail study<br/>sampler + leave-running"]
  T3a["Trial 3a: newer agent<br/>publish = copy"]
  T3b["Trial 3b: direct publish<br/>same agent build"]
  T1 --> T2 --> T3a --> T3b
```

**Agent builds (plain language)**

| Label | What changed |
|---|---|
| **Baseline** | Original snapshot agent we started with (Oct runs) |
| **Baseline + diagnostic** | Same agent, one-off test that does not kill the engine after dump |
| **Newer stack** | Upgraded operator, agent, and PageBroker to current upstream main |
| **Newer + direct publish** | Small agent change so capture writes on the PVC and commits with a rename (no big copy step) |

Storage toggles (**Trial 1 only**): direct vs buffered I/O, PageBroker on/off, agent memory 4 Gi vs 96 Gi.

---

## What one capture looks like

![Capture timeline](0-capture-timeline.png)

```mermaid
flowchart LR
  A[Freeze workload] --> B[Write checkpoint pages]
  B --> C[Kill engine and wait on kernel]
  C --> D[Publish / switch version]
```

| Term | Meaning |
|---|---|
| Capture total | Wall clock for the checkpoint the operator reports |
| CRIU dump window | Time the agent spends inside the CRIU dump call |
| Write phase | Until the image on disk is essentially complete |
| Post-write tail | After the image is done, before CRIU finishes |
| Publish | Making the new checkpoint visible (rename or copy) |

---

## Trial 1: path matrix (baseline agent)

**When:** Oct 14–16 · **Models:** Qwen3 0.6B and 14B · **Repeats:** 3 per arm

We tried six combinations of I/O mode, PageBroker, and agent memory on the same node and PVC.

![14B capture by path](1-phases-14b.png)

| Path | 14B capture | Write | Tail | Extra publish | 14B restore |
|---|---:|---:|---:|---:|---:|
| **Direct I/O, 96 Gi agent** | **19.5 s** | 10.8 s | 8.7 s | (rename) | 36 s |
| Direct, 4 Gi agent | 20.2 s | 11.2 s | 8.8 s | (rename) | — |
| Buffered I/O, 96 Gi | 22.6 s | 13.8 s | 8.6 s | (rename) | 36 s |
| Buffered, 4 Gi | 31.8 s | 22.8 s | 8.8 s | (rename) | — |
| PageBroker + direct | 35.4 s | 26.6 s | 8.8 s | 20 s | 64 s |
| PageBroker + buffered | 35.5 s | 26.4 s | 8.7 s | 20 s | 63 s |

**Takeaways**

- The **~8 s tail** showed up on every arm that did not use PageBroker staging. Disk tuning did not move it.
- **Direct I/O at ~4.7 GB/s** on this PVC; agent memory did not matter much for direct.
- **Buffered I/O** slowed down a lot with only **4 Gi** on the agent pod.
- **PageBroker** added a long staging write plus **~20 s** to publish; capture and restore both got slower.

### Tail vs model size

![Tail grows with checkpoint size](2-tail-vs-size.png)

Tail length tracks image size (~0.15 s/GB). That fits “cost of tearing down a big process” more than “slow disk.”

---

## Trial 2: what is the tail?

**Sampler run** (14B, same baseline path as above):

![Tail probe](3-tail-probe.png)

During the tail, CRIU was idle on CPU while the kernel sat in `wait4` on the engine (zombie, heavy RSS). Nothing useful showed up in CRIU logs between “image done” and “stats written.”

**Leave-running diagnostic** (engine not killed after dump):

![Leave running vs control](4-leaverunning.png)

| | Normal kill | Leave running |
|---|---:|---:|
| Tail | 7.96 s | 0.01 s |
| CRIU dump window | 19.77 s | 11.42 s |

Same bytes written either way. The **~8 s** we keep seeing lines up with **kill + kernel reap**, not extra disk I/O.

---

## Trial 3: newer agent stack

**When:** Oct 19 · **Model:** 14B · **Repeats:** 1 per step (exploratory)

Same node and workload. Compare to baseline **direct I/O, 96 Gi** from Trial 1.

| Step | CRIU dump | Publish | Capture total | Restore |
|---|---:|---:|---:|---:|
| Baseline (Trial 1 median) | 19.5 s | rename | ~19.5 s | 36 s |
| **3a** Newer stack, staged publish | 34.8 s | 18.8 s copy | **57.7 s** | 27 s |
| **3b** Direct publish on PVC | 21.2 s | ~0 s | **24.2 s** | 25 s |

**3a → 3b** only changed how we publish after CRIU. Operator and PageBroker stayed on the newer build.

After direct publish, **~21 s** still sat in the CRIU dump window (write + likely the same tail we saw in Trial 1). We did not re-run the tail sampler on these builds.

---

## What looks worth optimizing

1. **Post-write reap (~8 s at 14B)** – largest fixed chunk on the baseline path; diagnostic says it is not image I/O.
2. **Staged publish copy (~19–20 s)** – shows up with PageBroker on the baseline agent and with the stock newer agent until direct publish.
3. **Buffered I/O on a small agent** – big penalty when the agent cannot hold page cache.
4. **CUDA checkpoint (~3 s)** – smaller than the two items above.

Open questions: when can we mark a snapshot “ready” without waiting on reap; whether direct publish should be the default on large checkpoints; more repeats on **3b** (n≥3) and a tail probe on the newer stack.

---

## Caveats

- Trial 1: **n=3**; tail diagnostic and Trial 3: **n=1**.
- Leave-running is **not** a supported production mode; it only isolates the tail.
- Raw JSON and logs live under `benchmarks/results/` on the machine that ran the tests (not all checked into git).

---

## Repo

Report and plots: `benchmarks/results/capture-latency-report/` on branch `exp/criu-capture-latency` ([fork](https://github.com/sheetal104/snapshot/tree/exp/criu-capture-latency/benchmarks/results/capture-latency-report)).
