"""Smoke test: synthetic panel + button, no camera needed."""

import base64
import io

import numpy as np
from PIL import Image

from detector import detect


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
    assert 1 <= len(els) <= 40, els
    for el in els:
        assert 0 <= el["x"] < 1600 and 0 <= el["y"] < 900
        assert el["cx"] == el["x"] + el["w"] // 2
    print(f"ok: {len(els)} elements")


if __name__ == "__main__":
    test_finds_boxes()
