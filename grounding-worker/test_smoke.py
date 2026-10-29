"""Smoke test: synthetic panel + button, no camera needed."""

import base64
import io
import sys
import types

import numpy as np
from PIL import Image

from detector import _ocr_line_boxes, detect


def _synthetic() -> np.ndarray:
    img = Image.new("RGB", (1600, 900), (240, 240, 240))
    from PIL import ImageDraw

    d = ImageDraw.Draw(img)
    d.rectangle([0, 860, 1600, 900], fill=(60, 60, 60))  # panel
    d.rectangle([100, 200, 300, 250], outline=(0, 0, 0), width=3)  # button
    d.rectangle([1200, 100, 1400, 400], outline=(0, 0, 0), width=3)  # window
    return np.array(img)[:, :, ::-1].copy()  # RGB -> BGR


def test_finds_boxes():
    els = detect(_synthetic())
    assert 1 <= len(els) <= 24, els
    for el in els:
        assert 0 <= el["x"] < 1600 and 0 <= el["y"] < 900
        assert el["cx"] == el["x"] + el["w"] // 2
    print(f"ok: {len(els)} elements")


def test_ocr_merges_words_into_labeled_lines():
    # Stub pytesseract: two words on one line, one low-conf word dropped.
    fake = types.ModuleType("pytesseract")
    fake.Output = types.SimpleNamespace(DICT="dict")
    fake.image_to_data = lambda *a, **k: {
        "text": ["Indi", "cators", "x"],
        "conf": [95, 90, 5],
        "left": [380, 415, 900],
        "top": [122, 122, 500],
        "width": [34, 50, 10],
        "height": [22, 22, 12],
        "block_num": [1, 1, 2],
        "par_num": [1, 1, 1],
        "line_num": [1, 1, 1],
        "page_num": [1, 1, 1],
    }
    sys.modules["pytesseract"] = fake
    import shutil

    real_which = shutil.which
    shutil.which = lambda *a, **k: "/usr/bin/tesseract"
    try:
        gray = np.zeros((900, 1600), dtype=np.uint8)
        boxes, scores, labels = _ocr_line_boxes(gray)
    finally:
        del sys.modules["pytesseract"]
        shutil.which = real_which
    assert len(boxes) == 1, boxes
    assert boxes[0] == (380, 122, 85, 22), boxes[0]
    assert labels[0] == "Indi cators", labels
    print("ok: ocr line merge", boxes[0], labels[0])


if __name__ == "__main__":
    test_finds_boxes()
    test_ocr_merges_words_into_labeled_lines()
