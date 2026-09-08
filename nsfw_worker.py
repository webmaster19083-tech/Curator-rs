#!/usr/bin/env python3
"""
Curator NSFW-classification worker.

curator.exe writes this script out to <data_dir>/nsfw_worker.py on every
startup (it's embedded in the binary via include_str!, so it always matches
the version of Curator you're running) and launches it once as a long-lived
subprocess — loading the model is the expensive part, so this avoids paying
that cost per image the way spawning a fresh interpreter per file would.

Protocol: one JSON object per line in each direction over stdin/stdout.

    Rust -> Python:  {"id": 123, "path": "/abs/path/to/file.jpg"}
    Python -> Rust:  {"id": 123, "score": 0.0431}
                   or {"id": 123, "error": "cannot identify image file"}

On startup, once the model has finished loading, this prints a single
{"ready": true} line — Curator waits for that before sending any requests,
so the first real job isn't the one eating the model-load delay.

A failure classifying one particular image (corrupt file, unsupported
format, whatever) is reported back as an {"id": ..., "error": ...} line and
the worker keeps running — the same lesson as Curator's thumbnail handling:
one bad file shouldn't cost you the whole warmed-up process. Only a
genuinely broken pipe (or this script exiting) makes the Rust side spawn a
replacement.

Requires: opennsfw-onnx (pulls in onnxruntime and Pillow itself, current
NumPy — no version pin needed). Install with your Python's pip, e.g.:

    pip install opennsfw-onnx

This is entirely optional — if this script can't import its dependencies,
it reports that once on stdout and exits; Curator just leaves media
unscored and everything else keeps working normally.
"""
import json
import sys


def main() -> int:
    # The parent sends UTF-8 JSON regardless of the Windows locale.
    sys.stdin.reconfigure(encoding="utf-8", errors="strict")
    sys.stdout.reconfigure(encoding="utf-8", errors="strict")
    try:
        from opennsfw_onnx import NSFWClassifier
    except Exception as e:  # noqa: BLE001 - report anything, don't just crash silently
        print(json.dumps({"ready": False, "error": f"missing dependency: {e}"}), flush=True)
        return 1

    try:
        clf = NSFWClassifier()
        clf.warmup()  # forces the onnxruntime session to load now, not on job 1
    except Exception as e:  # noqa: BLE001
        print(json.dumps({"ready": False, "error": f"model load failed: {e}"}), flush=True)
        return 1

    print(json.dumps({"ready": True}), flush=True)

    for raw_line in sys.stdin:
        raw_line = raw_line.strip()
        if not raw_line:
            continue

        req_id = None
        try:
            req = json.loads(raw_line)
            req_id = req.get("id")
            path = req["path"]
        except Exception as e:  # noqa: BLE001 - malformed request, not a worker crash
            print(json.dumps({"id": req_id, "error": f"bad request: {e}"}), flush=True)
            continue

        try:
            pred = clf.classify(path)  # accepts a path directly, no manual read needed
            print(json.dumps({"id": req_id, "score": pred.nsfw}), flush=True)
        except Exception as e:  # noqa: BLE001 - this image failed, worker stays up
            print(json.dumps({"id": req_id, "error": str(e)}), flush=True)

    return 0


if __name__ == "__main__":
    sys.exit(main())
