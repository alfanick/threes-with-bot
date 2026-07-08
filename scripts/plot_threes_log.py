#!/usr/bin/env python3

"""Plot Threes machine logs with score/eval and per-layer state counts."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path

import matplotlib.pyplot as plt
import matplotlib.colors as mcolors


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Plot score/evaluation and alpha-beta state counts from a machine log."
    )
    parser.add_argument("log_path", type=Path, help="Path to machine-readable JSONL log.")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("threes_log_plot.pdf"),
        help="Output PDF path.",
    )
    return parser.parse_args()


def load_steps(log_path: Path) -> tuple[list[int], list[float], list[float], list[list[float]]]:
    steps: list[int] = []
    scores: list[float] = []
    evals: list[float] = []
    layers: list[list[float]] = []

    step = 0
    with log_path.open("r", encoding="utf-8") as handle:
        for raw in handle:
            text = raw.strip()
            if not text:
                continue
            event = json.loads(text)
            if event.get("event") != "turn":
                continue

            result = event.get("result", {})
            accepted = result.get("accepted", True)
            if not accepted:
                continue

            step += 1
            steps.append(step)

            scores.append(float(result.get("score", float("nan"))))
            bot_search = event.get("bot_search")
            evals.append(
                float(bot_search.get("board_eval"))
                if isinstance(bot_search, dict) and bot_search.get("board_eval") is not None
                else float("nan")
            )
            raw_layers = []
            if isinstance(bot_search, dict):
                raw_layers = bot_search.get("states_per_layer") or []
            layers.append(to_tui_layer_summary([float(v) for v in raw_layers]))

    return steps, scores, evals, layers


def to_tui_layer_summary(layer_counts: list[float]) -> list[float]:
    if not layer_counts:
        return []

    summary: list[float] = []
    for layer, value in enumerate(layer_counts):
        if layer == 0:
            summary.append(value)
            continue

        parent = layer_counts[layer - 1]
        if parent == 0.0:
            summary.append(0.0)
        else:
            summary.append(math.ceil(value / parent))

    return summary


def pad_layer(values: list[list[float]], layer: int) -> list[float]:
    return [float("nan") if len(row) <= layer else row[layer] for row in values]


def plot(
    steps: list[int],
    scores: list[float],
    evals: list[float],
    layer_values: list[list[float]],
    output: Path,
) -> None:
    if not steps:
        raise RuntimeError("No turn events found in log")

    fig, score_ax = plt.subplots(figsize=(24, 4))
    score_ax.set_xlabel("step")
    score_ax.set_ylabel("score")
    score_ax.plot(steps, scores, label="score", color="#1f77b4", linewidth=1.2)
    score_ax.plot(steps, evals, label="eval", color="#2ca02c", linestyle="--", linewidth=1.0)
    score_ax.grid(True, alpha=0.25)
    score_ax.tick_params(axis="y")

    state_ax = score_ax.twinx()
    state_ax.set_ylabel("states per layer")

    max_layers = max((len(v) for v in layer_values), default=0)
    layer_color = "#9467bd"
    min_layer_alpha = 0.25
    layer_alphas: list[float] = []
    for layer in range(max_layers):
        if max_layers <= 1:
            alpha = 1.0
        else:
            alpha = 1.0 - (layer / (max_layers - 1)) * (1.0 - min_layer_alpha)
        layer_alphas.append(alpha)
        state_ax.plot(
            steps,
            pad_layer(layer_values, layer),
            label=f"layer {layer}",
            color=layer_color,
            alpha=alpha,
            linewidth=0.9,
        )

    lines, labels = score_ax.get_legend_handles_labels()
    s_lines, s_labels = state_ax.get_legend_handles_labels()
    score_ax.legend(
        lines + s_lines,
        labels + s_labels,
        loc="upper left",
        fontsize="small",
    )
    legend = score_ax.get_legend()
    if legend is not None:
        text_labels = legend.get_texts()
        for idx, text in enumerate(text_labels):
            layer_index = idx - len(lines)
            if layer_index < 0:
                continue
            if 0 <= layer_index < len(layer_alphas):
                text.set_color(mcolors.to_rgba(layer_color, alpha=layer_alphas[layer_index]))

    output.parent.mkdir(parents=True, exist_ok=True)
    fig.tight_layout()
    fig.savefig(output, format="pdf", dpi=300)
    plt.close(fig)


def main() -> None:
    args = parse_args()
    steps, scores, evals, layers = load_steps(args.log_path)
    plot(steps, scores, evals, layers, args.output)


if __name__ == "__main__":
    main()
