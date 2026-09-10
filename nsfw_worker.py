#!/usr/bin/env python3
"""Persistent NudeNet 320 detector worker for Curator.

The wire protocol is JSON Lines.  Each request has an ``id`` and a bounded
``paths`` list; each result carries compact NudeNet detections rather than a
made-up sexual-activity score.  Curator's Rust side owns the 1/2/3 mapping,
which makes it impossible for detector confidence alone to produce Fast or
Cum.

Requires the optional local dependency:

    pip install nudenet

NudeNet ships/uses its own detector model.  Recent versions expose
``detect_batch``; older compatible versions fall back to per-file ``detect``
without changing the protocol.
"""
import json
import sys


def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


def compact_detection(raw):
    label = raw.get("class") or raw.get("label") or raw.get("name") or "UNKNOWN"
    try:
        score = float(raw.get("score", raw.get("confidence", 0.0)))
    except (TypeError, ValueError):
        score = 0.0
    output = {"label": str(label), "score": max(0.0, min(1.0, score))}
    box = raw.get("box") or raw.get("bbox")
    if isinstance(box, (list, tuple)) and len(box) >= 4:
        try:
            output["box"] = [int(round(float(value))) for value in box[:4]]
        except (TypeError, ValueError):
            pass
    return output


def normalize_batch(raw, count, paths):
    # NudeNet 3 returns one list per path.  Be defensive about early package
    # versions and a single-item list because optional ML dependencies vary.
    if isinstance(raw, dict):
        # Some releases return ``{path: detections}`` instead of a list. Keep
        # request order stable so Rust can associate evidence with each frame.
        return [raw.get(path) or raw.get(str(path)) or [] for path in paths]
    if not isinstance(raw, list):
        return [[] for _ in range(count)]
    if count == 1 and (not raw or isinstance(raw[0], dict)):
        return [raw]
    if len(raw) == count and all(isinstance(item, list) for item in raw):
        return raw
    if len(raw) == count and all(isinstance(item, dict) and "detections" in item for item in raw):
        return [item.get("detections") or [] for item in raw]
    # Do not accidentally attach a flattened multi-file result to every file.
    return [[] for _ in range(count)]


def main() -> int:
<<<<<<< Updated upstream
=======
    sys.stdin.reconfigure(encoding="utf-8", errors="strict")
    sys.stdout.reconfigure(encoding="utf-8", errors="strict")
>>>>>>> Stashed changes
    try:
        import nudenet
        from nudenet import NudeDetector
    except Exception as error:  # optional dependency: fail soft and visibly
        emit({"ready": False, "error": f"missing NudeNet dependency: {error}"})
        return 1

    try:
        # NudeNet's bundled detector is the lightweight 320-ish inference
        # path in supported releases.  Do not pass a downloaded arbitrary
        # model path here; Curator deliberately records the bundled model.
        detector = NudeDetector()
        version = str(getattr(nudenet, "__version__", "unknown"))
    except Exception as error:
        emit({"ready": False, "error": f"NudeNet model load failed: {error}"})
        return 1

    emit({"ready": True, "model": "NudeNet-320", "version": version})
    for raw_line in sys.stdin:
        raw_line = raw_line.strip()
        if not raw_line:
            continue
        request_id = None
        try:
            request = json.loads(raw_line)
            request_id = request.get("id")
            paths = request.get("paths")
            if not isinstance(paths, list) or not paths or len(paths) > 12:
                raise ValueError("paths must be a non-empty batch of at most 12 files")
            if not all(isinstance(path, str) and path for path in paths):
                raise ValueError("each path must be a string")
        except Exception as error:
            emit({"id": request_id, "error": f"bad request: {error}"})
            continue
        try:
            try:
                raw_results = detector.detect_batch(paths)
            except (AttributeError, TypeError):
                raw_results = [detector.detect(path) for path in paths]
            grouped = normalize_batch(raw_results, len(paths), paths)
            results = []
            for detections in grouped:
                detections = [compact_detection(item) for item in detections if isinstance(item, dict)]
                score = max((item["score"] for item in detections), default=0.0)
                results.append({
                    "model": "NudeNet-320",
                    "version": version,
                    "score": score,
                    "detections": detections[:32],
                })
            emit({"id": request_id, "results": results})
        except Exception as error:  # a corrupt image must not kill a warm worker
            emit({"id": request_id, "error": str(error)})
    return 0


if __name__ == "__main__":
    sys.exit(main())
