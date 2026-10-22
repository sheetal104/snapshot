import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import FancyBboxPatch

OUT = "/home/shsriram/workspace/snapshot/benchmarks/results/capture-latency-report"
BLUE, GREY, GREEN, AMBER = "#3b6fd8", "#9aa0a6", "#2e7d4f", "#d9962b"


def rounded_box(ax, xy, w, h, text, fc, ec="#333"):
    x, y = xy
    box = FancyBboxPatch(
        (x, y),
        w,
        h,
        boxstyle="round,pad=0.02,rounding_size=0.08",
        linewidth=1.2,
        edgecolor=ec,
        facecolor=fc,
    )
    ax.add_patch(box)
    ax.text(x + w / 2, y + h / 2, text, ha="center", va="center", fontsize=9, wrap=True)


def arrow(ax, x1, y1, x2, y2):
    ax.annotate("", xy=(x2, y2), xytext=(x1, y1), arrowprops=dict(arrowstyle="->", lw=1.4, color="#444"))


# Experiment roadmap
fig, ax = plt.subplots(figsize=(10, 3.2))
ax.set_xlim(0, 10)
ax.set_ylim(0, 3)
ax.axis("off")
rounded_box(ax, (0.2, 1.0), 1.55, 1.0, "Trial 1\nBaseline matrix\n6 paths · n=3", BLUE, ec=BLUE)
rounded_box(ax, (2.15, 1.0), 1.55, 1.0, "Trial 2\nTail deep-dive\nprobe + leave-running", GREY)
rounded_box(ax, (4.1, 1.0), 1.55, 1.0, "Trial 3a\nNewer stack\nstaged publish", AMBER)
rounded_box(ax, (6.05, 1.0), 1.55, 1.0, "Trial 3b\nDirect publish\n(same stack)", GREEN, ec=GREEN)
arrow(ax, 1.75, 1.5, 2.15, 1.5)
arrow(ax, 3.7, 1.5, 4.1, 1.5)
arrow(ax, 5.65, 1.5, 6.05, 1.5)
ax.set_title("Experiments (in order)", loc="left", fontsize=11, fontweight="bold")
fig.tight_layout()
fig.savefig(f"{OUT}/0-experiment-roadmap.png", dpi=150, bbox_inches="tight")
plt.close(fig)

# Capture timeline (14B, best baseline path)
fig, ax = plt.subplots(figsize=(9, 2.2))
ax.set_xlim(0, 20)
ax.set_ylim(0, 2)
ax.axis("off")
phases = [
    (0, 0.46, "Freeze", "#e8eef8"),
    (0.46, 11.34, "Write checkpoint\n~11 s", BLUE),
    (11.8, 7.96, "After image done\nkernel reap ~8 s", GREY),
    (19.76, 0.24, "Done", "#e8eef8"),
]
y, h = 0.65, 0.7
for x, w, label, color in phases:
    ax.add_patch(plt.Rectangle((x, y), w, h, facecolor=color, edgecolor="#333", lw=1))
    ax.text(x + w / 2, y + h / 2, label, ha="center", va="center", fontsize=9, color="white" if color == BLUE else "#222")
for t, lab in [(0, "0 s"), (0.46, "0.5 s"), (11.8, "11.8 s"), (19.76, "~20 s")]:
    ax.text(t, 0.35, lab, ha="center", fontsize=8, color="#555")
ax.set_title("Where a 14B capture spends time (baseline · direct I/O · 96 Gi agent)", loc="left", fontsize=11, fontweight="bold")
fig.tight_layout()
fig.savefig(f"{OUT}/0-capture-timeline.png", dpi=150, bbox_inches="tight")
plt.close(fig)

print("Wrote 0-experiment-roadmap.png and 0-capture-timeline.png")
