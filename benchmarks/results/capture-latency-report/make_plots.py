import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

OUT = "/home/shsriram/workspace/snapshot/benchmarks/results/capture-latency-report"
SRC = "snapshot benchmarks · dynamo-aws-dev-02 · Oct 13–16 2026"
BLUE, GREY, AMBER, RED = "#3b6fd8", "#9aa0a6", "#d9962b", "#c8453b"

plt.rcParams.update({"font.size": 10, "axes.spines.top": False, "axes.spines.right": False})


def caption(fig, text):
    fig.text(0.01, 0.01, text, fontsize=8, color="#666")


# 1. Where a 14B capture spends its time
arms = ["direct · 96Gi", "direct · 4Gi", "writeback · 96Gi", "writeback · 4Gi", "PageBroker · direct", "PageBroker · writeback"]
write = [10.8, 11.2, 13.8, 22.8, 26.6, 26.4]
tail = [8.65, 8.81, 8.61, 8.82, 8.76, 8.72]
commit = [0, 0, 0, 0, 20.19, 20.19]
fig, ax = plt.subplots(figsize=(9, 4.4))
y = range(len(arms))[::-1]
ax.barh(y, write, color=BLUE, label="Write phase (to 99%)")
ax.barh(y, tail, left=write, color=GREY, label="Post-write tail (kill + reap)")
ax.barh(y, commit, left=[w + t for w, t in zip(write, tail)], color=AMBER, label="PageBroker commit")
for yi, w, t, c in zip(y, write, tail, commit):
    ax.text(w + t + c + 0.4, yi, f"{w + t + c:.1f} s", va="center", fontsize=9)
ax.set_yticks(list(y), arms)
ax.set_xlabel("Seconds (median of 3)")
ax.set_title("Qwen3 14B capture time by path")
ax.legend(loc="upper center", bbox_to_anchor=(0.5, -0.18), ncol=3, frameon=False, fontsize=9)
ax.set_xlim(0, 62)
caption(fig, f"Write/tail split from image-growth sampling. {SRC}")
fig.tight_layout(rect=(0, 0.04, 1, 1))
fig.savefig(f"{OUT}/1-phases-14b.png", dpi=150)

# 2. Tail vs image size
gb = [7.44, 10.35, 17.16, 28.58, 51.19]
tl = [0.87, 1.37, 2.44, 4.27, 7.88]
labels = ["0.6B", "1.7B", "4B", "8B", "14B"]
fig, ax = plt.subplots(figsize=(6, 3.6))
ax.plot(gb, tl, "o-", color=BLUE, label="measured")
k = sum(g * t for g, t in zip(gb, tl)) / sum(g * g for g in gb)
ax.plot([0, 55], [0, 55 * k], "--", color=GREY, label=f"fit through 0: {k:.3f} s/GB")
for g, t, l in zip(gb, tl, labels):
    ax.annotate(l, (g, t), textcoords="offset points", xytext=(5, -10), fontsize=8)
ax.set_xlabel("Checkpoint image size (GB)")
ax.set_ylabel("Unfreeze → Writing stats (s)")
ax.set_title("Post-write tail grows linearly with image size")
ax.legend(frameon=False)
caption(fig, f"Oct 13 direct sweep, dump.log markers. {SRC}")
fig.tight_layout(rect=(0, 0.04, 1, 1))
fig.savefig(f"{OUT}/2-tail-vs-size.png", dpi=150)

# 3. Tail probe: bytes written vs dying-process kernel CPU
t = [i * 0.5 for i in range(40)]
W = [0.0, 0.86, 1.22, 4.47, 6.84, 9.17, 12.28, 14.47, 16.92, 19.12, 21.96, 24.43, 26.72, 29.84, 32.15, 34.46, 37.59, 39.91, 41.99, 43.7, 44.35, 46.0, 47.67] + [47.67] * 17
E = [0] * 20 + [0.05, 0.12, 0.19, 0.3, 0.86, 1.26, 1.81, 2.34, 2.73, 3.28, 3.83, 4.23, 4.78, 5.32, 5.73, 6.27, 6.81, 7.22, 7.77, 8.04]
fig, ax1 = plt.subplots(figsize=(9, 3.8))
ax1.plot(t, W, color=BLUE, lw=2, label="CRIU bytes written (GiB, left)")
ax1.set_xlabel("Seconds since dump start")
ax1.set_ylabel("GiB written", color=BLUE)
ax2 = ax1.twinx()
ax2.spines["right"].set_visible(True)
ax2.plot(t, E, color=RED, lw=2, label="Engine process kernel CPU (s, right)")
ax2.set_ylabel("Cumulative kernel CPU (s)", color=RED)
ax1.axvline(11.8, color=GREY, ls="--")
ax1.text(12.0, 4, "Unfreezing tasks into 2\n(CRIU kills the source)", fontsize=8, color="#444")
ax1.axvspan(11.8, 19.76, color=GREY, alpha=0.12)
ax1.text(12.2, 38, "CRIU: 0.00 s CPU,\nstack do_wait → kernel_wait4", fontsize=8, color="#444")
h1, l1 = ax1.get_legend_handles_labels()
h2, l2 = ax2.get_legend_handles_labels()
ax1.legend(h1 + h2, l1 + l2, loc="upper left", frameon=False, fontsize=9)
ax1.set_title("Qwen3 14B dump: writing ends at 11.6 s, the rest is reaping the killed process")
caption(fig, f"/proc sampler at ~100 ms, control build, direct · 96Gi. {SRC}")
fig.tight_layout(rect=(0, 0.04, 1, 1))
fig.savefig(f"{OUT}/3-tail-probe.png", dpi=150)

# 4. Leave-running A/B
fig, ax = plt.subplots(figsize=(7, 2.6))
names = ["LeaveRunning=true\n(diagnostic)", "Control\n(kill, as shipped)"]
wr = [11.41, 11.80]
tt = [0.01, 7.96]
ax.barh([0, 1], wr, color=BLUE, label="Until Unfreezing tasks")
ax.barh([0, 1], tt, left=wr, color=GREY, label="Unfreezing → Writing stats")
for yi, a, b in zip([0, 1], wr, tt):
    ax.text(a + b + 0.3, yi, f"{a + b:.2f} s", va="center")
ax.set_yticks([0, 1], names)
ax.set_xlabel("criu_dump (s)")
ax.set_xlim(0, 23)
ax.set_title("Removing the kill removes the tail (Qwen3 14B, n=1 each)")
ax.legend(loc="lower right", frameon=False, fontsize=8)
caption(fig, f"Both built from a3cb9545; only dump.go:56 differs. {SRC}")
fig.tight_layout(rect=(0, 0.06, 1, 1))
fig.savefig(f"{OUT}/4-leaverunning.png", dpi=150)
print("ok")
