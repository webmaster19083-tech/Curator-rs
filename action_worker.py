#!/usr/bin/env python3
"""Optional P-HAR-compatible temporal-action worker.

This worker is intentionally separate from ``nsfw_worker.py``.  It expects a
locally supplied TorchScript action model and an adjacent optional
``<model>.labels.json`` list.  The public P-HAR project does not ship one
stable universal inference API/model checkpoint, so Curator uses this small,
explicit adapter rather than pretending a generic NudeNet detector can infer
activity.  Missing torch/OpenCV/model dependencies report ``ready:false`` and
the Rust scheduler places clips in manual review exactly once.

Protocol: one JSON object per line, with ``path`` and overlapping ``windows``.
Output labels are evidence only; Rust applies the two-consecutive-windows
threshold and never assigns Cum automatically.
"""
import argparse
import json
import sys
from pathlib import Path


def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


def load_labels(model_path):
    candidates = [
        Path(str(model_path) + ".labels.json"),
        Path(model_path).with_suffix(".labels.json"),
    ]
    for candidate in candidates:
        try:
            values = json.loads(candidate.read_text(encoding="utf-8"))
            if isinstance(values, list) and all(isinstance(value, str) for value in values):
                return values
        except Exception:  # optional sidecar
            pass
    # Safe neutral fallback: unlabeled outputs can never be considered an
    # explicit action by Curator.
    return ["unknown"]


def sample_window(cv2, path, start, end, frames=16):
    capture = cv2.VideoCapture(str(path))
    if not capture.isOpened():
        raise RuntimeError("could not open video")
    values = []
    try:
        for index in range(frames):
            fraction = (index + 0.5) / frames
            capture.set(cv2.CAP_PROP_POS_MSEC, (start + (end - start) * fraction) * 1000.0)
            ok, frame = capture.read()
            if not ok:
                raise RuntimeError("could not decode a temporal frame")
            frame = cv2.resize(frame, (224, 224))
            # BGR -> RGB and HWC -> CHW
            values.append(frame[:, :, ::-1].transpose(2, 0, 1))
    finally:
        capture.release()
    return values


def predict(torch, numpy, model, cv2, path, start, end, labels):
    frames = sample_window(cv2, path, start, end)
    tensor = torch.from_numpy(numpy.stack(frames).copy()).float() / 255.0
    # P-HAR-family checkpoints vary between [N,T,C,H,W] and [N,C,T,H,W].
    # Try the conventional former first and only retry shape, not inference.
    with torch.no_grad():
        try:
            output = model(tensor.unsqueeze(0))
        except Exception:
            output = model(tensor.permute(1, 0, 2, 3).unsqueeze(0))
    if isinstance(output, (tuple, list)):
        output = output[0]
    probabilities = torch.softmax(output.flatten(), dim=0)
    score, index = torch.max(probabilities, dim=0)
    index = int(index.item())
    label = labels[index] if 0 <= index < len(labels) else "unknown"
    return {"start_secs": start, "end_secs": end, "label": label, "score": float(score.item())}


def main() -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--model", required=True)
    args = parser.parse_args()
    sys.stdin.reconfigure(encoding="utf-8", errors="strict")
    sys.stdout.reconfigure(encoding="utf-8", errors="strict")
    try:
        import cv2
        import numpy
        import torch
        model_path = Path(args.model)
        if not model_path.is_file():
            raise FileNotFoundError(model_path)
        model = torch.jit.load(str(model_path), map_location="cpu")
        model.eval()
        labels = load_labels(model_path)
    except Exception as error:
        emit({"ready": False, "error": f"P-HAR model unavailable: {error}"})
        return 1

    emit({"ready": True, "model": "P-HAR", "version": str(getattr(torch, "__version__", "unknown"))})
    for raw_line in sys.stdin:
        raw_line = raw_line.strip()
        if not raw_line:
            continue
        request_id = None
        try:
            request = json.loads(raw_line)
            request_id = request.get("id")
            path = Path(request["path"])
            windows = request["windows"]
            if not path.is_file() or not isinstance(windows, list) or not windows:
                raise ValueError("path and non-empty windows are required")
            clean_windows = []
            for window in windows[:64]:
                start, end = float(window[0]), float(window[1])
                if not (0 <= start < end):
                    raise ValueError("invalid temporal window")
                clean_windows.append((start, end))
        except Exception as error:
            emit({"id": request_id, "error": f"bad request: {error}"})
            continue
        try:
            output = [predict(torch, numpy, model, cv2, path, start, end, labels) for start, end in clean_windows]
            emit({"id": request_id, "result": {"model": "P-HAR", "version": str(getattr(torch, "__version__", "unknown")), "windows": output}})
        except Exception as error:
            emit({"id": request_id, "error": str(error)})
    return 0


if __name__ == "__main__":
    sys.exit(main())
