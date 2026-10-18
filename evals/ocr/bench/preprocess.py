# SPDX-FileCopyrightText: 2026 Curtis Galloway
# SPDX-License-Identifier: Apache-2.0

"""Composable preprocessing steps, and the variants the runner sweeps.

Two things are being measured here, and they are not the same question.

**Which preprocessing helps an engine.** Upscaling, inverting and binarizing
change what the engine sees. Note that panio's helpers already preprocess
internally (`visionocr` and `linuxocr` both upscale 2x and pad), so a variant
applied here stacks on top of that — the numbers say "does more help", not "does
any help".

**Whether panio's own pipeline is destroying quality.** `luma_direct` against
`rgb_roundtrip` is the interesting pair. On macOS and Windows a captured frame
is NV12: the Y plane *is* the greyscale image OCR wants, available for free. But
`/ocr` converts to RGB, encodes a PNG, and the helper decodes and re-greyscales
it — reconstructing luma from subsampled chroma it already had cleanly. The
research this harness came from says capture artifacts dominate engine choice,
so measuring the pipeline matters at least as much as measuring the engines.
"""

from __future__ import annotations

import cv2
import numpy as np


def identity(img: np.ndarray) -> np.ndarray:
    return img


def luma(img: np.ndarray) -> np.ndarray:
    """BT.601 luma. cv2's BGR2GRAY uses the same weights."""
    if img.ndim == 2:
        return img
    return cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)


def invert(img: np.ndarray) -> np.ndarray:
    return 255 - img


def upscale2_lanczos(img: np.ndarray) -> np.ndarray:
    return cv2.resize(img, None, fx=2, fy=2, interpolation=cv2.INTER_LANCZOS4)


def upscale3_lanczos(img: np.ndarray) -> np.ndarray:
    return cv2.resize(img, None, fx=3, fy=3, interpolation=cv2.INTER_LANCZOS4)


def upscale2_nearest(img: np.ndarray) -> np.ndarray:
    """Nearest-neighbour, for bitmap console fonts.

    Lanczos invents intermediate greys along a glyph edge that a bitmap font
    never had; nearest keeps the hard edge. Worth testing separately from the
    smooth upscale rather than assuming one is better for both screen types.
    """
    return cv2.resize(img, None, fx=2, fy=2, interpolation=cv2.INTER_NEAREST)


def otsu(img: np.ndarray) -> np.ndarray:
    g = luma(img)
    _, out = cv2.threshold(g, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
    return out


def rgb_roundtrip(img: np.ndarray) -> np.ndarray:
    """Simulate what panio's capture path does to a frame before OCR.

    NV12 in, RGB out, chroma subsampled at 4:2:0 and reconstructed. This is not
    a preprocessing step anyone would choose — it is the pipeline reproduced so
    its cost can be measured against `luma_direct`.
    """
    yuv = cv2.cvtColor(img, cv2.COLOR_BGR2YUV_I420)
    return cv2.cvtColor(yuv, cv2.COLOR_YUV2BGR_I420)


def luma_direct(img: np.ndarray) -> np.ndarray:
    """The Y plane as the capture device delivered it, no chroma round trip."""
    yuv = cv2.cvtColor(img, cv2.COLOR_BGR2YUV_I420)
    h = img.shape[0]
    return yuv[:h, :]


STEPS = {
    "identity": identity,
    "luma": luma,
    "invert": invert,
    "upscale2_lanczos": upscale2_lanczos,
    "upscale3_lanczos": upscale3_lanczos,
    "upscale2_nearest": upscale2_nearest,
    "otsu": otsu,
    "rgb_roundtrip": rgb_roundtrip,
    "luma_direct": luma_direct,
}

VARIANTS: dict[str, list[str]] = {
    "raw": [],
    "luma": ["luma"],
    "luma_up2": ["luma", "upscale2_lanczos"],
    "luma_up2_inv": ["luma", "upscale2_lanczos", "invert"],
    "luma_up2_otsu": ["luma", "upscale2_lanczos", "otsu"],
    "luma_up3_inv": ["luma", "upscale3_lanczos", "invert"],
    "luma_up2_nn": ["luma", "upscale2_nearest"],
    # The pipeline pair — see the module docstring.
    "pipeline_luma_direct": ["luma_direct"],
    "pipeline_rgb_roundtrip": ["rgb_roundtrip", "luma"],
}


def apply(img: np.ndarray, variant: str) -> np.ndarray:
    out = img
    for step in VARIANTS[variant]:
        out = STEPS[step](out)
    return out


def encode_png(img: np.ndarray) -> bytes:
    ok, buf = cv2.imencode(".png", img)
    if not ok:
        raise RuntimeError("PNG encode failed")
    return buf.tobytes()
