# SPDX-FileCopyrightText: 2026 The Panio Authors
# SPDX-License-Identifier: Apache-2.0

"""Turn raw rows into a summary someone can decide from.

`summary.md` alone should be enough to pick the production config per screen
type. That means it has to say which tokens were *missed*, not just a rate: an
engine that reads a BIOS page's headings perfectly and drops every boot-order
value scores respectably and is useless for the job.
"""

from __future__ import annotations

import statistics
from collections import defaultdict
from pathlib import Path


def _median(xs: list[float]) -> float | None:
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def write_report(rows: list[dict], env: dict, out: Path) -> None:
    ok = [r for r in rows if not r.get("error") and r.get("score_kind") not in (None, "none")]
    errors = [r for r in rows if r.get("error")]

    lines: list[str] = ["# OCR benchmark", ""]
    lines += [
        "## Environment",
        "",
        f"- host: `{env.get('model')}` ({env.get('machine')})",
        f"- python: {env.get('python')}",
        f"- governor: {env.get('governor') or 'n/a'}",
        f"- tesseract: {(env.get('tesseract') or ['n/a'])[0]}",
        f"- throttled at start: `{env.get('throttled_before') or 'n/a'}`",
        "",
    ]

    if not ok:
        lines += [
            "## No scored results",
            "",
            "Every row either errored or had no ground truth. Ground truth lives",
            "beside each image as `<stem>.gt.txt` (full transcription, scored by",
            "CER/WER) or `<stem>.gt.json` with `required_tokens` (scored by recall).",
            "",
        ]

    by_mode: dict[str, list[dict]] = defaultdict(list)
    for r in ok:
        by_mode[r["mode"]].append(r)

    for mode in sorted(by_mode):
        lines += [f"## {mode} screens", ""]
        agg: dict[tuple[str, str], list[dict]] = defaultdict(list)
        for r in by_mode[mode]:
            agg[(r["engine"], r["variant"])].append(r)

        table = []
        for (engine, variant), rs in agg.items():
            table.append(
                {
                    "engine": engine,
                    "variant": variant,
                    "primary": _median([float(r["primary"]) for r in rs if r.get("primary") != ""]),
                    "latency": _median([float(r["latency_ms"]) for r in rs if r.get("latency_ms")]),
                    "n": len(rs),
                    "missing": sorted(
                        {m for r in rs for m in (r.get("missing") or "").split("|") if m}
                    ),
                }
            )
        # Lower primary is better; ties broken by latency, per the spec.
        table.sort(key=lambda t: (t["primary"] if t["primary"] is not None else 9, t["latency"] or 9e9))

        lines += [
            "| engine | variant | error (lower=better) | median ms | n |",
            "| --- | --- | --- | --- | --- |",
        ]
        for t in table:
            p = "n/a" if t["primary"] is None else f"{t['primary']:.3f}"
            ms = "n/a" if t["latency"] is None else f"{t['latency']:.0f}"
            lines.append(f"| {t['engine']} | {t['variant']} | {p} | {ms} | {t['n']} |")
        lines.append("")

        if table:
            best = table[0]
            lines += [
                f"**Recommended for {mode}: `{best['engine']}` + `{best['variant']}`** "
                f"(error {best['primary']:.3f}, {best['latency']:.0f} ms median)."
                if best["primary"] is not None and best["latency"] is not None
                else f"**Recommended for {mode}: `{best['engine']}` + `{best['variant']}`**.",
                "",
            ]
            if best["missing"]:
                lines += [
                    "Even the best config missed these required tokens — *which* strings",
                    "are lost matters more than the rate:",
                    "",
                ]
                lines += [f"- `{m}`" for m in best["missing"][:12]]
                lines.append("")

    if errors:
        lines += ["## Errors", ""]
        seen = set()
        for r in errors[:20]:
            key = (r["engine"], r.get("error", "")[:80])
            if key in seen:
                continue
            seen.add(key)
            lines.append(f"- `{r['engine']}` on {r['image']} ({r['variant']}): {r['error']}")
        lines.append("")

    out.write_text("\n".join(lines), encoding="utf-8")
