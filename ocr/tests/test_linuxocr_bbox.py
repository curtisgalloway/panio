# Copyright 2026 The Panio Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Tests for ocr/linuxocr's bbox mapping (docs/dev/ocr.md's v1 OCR contract).

`ocr/linuxocr` has no .py suffix -- it's installed and run directly as a
script -- so it's loaded by path with importlib rather than imported as a
regular module.

These tests exercise `_clip_to_source` (the pure corner-mapping-and-clipping
math) directly, plus one integration test through `_lines_from_tsv`. Neither
needs Pillow: that is only used by `_preprocess`, which these tests never
call.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import pathlib
import types

import pytest

_LINUXOCR_PATH = pathlib.Path(__file__).resolve().parent.parent / "linuxocr"


def _load_linuxocr() -> types.ModuleType:
    loader = importlib.machinery.SourceFileLoader(
        "linuxocr_under_test", str(_LINUXOCR_PATH)
    )
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


linuxocr = _load_linuxocr()

# Synthetic source frame and preprocessing shared by every case below: a
# 100x80 source image, upscaled 2x and padded 10px on each side -- issue
# #149's own numbers for the preprocessing linuxocr applies when Pillow is
# present.
SCALE = 2
PAD = 10
SRC_W = 100
SRC_H = 80


@pytest.mark.parametrize(
    "name, x0, y0, x1, y1, expected",
    [
        ("fully inside", 40, 40, 60, 50, (15, 15, 10, 5)),
        # Issue #149's own example: a recognition rectangle flush with the
        # padded image's left edge (processed left=0, width=30) maps to
        # source x=0, width=10 -- not width=15, which is what the old code
        # reported by clamping the origin to 0 but leaving the
        # processed-space width (30 / scale = 15) unchanged.
        ("left edge crossing", 0, 40, 30, 60, (0, 15, 10, 10)),
        ("top edge crossing", 40, 0, 60, 30, (15, 0, 10, 10)),
        ("right edge crossing", 190, 40, 230, 60, (90, 15, 10, 10)),
        ("bottom edge crossing", 40, 150, 60, 190, (15, 70, 10, 10)),
        (
            "fully outside bottom-right clips to zero, not negative",
            300,
            300,
            340,
            340,
            (100, 80, 0, 0),
        ),
        (
            "fully outside top-left clips to zero, not negative",
            -40,
            -40,
            -10,
            -10,
            (0, 0, 0, 0),
        ),
    ],
)
def test_clip_to_source(name, x0, y0, x1, y1, expected):
    got = linuxocr._clip_to_source(x0, y0, x1, y1, SCALE, PAD, SRC_W, SRC_H)
    assert got == expected, name


def test_clip_to_source_unknown_source_size_does_not_clip_far_edge():
    """src_w/src_h of 0 means the PNG header could not be parsed (see
    `_png_size`). The near edge is still clamped to 0, but the far edge is
    left unclipped since the true source extent isn't known.
    """
    got = linuxocr._clip_to_source(190, 150, 230, 190, SCALE, PAD, 0, 0)
    assert got == (90, 70, 20, 20)


def test_lines_from_tsv_clips_at_source_edge():
    """End-to-end through `_lines_from_tsv`, not just the helper function --
    a single word's box at the padded image's left edge should come out
    clipped to the source frame."""
    tsv = (
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num"
        "\tleft\ttop\twidth\theight\tconf\ttext\n"
        "5\t1\t1\t1\t1\t1\t0\t40\t30\t20\t90.0\tAB\n"
    )
    lines = linuxocr._lines_from_tsv(tsv, SCALE, PAD, SRC_W, SRC_H)
    assert len(lines) == 1
    assert lines[0]["bbox"] == [0, 15, 10, 10]
    assert lines[0]["text"] == "AB"
    assert lines[0]["confidence"] == pytest.approx(0.9)
