"""Grounding sidecar: JPEG in -> SoM elements + annotated JPEG out.

Lite OmniParser-compatible contract (no ML weights, runs on 4 CPU / 4 GB):

  GET  /health -> {ok, backend, max_elements}
  POST /parse  {image_b64, max_elements?} ->
    {width, height, ms, elements: [{id,x,y,w,h,cx,cy,conf,label}],
     annotated_b64}

Annotated JPEG draws red boxes + numbered badges; the brain picks
`element: N` instead of guessing `x,y`. Full OmniParser V2 can replace
this service later behind the identical contract.
"""

from __future__ import annotations

import base64
import io
import time

import cv2
import numpy as np
from fastapi import FastAPI, HTTPException
from PIL import Image, ImageDraw
from pydantic import BaseModel, Field

from detector import _MAX_ELEMENTS, detect

JPEG_QUALITY = 85

app = FastAPI(title="rose-grounding-lite")


class ParseRequest(BaseModel):
    image_b64: str = Field(description="JPEG bytes, base64 (1600x900 desktop)")
    max_elements: int = Field(default=_MAX_ELEMENTS, ge=1, le=60)


def _decode_jpeg(image_b64: str) -> np.ndarray:
    try:
        raw = base64.b64decode(image_b64, validate=True)
    except Exception as e:
        raise HTTPException(status_code=400, detail=f"bad base64: {e}")
    arr = np.frombuffer(raw, dtype=np.uint8)
    bgr = cv2.imdecode(arr, cv2.IMREAD_COLOR)
    if bgr is None:
        raise HTTPException(status_code=400, detail="not a decodable image")
    return bgr


def _annotate(bgr: np.ndarray, elements: list[dict]) -> bytes:
    rgb = cv2.cvtColor(bgr, cv2.COLOR_BGR2RGB)
    img = Image.fromarray(rgb)
    d = ImageDraw.Draw(img)
    for el in elements:
        x, y, w, h, i = el["x"], el["y"], el["w"], el["h"], el["id"]
        d.rectangle([x, y, x + w, y + h], outline=(220, 30, 30), width=2)
        label = str(i)
        # Badge above box, clamped into frame.
        bx0, by0 = x, max(0, y - 20)
        bx1, by1 = x + 8 + 8 * len(label), by0 + 20
        d.rectangle([bx0, by0, bx1, by1], fill=(0, 0, 0))
        d.text((bx0 + 4, by0 + 2), label, fill=(255, 255, 255))
    buf = io.BytesIO()
    img.save(buf, format="JPEG", quality=JPEG_QUALITY)
    return buf.getvalue()


@app.get("/health")
def health() -> dict:
    return {"ok": True, "backend": "contour-lite", "max_elements": _MAX_ELEMENTS}


@app.post("/parse")
def parse(req: ParseRequest) -> dict:
    started = time.perf_counter()
    bgr = _decode_jpeg(req.image_b64)
    h, w = bgr.shape[:2]
    elements = detect(bgr)[: req.max_elements]
    # Re-number after cap so ids stay 1..N contiguous.
    for rank, el in enumerate(elements, start=1):
        el["id"] = rank
        el["label"] = f"control {rank} ({el['w']}x{el['h']})"
    annotated = _annotate(bgr, elements)
    ms = int((time.perf_counter() - started) * 1000)
    return {
        "width": int(w),
        "height": int(h),
        "ms": ms,
        "elements": elements,
        "annotated_b64": base64.b64encode(annotated).decode("ascii"),
    }
