"""Lite interactable-region detector: classical CV only, no ML weights.

Runs on 4 CPU / 4 GB RAM (RSS < 500 MB, ~200-500 ms per 1600x900 frame).
Same POST /parse contract as OmniParser V2 so the V2 weights can replace
this file later without touching Rust.
"""

from __future__ import annotations

import cv2
import numpy as np

# Tuned for 1600x900 XFCE: buttons, panel launchers, window controls,
# desktop icons. Filters out full-screen contours and 1px noise.
_MIN_AREA = 300        # ~20x15 px
_MAX_AREA = 120_000    # ~400x300 px (windows themselves are not targets)
_MIN_SIDE = 12
_NMS_IOU = 0.5
_MAX_ELEMENTS = 40


def _nms(boxes: list[tuple[int, int, int, int]], scores: list[float]) -> list[int]:
    """Greedy IoU NMS, pure numpy. Returns kept indices (score order)."""
    if not boxes:
        return []
    order = sorted(range(len(boxes)), key=lambda i: scores[i], reverse=True)
    kept: list[int] = []
    b = np.array(boxes, dtype=float)
    while order:
        i = order.pop(0)
        kept.append(i)
        if not order:
            break
        xx1 = np.maximum(b[i, 0], b[order, 0])
        yy1 = np.maximum(b[i, 1], b[order, 1])
        xx2 = np.minimum(b[i, 0] + b[order, 2], b[order, 0] + b[order, 2])
        yy2 = np.minimum(b[i, 1] + b[order, 3], b[order, 1] + b[order, 3])
        inter = np.maximum(0, xx2 - xx1) * np.maximum(0, yy2 - yy1)
        union = (
            b[i, 2] * b[i, 3]
            + b[order, 2] * b[order, 3]
            - inter
            + 1e-6
        )
        order = [j for k, j in enumerate(order) if inter[k] / union[k] <= _NMS_IOU]
    return kept


def detect(bgr: np.ndarray) -> list[dict]:
    """BGR 1600x900 -> [{x,y,w,h,cx,cy,conf}] in media pixels."""
    h, w = bgr.shape[:2]
    gray = cv2.cvtColor(bgr, cv2.COLOR_BGR2GRAY)
    blur = cv2.GaussianBlur(gray, (3, 3), 0)

    # Edge-based boxes catch buttons, icons, panel launchers.
    edges = cv2.Canny(blur, 60, 160)
    edges = cv2.dilate(edges, np.ones((3, 3), np.uint8), iterations=1)

    contours, _ = cv2.findContours(edges, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)

    boxes: list[tuple[int, int, int, int]] = []
    scores: list[float] = []
    for c in contours:
        x, y, bw, bh = cv2.boundingRect(c)
        area = bw * bh
        if area < _MIN_AREA or area > _MAX_AREA:
            continue
        if bw < _MIN_SIDE or bh < _MIN_SIDE:
            continue
        aspect = bw / max(bh, 1)
        if aspect < 0.1 or aspect > 10.0:
            continue
        # Drop near-fullscreen frames (the wallpaper itself).
        if bw > w * 0.9 and bh > h * 0.9:
            continue
        # Solidity filter: reject thin edge streaks.
        rect_area = float(area)
        hull_area = float(cv2.contourArea(cv2.convexHull(c))) + 1e-6
        solidity = rect_area / hull_area
        if solidity < 0.4:
            continue
        # Score: larger + more rectangular = more likely a control.
        peri = cv2.arcLength(c, True) + 1e-6
        rectangularity = 4 * 3.14159 * cv2.contourArea(c) / (peri * peri)
        scores.append(float(area / (w * h) + rectangularity))
        boxes.append((int(x), int(y), int(bw), int(bh)))

    # Text-line pass: long thin boxes (panel, toolbars) Canny sometimes
    # splits. Adaptive threshold + horizontal close fills them.
    thresh = cv2.adaptiveThreshold(
        blur, 255, cv2.ADAPTIVE_THRESH_GAUSSIAN_C, cv2.THRESH_BINARY_INV, 25, 10
    )
    closed = cv2.morphologyEx(
        thresh, cv2.MORPH_CLOSE, cv2.getStructuringElement(cv2.MORPH_RECT, (25, 5))
    )
    contours2, _ = cv2.findContours(closed, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    for c in contours2:
        x, y, bw, bh = cv2.boundingRect(c)
        area = bw * bh
        if area < 800 or area > 60_000:
            continue
        if bh > 60 or bw < 40:
            continue
        # Skip full-width edge streaks (e.g. the panel boundary line):
        # wide and razor-thin is a seam, not a control.
        if bw > 1000 and bh < 20:
            continue
        scores.append(float(area / (w * h) + 0.1))
        boxes.append((int(x), int(y), int(bw), int(bh)))

    kept = _nms(boxes, scores)[:_MAX_ELEMENTS]
    # Stable IDs: top-to-bottom, left-to-right (panel first, like a user).
    kept.sort(key=lambda i: (boxes[i][1] // 60, boxes[i][0]))

    out = []
    for rank, i in enumerate(kept, start=1):
        x, y, bw, bh = boxes[i]
        # Clamp into frame (detector can overshoot by 1px after dilate).
        x = max(0, min(x, w - 1))
        y = max(0, min(y, h - 1))
        bw = max(1, min(bw, w - x))
        bh = max(1, min(bh, h - y))
        out.append(
            {
                "id": rank,
                "x": x,
                "y": y,
                "w": bw,
                "h": bh,
                "cx": x + bw // 2,
                "cy": y + bh // 2,
                "conf": round(min(0.99, max(0.05, scores[i] * 2.0)), 3),
                "label": f"control {rank} ({bw}x{bh})",
            }
        )
    return out
