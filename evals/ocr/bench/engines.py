# SPDX-FileCopyrightText: 2026 Curtis Galloway
# SPDX-License-Identifier: Apache-2.0

"""OCR engines, behind one adapter.

**The adapter is panio's v1 OCR envelope** (`docs/dev/ocr.md`), not a Python
protocol invented here. That is a deliberate departure from the original
benchmark plan, and the reason is that the envelope arrived in the meantime: it
already carries text, per-line confidence, bounding boxes and engine identity in
a uniform shape, and it is what panio *actually runs in production*. A harness
that scored Python library calls instead would be measuring something adjacent
to the thing being shipped.

So there are two kinds of engine:

- `HelperEngine` runs an installed panio helper (`visionocr`, `winocr`,
  `linuxocr`) with `--json`. This is the deployed path, byte for byte.
- `RapidOcrEngine` wraps a *candidate* that panio does not ship yet, and
  produces the same envelope.

The point of the second shape is that a candidate which wins can be promoted to
a helper binary with no change to anything that scores it — and that a candidate
which loses costs nothing to remove. All RapidOCR-specific imports stay inside
its class: it is a thin wrapper around models from upstream PaddleOCR with a
single-maintainer bus factor, so the replacement path (a direct ONNX Runtime
pipeline over the same models) must stay a one-file change.
"""

from __future__ import annotations

import json
import shutil
import subprocess
from dataclasses import dataclass, field
from typing import Protocol


@dataclass
class Line:
    text: str
    confidence: float | None = None
    bbox: tuple[int, int, int, int] | None = None


@dataclass
class OcrResult:
    """A v1 envelope, parsed."""

    text: str
    lines: list[Line] = field(default_factory=list)
    engine: str = ""
    engine_detail: str = ""
    width: int = 0
    height: int = 0

    @classmethod
    def from_envelope(cls, env: dict) -> "OcrResult":
        lines = []
        for ln in env.get("lines", []):
            bbox = ln.get("bbox")
            lines.append(
                Line(
                    text=ln.get("text", ""),
                    # Absent confidence stays None. Coercing it to 0.0 would
                    # make an engine that cannot measure quality (Windows) look
                    # like one that measured it and found none.
                    confidence=ln.get("confidence"),
                    bbox=tuple(bbox) if bbox and len(bbox) == 4 else None,
                )
            )
        return cls(
            text=env.get("text", ""),
            lines=lines,
            engine=env.get("engine", ""),
            engine_detail=env.get("engine_detail", ""),
            width=env.get("width", 0),
            height=env.get("height", 0),
        )


class OcrEngine(Protocol):
    name: str

    def available(self) -> bool: ...
    def warmup(self) -> None: ...
    def recognize(self, png: bytes) -> OcrResult: ...


class HelperEngine:
    """An installed panio OCR helper, driven exactly as hdmicap drives it."""

    def __init__(self, name: str, binary: str, extra_args: list[str] | None = None):
        self.name = name
        self.binary = binary
        self.extra_args = extra_args or []

    def available(self) -> bool:
        return shutil.which(self.binary) is not None

    def warmup(self) -> None:
        # A 1x1 PNG: enough to pay any one-time cost (dynamic linking, engine
        # construction) without measuring it as part of a real frame.
        png = bytes.fromhex(
            "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
            "1f15c4890000000a49444154789c6300010000050001"
            "0d0a2db40000000049454e44ae426082"
        )
        try:
            self.recognize(png)
        except Exception:
            pass

    def recognize(self, png: bytes) -> OcrResult:
        proc = subprocess.run(
            [self.binary, *self.extra_args, "--json", "-"],
            input=png,
            capture_output=True,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"{self.binary} exited {proc.returncode}: "
                f"{proc.stderr.decode('utf-8', 'replace').strip()[:200]}"
            )
        env = json.loads(proc.stdout)
        if "version" not in env:
            raise RuntimeError(f"{self.binary} did not emit a v1 envelope")
        return OcrResult.from_envelope(env)


class RapidOcrEngine:
    """PP-OCRv5 mobile via RapidOCR — a candidate, not a shipped helper.

    Reports no confidence per *line* directly from the envelope shape; RapidOCR
    gives a per-detection score, which maps cleanly onto one line each.
    """

    name = "rapidocr"

    def __init__(self) -> None:
        self._engine = None

    def available(self) -> bool:
        try:
            import rapidocr  # noqa: F401
        except ImportError:
            return False
        return True

    def _lazy(self):
        if self._engine is None:
            from rapidocr import RapidOCR

            # Four threads matches the Pi's core count, which is the target
            # this engine exists to serve. The key is nested under the engine
            # rather than Global — RapidOCR validates keys strictly and raises
            # on an unknown one, so a guess fails loudly rather than silently
            # running single-threaded.
            self._engine = RapidOCR(
                params={"EngineConfig.onnxruntime.intra_op_num_threads": 4}
            )
        return self._engine

    def warmup(self) -> None:
        self._lazy()

    def recognize(self, png: bytes) -> OcrResult:
        import cv2
        import numpy as np

        engine = self._lazy()
        img = cv2.imdecode(np.frombuffer(png, np.uint8), cv2.IMREAD_COLOR)
        h, w = img.shape[:2]
        out = engine(img)
        lines: list[Line] = []

        # `x or []` is wrong here: RapidOCR returns numpy arrays, and their
        # __bool__ raises "truth value of an array with more than one element is
        # ambiguous". That failed 116 of 117 runs while looking like an engine
        # problem rather than an adapter one.
        def as_list(attr: str) -> list:
            v = getattr(out, attr, None)
            return [] if v is None else list(v)

        boxes, txts, scores = as_list("boxes"), as_list("txts"), as_list("scores")
        for i, txt in enumerate(txts):
            bbox = None
            if i < len(boxes) and boxes[i] is not None:
                pts = np.asarray(boxes[i]).reshape(-1, 2)
                x0, y0 = pts[:, 0].min(), pts[:, 1].min()
                x1, y1 = pts[:, 0].max(), pts[:, 1].max()
                bbox = (int(x0), int(y0), int(x1 - x0), int(y1 - y0))
            conf = float(scores[i]) if i < len(scores) else None
            lines.append(Line(text=str(txt), confidence=conf, bbox=bbox))
        return OcrResult(
            text="\n".join(ln.text for ln in lines),
            lines=lines,
            engine="rapidocr",
            engine_detail="PP-OCRv5 mobile via RapidOCR",
            width=w,
            height=h,
        )


def registry() -> dict[str, OcrEngine]:
    """Every engine this harness knows how to drive.

    The helpers are listed for all platforms, not just the current one — an
    unavailable engine is reported as skipped rather than silently dropped, so a
    run's coverage is legible from its own output.
    """
    engines: dict[str, OcrEngine] = {
        "visionocr": HelperEngine("visionocr", "visionocr"),
        "visionocr-fast": HelperEngine("visionocr-fast", "visionocr", ["--fast"]),
        "winocr": HelperEngine("winocr", "winocr"),
        "linuxocr": HelperEngine("linuxocr", "linuxocr"),
        "rapidocr": RapidOcrEngine(),
    }
    return engines
