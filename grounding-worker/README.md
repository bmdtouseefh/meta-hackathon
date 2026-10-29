# grounding-worker (lite SoM parser)

Classical-CV overlay parser. No torch, no weights, runs on 4 CPU / 4 GB RAM
(RSS < 500 MB, ~200-500 ms per 1600x900 frame).

Same HTTP contract as OmniParser V2, so V2 weights can replace this
service later with zero Rust changes.

## Run

```
pip install -r requirements.txt
uvicorn app:app --host 127.0.0.1 --port 8791 --workers 1
```

Low-RAM flags: `OMP_NUM_THREADS=2 OPENBLAS_NUM_THREADS=2`.

## API

* `GET /health` -> `{ok, backend, max_elements}`
* `POST /parse {image_b64, max_elements?}` ->
  `{width, height, ms, elements:[{id,x,y,w,h,cx,cy,conf,label}], annotated_b64}`

Coordinates are media pixels (1600x900). Rust maps `element:N` -> centre.

## Swap to full OmniParser V2 later

Keep the contract, replace `detector.py` with YOLOv9-E + Florence
(`microsoft/OmniParser-v2.0` weights) when a GPU box exists.
