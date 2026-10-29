"""Lite interactable-region detector: classical CV + OCR, no ML weights.

Runs on 4 CPU / 4 GB RAM (RSS < 500 MB, ~0.3-1.5 s per 1600x900 frame:
contours ~70 ms, Tesseract OCR the rest).
Same POST /parse contract as OmniParser V2 so the V2 weights can replace
this file later without touching Rust.
"""

from __future__ import annotations

import shutil

import cv2
import numpy as np

# Tuned for 1600x900 XFCE: buttons, panel launchers, window controls,
# desktop icons. Filters out full-screen contours and 1px noise.
_MIN_AREA = 300        # ~20x15 px
_MAX_AREA = 120_000    # ~400x300 px (windows themselves are not targets)
_MIN_SIDE = 12
_NMS_IOU = 0.5
# Cap + chrome quota (all in 1600x900 media pixels).
# Pure-confidence ranking buries browser chrome (tabs, address bar,
# toolbar) under high-confidence body text — yet chrome is what the
# agent clicks. So the top band always keeps its own slots.
_MAX_ELEMENTS = 32
_TOP_BAND_Y = 170
_TOP_BAND_SLOTS = 10

# OCR text-line pass (needs the `tesseract` binary + pytesseract).
# Words Tesseract is unsure about (conf < 30) are dropped; words on the
# same line merge into one clickable target carrying the line's text.
_OCR_MIN_CONF = 30
_OCR_MIN_HEIGHT = 10


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


def _icon_boxes(
    bgr: np.ndarray, gray: np.ndarray, blur: np.ndarray,
) -> tuple[list[tuple[int, int, int, int]], list[float]]:
    """Edge-based boxes: buttons, icons, panel launchers."""
    h, w = gray.shape
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
    return boxes, scores


def _text_strip_boxes(
    blur: np.ndarray,
) -> tuple[list[tuple[int, int, int, int]], list[float]]:
    """Long thin boxes (panel, toolbars) Canny sometimes splits."""
    thresh = cv2.adaptiveThreshold(
        blur, 255, cv2.ADAPTIVE_THRESH_GAUSSIAN_C, cv2.THRESH_BINARY_INV, 25, 10
    )
    closed = cv2.morphologyEx(
        thresh, cv2.MORPH_CLOSE, cv2.getStructuringElement(cv2.MORPH_RECT, (25, 5))
    )
    contours, _ = cv2.findContours(closed, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    boxes: list[tuple[int, int, int, int]] = []
    scores: list[float] = []
    for c in contours:
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
        scores.append(float(area / (1600 * 900) + 0.1))
        boxes.append((int(x), int(y), int(bw), int(bh)))
    return boxes, scores


def _ocr_line_boxes(
    gray: np.ndarray,
) -> tuple[list[tuple[int, int, int, int]], list[float], list[str]]:
    """Tesseract word boxes grouped into clickable targets with text labels.

    PSM 11 (sparse text) + own row clustering: Tesseract's built-in line
    grouping merges whole toolbar rows into one giant box, so words are
    clustered by y-overlap here and split on wide x-gaps. One group =
    one badge ("Indicators", "Alert", "Watchlist"...).

    Returns ([], [], []) when pytesseract or the tesseract binary is
    missing — the icon passes above still work on their own.
    """
    try:
        import pytesseract
    except ImportError:
        return [], [], []
    if shutil.which("tesseract") is None:
        return [], [], []
    try:
        data = pytesseract.image_to_data(
            gray, output_type=pytesseract.Output.DICT, config="--psm 11"
        )
    except Exception:
        return [], [], []
    words: list[tuple[int, int, int, int, float, str]] = []
    H, W = gray.shape
    for i, raw in enumerate(data.get("text", [])):
        word = (raw or "").strip()
        if not word:
            continue
        try:
            conf = float(data["conf"][i])
            x, y = int(data["left"][i]), int(data["top"][i])
            bw, bh = int(data["width"][i]), int(data["height"][i])
        except (ValueError, TypeError):
            continue
        if conf < _OCR_MIN_CONF or bh < _OCR_MIN_HEIGHT or bw < 4:
            continue
        words.append((x, y, bw, bh, conf, word))
    if not words:
        return [], [], []
    # Row clustering by vertical overlap.
    words.sort(key=lambda t: t[1])
    rows: list[list[tuple[int, int, int, int, float, str]]] = []
    for wd in words:
        _, y, _, bh, _, _ = wd
        placed = False
        for row in rows:
            _, ry, _, rbh, _, _ = row[0]
            if abs((y + bh / 2) - (ry + rbh / 2)) < max(bh, rbh) * 0.6:
                row.append(wd)
                placed = True
                break
        if not placed:
            rows.append([wd])
    boxes: list[tuple[int, int, int, int]] = []
    scores: list[float] = []
    labels: list[str] = []
    for row in rows:
        row.sort(key=lambda t: t[0])
        median_h = sorted(t[3] for t in row)[len(row) // 2]
        # Split a row into targets on wide gaps (toolbar controls are
        # spaced; a sentence is not).
        groups: list[list[tuple[int, int, int, int, float, str]]] = [[row[0]]]
        for prev, cur in zip(row, row[1:]):
            gap = cur[0] - (prev[0] + prev[2])
            if gap > max(30, median_h * 2):
                groups.append([cur])
            else:
                groups[-1].append(cur)
        for g in groups:
            xs = [t[0] for t in g]
            ys = [t[1] for t in g]
            xe = [t[0] + t[2] for t in g]
            ye = [t[1] + t[3] for t in g]
            x, y = max(0, min(xs)), max(0, min(ys))
            bw = max(1, min(max(xe) - x, W - x))
            bh = max(1, min(max(ye) - y, H - y))
            if bw < 8 or bh < _OCR_MIN_HEIGHT or bw > W * 0.9:
                continue
            text = " ".join(t[5] for t in g)
            text = " ".join(text.split())[:48]
            if not text:
                continue
            # Drop chart-candle gibberish ("© a) © D @®", "| | | |"): a
            # real control label is mostly letters/digits.
            alnum = sum(c.isalnum() for c in text)
            if alnum < len(text.replace(" ", "")) * 0.5:
                continue
            # Drop full-width merges (tab bar + header glued into one
            # line): a 700px+ box centre-clicks nowhere useful.
            if bw > 700:
                continue
            avg_conf = sum(t[4] for t in g) / len(g) / 100.0
            boxes.append((x, y, bw, bh))
            # Text lines outrank icon blobs: they are what the user reads
            # and what the brain matches against.
            scores.append(1.5 + avg_conf)
            labels.append(text)
    return boxes, scores, labels


def detect(bgr: np.ndarray) -> list[dict]:
    """BGR 1600x900 -> [{x,y,w,h,cx,cy,conf,label}] in media pixels."""
    h, w = bgr.shape[:2]
    gray = cv2.cvtColor(bgr, cv2.COLOR_BGR2GRAY)
    blur = cv2.GaussianBlur(gray, (3, 3), 0)

    boxes, scores = _icon_boxes(bgr, gray, blur)
    sboxes, sscores = _text_strip_boxes(blur)
    boxes.extend(sboxes)
    scores.extend(sscores)
    icon_labels: list[str | None] = [None] * len(boxes)
    n_icon = len(boxes)

    oboxes, oscores, olabels = _ocr_line_boxes(gray)
    # Containment suppression: an icon blob mostly inside a text line is
    # the same control (e.g. watchlist currency glyphs) — the labeled
    # line wins, the bare "control N" badge would only clutter.
    keep_icon = [True] * n_icon
    for i in range(n_icon):
        ix, iy, iw, ih = boxes[i]
        for ox, oy, ow, oh in oboxes:
            inter = max(0, min(ix + iw, ox + ow) - max(ix, ox)) * max(
                0, min(iy + ih, oy + oh) - max(iy, oy)
            )
            if inter > 0.6 * iw * ih:
                keep_icon[i] = False
                break
    boxes = [b for b, k in zip(boxes, keep_icon) if k]
    scores = [s for s, k in zip(scores, keep_icon) if k]
    icon_labels = [l for l, k in zip(icon_labels, keep_icon) if k]
    boxes.extend(oboxes)
    scores.extend(oscores)
    icon_labels.extend(olabels)

    kept = _nms(boxes, scores)
    # Chrome quota: the top band (tabs, address bar, toolbar) always
    # keeps slots — pure-confidence ranking would bury it under
    # high-confidence body text, yet chrome is what gets clicked.
    kept.sort(key=lambda i: scores[i], reverse=True)
    band = [i for i in kept if boxes[i][1] < _TOP_BAND_Y][:_TOP_BAND_SLOTS]
    rest = [i for i in kept if boxes[i][1] >= _TOP_BAND_Y]
    rest = rest[: max(0, _MAX_ELEMENTS - len(band))]
    kept = band + rest
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
        label = icon_labels[i]
        if label is None:
            label = f"control {rank} ({bw}x{bh})"
        out.append(
            {
                "id": rank,
                "x": x,
                "y": y,
                "w": bw,
                "h": bh,
                "cx": x + bw // 2,
                "cy": y + bh // 2,
                "conf": round(min(0.99, max(0.05, scores[i] / 2.5)), 3),
                "label": label,
            }
        )
    return out
