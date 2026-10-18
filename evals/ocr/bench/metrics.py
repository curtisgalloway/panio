# SPDX-FileCopyrightText: 2026 Curtis Galloway
# SPDX-License-Identifier: Apache-2.0

"""Scoring, and the two shapes of ground truth.

`<stem>.gt.txt` is a full transcription, scored by CER and WER. `<stem>.gt.json`
carries `required_tokens` — the strings panio actually has to find on that
screen — scored by recall.

The second form exists because full transcription of a busy GUI frame is
expensive and mostly measures things nobody cares about (window chrome, clock
digits, a wallpaper's stray glyphs). What matters for a bring-up tool is whether
`UEFI: PXE IPv4` and `KINGSTON` came back, and token recall says exactly that.

**Recall is scored against normalized text, but reported alongside which tokens
were missed**, because *which* ones vanish is the finding. An engine that drops
every boot-order value while reading the page heading perfectly is not 80%
correct in any sense an agent cares about.
"""

from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import dataclass
from pathlib import Path

import jiwer


def normalize(s: str, *, case_sensitive: bool = True) -> str:
    """Collapse whitespace, strip per line, NFC.

    Case is preserved by default: terminal output is case-significant, and a
    tool that reads `/DEV/SDA` as equivalent to `/dev/sda` is not reading a
    console correctly.
    """
    s = unicodedata.normalize("NFC", s)
    lines = [re.sub(r"\s+", " ", ln).strip() for ln in s.splitlines()]
    out = "\n".join(ln for ln in lines if ln)
    return out if case_sensitive else out.lower()


@dataclass
class Score:
    kind: str  # "cer_wer" or "token_recall"
    cer: float | None = None
    wer: float | None = None
    cer_ci: float | None = None
    recall: float | None = None
    found: int = 0
    total: int = 0
    missing: list[str] = None  # type: ignore[assignment]

    def primary(self) -> float:
        """One number, lower is better, for ranking across both kinds."""
        if self.kind == "cer_wer":
            return self.cer if self.cer is not None else 1.0
        return 1.0 - (self.recall or 0.0)


def load_truth(image: Path) -> tuple[str, object] | None:
    txt = image.with_suffix("").with_suffix(".gt.txt")
    if not txt.exists():
        txt = image.parent / (image.stem + ".gt.txt")
    if txt.exists():
        return ("text", txt.read_text(encoding="utf-8"))
    js = image.parent / (image.stem + ".gt.json")
    if js.exists():
        return ("tokens", json.loads(js.read_text(encoding="utf-8")))
    return None


def score(hypothesis: str, truth_kind: str, truth) -> Score:
    if truth_kind == "text":
        ref = normalize(str(truth))
        hyp = normalize(hypothesis)
        if not ref:
            return Score(kind="cer_wer", cer=1.0, wer=1.0)
        return Score(
            kind="cer_wer",
            cer=jiwer.cer(ref, hyp),
            wer=jiwer.wer(ref, hyp),
            cer_ci=jiwer.cer(ref.lower(), hyp.lower()),
        )

    tokens = list(truth.get("required_tokens", []))
    hyp = normalize(hypothesis)
    # Whitespace inside a token is normalized the same way as the hypothesis, so
    # a token written naturally still matches text the engine ran together.
    found, missing = 0, []
    for t in tokens:
        if normalize(t) in hyp:
            found += 1
        else:
            missing.append(t)
    return Score(
        kind="token_recall",
        recall=(found / len(tokens)) if tokens else 0.0,
        found=found,
        total=len(tokens),
        missing=missing,
    )
