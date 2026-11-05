# SPDX-FileCopyrightText: 2026 The Panio Authors
# SPDX-License-Identifier: Apache-2.0

"""Run the matrix: images x engines x preprocessing variants x repeats.

Sequentially, always. Two OCR engines sharing four Pi cores measure contention,
not themselves.

Usage:
  uv run python -m bench.runner --dataset dataset/ --out results/
  uv run python -m bench.runner --engines visionocr,linuxocr --variants raw,luma_up2
  uv run python -m bench.runner --limit 2 --repeats 1        # smoke test
"""

from __future__ import annotations

import argparse
import csv
import statistics
import sys
import time
from pathlib import Path

import cv2
import numpy as np

from . import hostinfo, metrics, preprocess
from .engines import registry


def load_images(dataset: Path, limit: int | None) -> list[tuple[str, Path]]:
    out: list[tuple[str, Path]] = []
    for mode in ("text", "gui"):
        files = sorted((dataset / mode).glob("*.png"))
        if limit:
            files = files[:limit]
        out.extend((mode, f) for f in files)
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dataset", type=Path, default=Path("dataset"))
    ap.add_argument("--out", type=Path, default=Path("results"))
    ap.add_argument("--engines", default="all")
    ap.add_argument("--variants", default="all")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--limit", type=int, default=None, help="images per mode")
    args = ap.parse_args(argv)

    engines = registry()
    if args.engines != "all":
        want = [e.strip() for e in args.engines.split(",")]
        unknown = [w for w in want if w not in engines]
        if unknown:
            print(f"unknown engine(s): {', '.join(unknown)}", file=sys.stderr)
            return 2
        engines = {k: v for k, v in engines.items() if k in want}

    variants = list(preprocess.VARIANTS)
    if args.variants != "all":
        variants = [v.strip() for v in args.variants.split(",")]
        unknown = [v for v in variants if v not in preprocess.VARIANTS]
        if unknown:
            print(f"unknown variant(s): {', '.join(unknown)}", file=sys.stderr)
            return 2

    images = load_images(args.dataset, args.limit)
    if not images:
        print(f"no images under {args.dataset}", file=sys.stderr)
        return 2

    live = {k: e for k, e in engines.items() if e.available()}
    for k in engines:
        if k not in live:
            print(f"  skip  {k} (not installed on this host)")
    if not live:
        print("no engines available on this host", file=sys.stderr)
        return 2

    env = hostinfo.describe()
    print(f"host: {env['model']} ({env['machine']}), engines: {', '.join(live)}")

    args.out.mkdir(parents=True, exist_ok=True)
    rows: list[dict] = []

    for name, engine in live.items():
        engine.warmup()
        temp_before, throttle_before = hostinfo.cpu_temp_c(), hostinfo.throttled()
        for mode, path in images:
            raw = cv2.imdecode(
                np.fromfile(str(path), dtype=np.uint8), cv2.IMREAD_COLOR
            )
            truth = metrics.load_truth(path)
            for variant in variants:
                t0 = time.perf_counter()
                try:
                    processed = preprocess.apply(raw, variant)
                    png = preprocess.encode_png(processed)
                except Exception as exc:  # a bad variant must not end the run
                    rows.append(
                        {
                            "image": path.name, "mode": mode, "engine": name,
                            "variant": variant, "error": f"preprocess: {exc}",
                        }
                    )
                    continue
                prep_ms = (time.perf_counter() - t0) * 1000

                lat: list[float] = []
                result = None
                error = ""
                for _ in range(args.repeats):
                    t1 = time.perf_counter()
                    try:
                        result = engine.recognize(png)
                    except Exception as exc:
                        error = str(exc)[:200]
                        break
                    lat.append((time.perf_counter() - t1) * 1000)

                row = {
                    "image": path.name, "mode": mode, "engine": name,
                    "variant": variant, "prep_ms": round(prep_ms, 2),
                    "error": error,
                }
                if result is not None and not error:
                    row["latency_ms"] = round(statistics.median(lat), 1)
                    row["lines"] = len(result.lines)
                    confs = [
                        ln.confidence for ln in result.lines if ln.confidence is not None
                    ]
                    row["mean_confidence"] = (
                        round(sum(confs) / len(confs), 4) if confs else ""
                    )
                    row["engine_detail"] = result.engine_detail
                    if truth:
                        sc = metrics.score(result.text, truth[0], truth[1])
                        row["score_kind"] = sc.kind
                        row["cer"] = "" if sc.cer is None else round(sc.cer, 4)
                        row["wer"] = "" if sc.wer is None else round(sc.wer, 4)
                        row["cer_ci"] = "" if sc.cer_ci is None else round(sc.cer_ci, 4)
                        row["recall"] = (
                            "" if sc.recall is None else round(sc.recall, 4)
                        )
                        row["missing"] = "|".join(sc.missing or [])
                        row["primary"] = round(sc.primary(), 4)
                    else:
                        row["score_kind"] = "none"
                rows.append(row)

        temp_after, throttle_after = hostinfo.cpu_temp_c(), hostinfo.throttled()
        if throttle_before != throttle_after:
            print(
                f"  !! {name}: throttle state changed "
                f"({throttle_before} -> {throttle_after}); latencies are not comparable"
            )
        if temp_before and temp_after:
            print(f"  {name}: {temp_before:.1f}C -> {temp_after:.1f}C")

    fields = [
        "image", "mode", "engine", "variant", "score_kind", "primary", "cer",
        "cer_ci", "wer", "recall", "missing", "lines", "mean_confidence",
        "latency_ms", "prep_ms", "engine_detail", "error",
    ]
    raw_csv = args.out / "raw.csv"
    with raw_csv.open("w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=fields, extrasaction="ignore")
        w.writeheader()
        w.writerows(rows)
    print(f"wrote {raw_csv} ({len(rows)} rows)")

    from .report import write_report

    write_report(rows, env, args.out / "summary.md")
    print(f"wrote {args.out / 'summary.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
