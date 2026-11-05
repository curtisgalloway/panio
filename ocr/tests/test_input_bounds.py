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

"""Tests for the resource-limit checks added to ocr/linuxocr and ocr/rapidocr
by issue #151: a bounded read that caps encoded input, and a header-based
dimension check that runs before any full image decode. Also issue #167's
follow-up: rapidocr refuses input that is not a PNG at all, because the
header check it runs before `cv2.imdecode` can only speak for a PNG.

Both scripts have no .py suffix -- like ocr/tests/test_linuxocr_bbox.py, each
is loaded by path with importlib rather than imported as a regular module.

rapidocr's own OCR dependencies (rapidocr, cv2, numpy) are heavy and may not
be installed in this environment; they are imported inside rapidocr's main(),
never at module scope, so loading the module here (which only runs module-
level code) never needs them -- the same property test_linuxocr_bbox.py
already relies on for linuxocr and Pillow.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import io
import pathlib
import types

import pytest

_OCR_DIR = pathlib.Path(__file__).resolve().parent.parent


def _load_by_path(path: pathlib.Path, name: str) -> types.ModuleType:
    loader = importlib.machinery.SourceFileLoader(name, str(path))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


linuxocr = _load_by_path(_OCR_DIR / "linuxocr", "linuxocr_bounds_under_test")
rapidocr = _load_by_path(_OCR_DIR / "rapidocr", "rapidocr_bounds_under_test")


def _fake_png_header(width: int, height: int) -> bytes:
    """The minimum bytes `_png_size` needs to report a size: the PNG
    signature plus an IHDR chunk's length/type/width/height fields. No CRC,
    no IDAT, no real pixel data -- big enough to declare a size, small
    enough that actually decoding it would be its own bug report.
    """
    return (
        b"\x89PNG\r\n\x1a\n"
        + (13).to_bytes(4, "big")
        + b"IHDR"
        + width.to_bytes(4, "big")
        + height.to_bytes(4, "big")
    )


def _fake_jpeg(width: int, height: int) -> bytes:
    """A JPEG whose baseline SOF0 frame header declares ``width`` x ``height``.

    The SOI marker, a JFIF APP0 and one SOF0 segment -- no Huffman tables and
    no scan data, so nothing could actually decode an image out of it. What
    matters for issue #167 is that it is *not a PNG* and that it declares a
    size far over the shared limits, which is exactly the shape of input that
    used to walk past rapidocr's pre-decode check.
    """
    sof0 = (
        b"\xff\xc0"
        + (17).to_bytes(2, "big")  # segment length
        + b"\x08"  # 8-bit sample precision
        + height.to_bytes(2, "big")  # JPEG puts height first
        + width.to_bytes(2, "big")
        + b"\x03"  # three components, each: id, sampling factors, table
        + b"\x01\x22\x00\x02\x11\x01\x03\x11\x01"
    )
    jfif = (
        b"\xff\xe0"
        + (16).to_bytes(2, "big")
        + b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00"
    )
    return b"\xff\xd8" + jfif + sof0


@pytest.mark.parametrize("module", [linuxocr, rapidocr], ids=["linuxocr", "rapidocr"])
class TestSharedInputBounds:
    """Both helpers factor out the same two functions (`_read_bounded`,
    `_check_dimensions`), with the same shared constants -- see each
    module's comment on `MAX_ENCODED_BYTES` for why they must match.
    """

    def test_read_bounded_accepts_exactly_the_cap(self, module):
        data = b"x" * 10
        assert module._read_bounded(io.BytesIO(data), 10) == data

    def test_read_bounded_rejects_one_byte_over_the_cap(self, module):
        data = b"x" * 11
        with pytest.raises(SystemExit):
            module._read_bounded(io.BytesIO(data), 10)

    def test_read_bounded_accepts_empty_input(self, module):
        assert module._read_bounded(io.BytesIO(b""), 10) == b""

    def test_read_bounded_reassembles_chunks_smaller_than_the_cap(self, module):
        # A stream that never hands back a full chunk_size read still has to
        # be reassembled correctly across several `.read()` calls.
        data = b"y" * 37
        assert module._read_bounded(io.BytesIO(data), 1000) == data

    def test_check_dimensions_accepts_100x100(self, module):
        module._check_dimensions(100, 100)  # must not raise

    def test_check_dimensions_unknown_size_is_a_noop(self, module):
        # 0 means the caller (via `_png_size`) could not determine a size
        # yet; there is nothing to check.
        module._check_dimensions(0, 0)  # must not raise

    def test_check_dimensions_rejects_a_header_declaring_20000x20000(self, module):
        """The scenario the byte cap and header check exist for: a tiny file
        whose IHDR claims a huge image. `_png_size` parses the declared size
        from the crafted header alone -- no Pillow/OpenCV decode involved --
        and `_check_dimensions` rejects it before any decode would run.
        """
        header = _fake_png_header(20000, 20000)
        parsed = module._png_size(header)
        assert parsed == (20000, 20000)
        with pytest.raises(SystemExit):
            module._check_dimensions(*parsed)

    def test_check_dimensions_rejects_over_the_pixel_count_limit(self, module):
        # 6000x6000 is under MAX_DIMENSION (8192) on each side but over
        # MAX_PIXELS (33,177,600) in total: 36,000,000 > 33,177,600.
        with pytest.raises(SystemExit):
            module._check_dimensions(6000, 6000)


def test_linuxocr_a_100x100_png_passes_the_header_precheck():
    """The header-declared size from a normal, small PNG must sail through
    both checks `main()` runs before `_preprocess` -- the source size and
    the 2x-upscaled working size _preprocess is about to allocate.
    """
    header = _fake_png_header(100, 100)
    src_w, src_h = linuxocr._png_size(header)
    assert (src_w, src_h) == (100, 100)
    linuxocr._check_dimensions(src_w, src_h)  # must not raise
    linuxocr._check_dimensions(
        src_w * linuxocr._UPSCALE, src_h * linuxocr._UPSCALE
    )  # must not raise


def test_linuxocr_a_4k_capture_passes_the_2x_working_size_check():
    """The limits are sized so a 4K capture (3840x2160), which linuxocr
    upscales 2x during preprocessing, always passes -- the whole point of
    checking the *working* size rather than just the source size. (2x
    doubles it to exactly 7680x4320 -- MAX_DIMENSION/MAX_PIXELS's own
    numbers -- so this is the boundary case, not one with room to spare.)
    """
    src_w, src_h = 3840, 2160
    linuxocr._check_dimensions(src_w, src_h)  # must not raise
    linuxocr._check_dimensions(
        src_w * linuxocr._UPSCALE, src_h * linuxocr._UPSCALE
    )  # must not raise


def test_linuxocr_a_source_that_only_overflows_after_2x_upscale_is_rejected():
    """A source image can be small enough on its own to pass the first
    check, yet still blow the working-size budget once linuxocr's 2x
    upscale is applied -- that is what the second, working-size check is
    for. 4500x4500 is under MAX_DIMENSION (8192) and MAX_PIXELS
    (33,177,600 -- 4500x4500 = 20,250,000) as a source, but doubles to
    9000x9000, over MAX_DIMENSION on each side.
    """
    src_w, src_h = 4500, 4500
    linuxocr._check_dimensions(src_w, src_h)  # source alone: must not raise
    with pytest.raises(SystemExit):
        linuxocr._check_dimensions(src_w * linuxocr._UPSCALE, src_h * linuxocr._UPSCALE)


def test_rapidocr_png_size_cannot_size_a_jpeg():
    """The gap issue #167 was about, stated as a fact about `_png_size`: it
    answers (0, 0) for anything without the PNG signature, so a check guarded
    by "did that parse?" had nothing to check -- while `cv2.imdecode`, which
    runs immediately after, decodes JPEG happily.
    """
    assert rapidocr._png_size(_fake_jpeg(20000, 20000)) == (0, 0)


def test_rapidocr_rejects_a_jpeg_before_any_decode(capsys):
    """A JPEG declaring 20000x20000 must be refused on its header alone.

    This is main()'s pre-decode step verbatim -- `_require_png` feeding
    `_check_dimensions` -- and it runs here without numpy, rapidocr or cv2
    installed, which is the point: nothing has decoded anything yet.
    """
    with pytest.raises(SystemExit):
        rapidocr._check_dimensions(*rapidocr._require_png(_fake_jpeg(20000, 20000)))
    err = capsys.readouterr().err
    assert err.startswith("rapidocr: ")
    assert err.count("\n") == 1, f"expected one line, got {err!r}"


def test_rapidocr_rejects_an_in_bounds_jpeg_too():
    """Not a size check with a format check bolted on: a JPEG whose declared
    size is perfectly reasonable is still refused, because the contract
    (docs/dev/ocr.md) is a PNG and a JPEG's dimensions are not something this
    helper reads before handing the bytes to OpenCV.
    """
    with pytest.raises(SystemExit):
        rapidocr._require_png(_fake_jpeg(800, 600))


def test_rapidocr_accepts_a_well_formed_png_header():
    header = _fake_png_header(1920, 1080)
    assert rapidocr._require_png(header) == (1920, 1080)


def test_rapidocr_rejects_a_png_signature_with_no_ihdr(capsys):
    """A truncated PNG -- the signature and nothing else -- is a size this
    helper cannot establish, so it is an error rather than a skipped check.
    """
    with pytest.raises(SystemExit):
        rapidocr._require_png(b"\x89PNG\r\n\x1a\n")
    err = capsys.readouterr().err
    assert err.startswith("rapidocr: ")
    assert err.count("\n") == 1, f"expected one line, got {err!r}"
