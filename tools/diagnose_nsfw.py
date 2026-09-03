#!/usr/bin/env python3
"""
diagnose_nsfw.py — merged/improved version.

Combines what each of your two candidate scripts did well, drops what each
missed:

  - From the "B" script: enumerates EVERY Python interpreter it can find
    on the machine (PATH, Windows `py` launcher, well-known install dirs)
    and probes each one for the required packages — so if you already
    have a working install under a different Python, this tells you
    instead of making you reinstall.
  - From the "A" script: explicitly checks for the Windows "app execution
    alias" trap (`python` on PATH silently opening the Microsoft Store
    instead of running) — this is very likely YOUR actual problem, since
    curator.log shows Curator's Python resolving into
    C:\\Users\\...\\AppData\\Local\\Packages\\PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0\\...
    which is the Microsoft Store Python package path, not a normal
    python.org install.
  - From "A": also runs the actual model-load step (OpenNSFWInferenceRunner.load()),
    not just `import` — a numpy<2 mismatch or a corrupt/missing ONNX model
    file passes the import check but fails here.
  - From "A": --fix flag that offers to pip-install the right packages
    into the exact interpreter Curator will launch.

Usage:
    python diagnose_nsfw.py                      # auto-find config.json
    python diagnose_nsfw.py D:\\Curator\\config.json  # or point at it directly
    python diagnose_nsfw.py --fix                # offer to install missing deps
"""
import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

REQUIRED_PACKAGES = ["numpy<2", "opennsfw-standalone", "onnxruntime", "Pillow"]
REQUIRED_MODULES = ["numpy", "PIL", "onnxruntime", "opennsfw_standalone"]

# Run inside the TARGET interpreter (not this script's own) so results
# reflect exactly what Curator's worker would see.
PROBE_SCRIPT = r"""
import json, sys
out = {"executable": sys.executable, "version": sys.version.split()[0], "modules": {}}
mods = %r
for m in mods:
    try:
        mod = __import__(m)
        out["modules"][m] = {"ok": True, "version": getattr(mod, "__version__", None)}
    except Exception as e:
        out["modules"][m] = {"ok": False, "error": f"{type(e).__name__}: {e}"}
if out["modules"].get("opennsfw_standalone", {}).get("ok"):
    try:
        from opennsfw_standalone import OpenNSFWInferenceRunner
        OpenNSFWInferenceRunner.load()
        out["model_load"] = {"ok": True}
    except Exception as e:
        out["model_load"] = {"ok": False, "error": f"{type(e).__name__}: {e}"}
print(json.dumps(out))
""" % REQUIRED_MODULES


def probe(exe: str):
    try:
        r = subprocess.run([exe, "-c", PROBE_SCRIPT], capture_output=True, text=True, timeout=60)
    except Exception as e:
        return {"executable": exe, "launch_error": str(e)}
    if r.returncode != 0 or not r.stdout.strip():
        detail = (r.stderr or r.stdout or "no output").strip()[:400]
        return {"executable": exe, "launch_error": detail}
    try:
        return json.loads(r.stdout.strip().splitlines()[-1])
    except Exception:
        return {"executable": exe, "launch_error": f"unparsable output: {r.stdout[:200]!r}"}


def find_config_json(explicit, data_dir):
    if explicit:
        p = Path(explicit)
        return p if p.exists() else None
    candidates = []
    if data_dir:
        candidates.append(Path(data_dir) / "config.json")
    candidates.append(Path.cwd() / "config.json")
    if os.name == "nt":
        for env in ("LOCALAPPDATA", "USERPROFILE"):
            base = os.environ.get(env)
            if base:
                candidates.append(Path(base))
        candidates.append(Path.home() / "Desktop")
        candidates.append(Path.home() / "Downloads")
    for c in candidates:
        if c.is_file() and c.name == "config.json":
            return c
        if c.is_dir():
            for hit in c.glob("**/config.json"):
                try:
                    data = json.loads(hit.read_text())
                except Exception:
                    continue
                if "data_dir" in data or "python_bin" in data or "gallery_dl_bin" in data:
                    return hit
    return None


def find_all_pythons():
    found = {}  # resolved path -> label

    def add(path, label):
        if not path:
            return
        rp = str(Path(path).resolve()) if Path(path).exists() else path
        found.setdefault(rp, label)

    for name in ("python", "python3", "python3.10", "python3.11", "python3.12", "python3.13"):
        add(shutil.which(name), f"PATH:{name}")

    if os.name == "nt":
        try:
            r = subprocess.run(["py", "-0p"], capture_output=True, text=True, timeout=10)
            for line in (r.stdout or "").splitlines():
                line = line.strip()
                if not line or line.startswith("Active"):
                    continue
                for tok in line.split():
                    if tok.lower().endswith("python.exe"):
                        add(tok, "py-launcher")
        except Exception:
            pass
        for base in (
            Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Python",
            Path("C:/Python313"), Path("C:/Python312"), Path("C:/Python311"), Path("C:/Python310"),
        ):
            if base.exists():
                for exe in base.glob("*/python.exe"):
                    add(exe, "well-known-path")
    else:
        import re
        name_re = re.compile(r"^python3(\.\d+)?$")
        for base in ("/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"):
            p = Path(base)
            if p.exists():
                for exe in p.glob("python3*"):
                    if exe.is_file() and name_re.match(exe.name):
                        add(exe, "well-known-path")

    return found


def is_windows_store_stub(exe_path: str) -> bool:
    """Windows' 'app execution alias' python.exe is a tiny stub at
    WindowsApps\\python.exe that either launches the Store or silently
    no-ops — shutil.which() finds it, but it isn't a real interpreter.
    """
    return "WindowsApps" in exe_path and "PythonSoftwareFoundation" not in exe_path


def is_windows_store_package_python(exe_path: str) -> bool:
    """A REAL Python that happens to have been installed via the Microsoft
    Store (path contains .../Packages/PythonSoftwareFoundation.Python...).
    Not broken, but worth flagging since it's a separate, easy-to-forget
    install from a python.org one — pip installs there don't show up
    under a python.org python and vice versa.
    """
    return "Packages" in exe_path and "PythonSoftwareFoundation.Python" in exe_path


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("config_path", nargs="?", help="path to config.json (optional)")
    ap.add_argument("--data-dir", help="Curator's data directory (printed in curator.log)")
    ap.add_argument("--fix", action="store_true", help="offer to pip install the missing packages")
    args = ap.parse_args()

    print("=" * 70)
    print("Curator NSFW worker diagnostic")
    print("=" * 70)

    cfg_path = find_config_json(args.config_path, args.data_dir)
    python_bin_override = None
    if cfg_path:
        print(f"\nFound config.json: {cfg_path}")
        try:
            cfg = json.loads(cfg_path.read_text())
            python_bin_override = cfg.get("python_bin") or None
            print(f"  python_bin override: {python_bin_override!r}" if python_bin_override
                  else "  no python_bin override set (Curator uses PATH default)")
        except Exception as e:
            print(f"  could not parse it: {e}")
    else:
        print("\nNo config.json found automatically. Pass its path as an argument, or")
        print("use --data-dir <dir> (the dir printed at the top of curator.log).")
        print("(harmless to skip — we'll just assume no override)")

    default_bin = "python" if os.name == "nt" else "python3"
    curator_will_run = python_bin_override or default_bin
    print(f"\nCurator will launch: {curator_will_run!r}")

    resolved = shutil.which(curator_will_run)
    if not resolved:
        print(f"  !! '{curator_will_run}' is NOT found on PATH at all.")
        print("     Fix: install Python from https://python.org (check \"Add python.exe")
        print("     to PATH\" during install), or set an explicit path in config.json:")
        print('         { "python_bin": "C:\\\\full\\\\path\\\\to\\\\python.exe" }')
        resolved_note = None
    else:
        print(f"  resolves to: {resolved}")
        if is_windows_store_stub(resolved):
            print()
            print("  !! This is the Windows \"app execution alias\" stub, not a real Python.")
            print("     It's what makes `python` silently open the Microsoft Store (or do")
            print("     nothing) instead of actually running your script. Turn it off at:")
            print("     Settings > Apps > Advanced app settings > App execution aliases")
            print("     — then either install Python from https://python.org, or point")
            print("     config.json at a real interpreter with \"python_bin\".")
        elif is_windows_store_package_python(resolved):
            print()
            print("  Note: this is a Python installed via the Microsoft Store, not")
            print("  python.org. That's fine, but remember it's a SEPARATE install —")
            print("  packages you pip-installed under a python.org Python (or vice")
            print("  versa) won't show up here.")

    print("\nProbing every Python this script can find on the machine...\n")
    pythons = find_all_pythons()
    if resolved:
        pythons.setdefault(str(Path(resolved).resolve()), "curator-target")

    curator_target_path = str(Path(resolved).resolve()) if resolved else None
    good_candidates = []
    target_report = None

    for exe, label in pythons.items():
        marker = "  <-- Curator uses this one" if exe == curator_target_path else ""
        print(f"[{label}] {exe}{marker}")
        res = probe(exe)
        if exe == curator_target_path:
            target_report = res
        if "launch_error" in res:
            print(f"    could not run: {res['launch_error']}")
            print()
            continue
        print(f"    Python {res['version']}")
        all_ok = True
        for m in REQUIRED_MODULES:
            info = res["modules"].get(m, {"ok": False, "error": "not checked"})
            status = "OK" if info["ok"] else "MISSING"
            ver = f" v{info['version']}" if info.get("version") else ""
            print(f"    {m:<22} {status}{ver}")
            if not info["ok"]:
                all_ok = False
            if m == "numpy" and info["ok"] and info.get("version", "0").split(".")[0] not in ("0", "1"):
                print(f"      NOTE: numpy {info['version']} installed, but opennsfw-standalone")
                print("      needs numpy<2 — it fails at import time with numpy 2.x in practice.")
                all_ok = False
        if all_ok and "model_load" in res:
            status = "OK" if res["model_load"]["ok"] else "FAILED"
            print(f"    {'model load':<22} {status}  {res['model_load'].get('error', '')}")
            if not res["model_load"]["ok"]:
                all_ok = False
        if all_ok:
            good_candidates.append(exe)
        print()

    print("=" * 70)
    if not resolved:
        print("Diagnosis: Curator can't find Python at all — see above.")
    elif curator_target_path in good_candidates:
        print("Diagnosis: the interpreter Curator launches already has everything")
        print("(imports AND model load both succeed).")
        print("If curator.log still shows the error, fully restart Curator — the")
        print("worker only spawns once, at startup.")
    else:
        print("Diagnosis: the interpreter Curator launches is missing something.")
        print(f"Curator is launching:\n  {curator_target_path}")
        if good_candidates:
            print("\nBut this Python on your machine already has everything working:")
            for c in good_candidates:
                print(f"  {c}")
            print("\nFix — either:")
            print("  (a) point Curator at that one — merge this into config.json")
            print(f"      (next to curator.exe, don't overwrite data_dir):")
            print(f'      {{ "python_bin": {json.dumps(good_candidates[0])} }}')
            print(f'  (b) or: "{curator_target_path}" -m pip install {" ".join(REQUIRED_PACKAGES)}')
        else:
            print("\nNo Python on this machine has it fully working yet. Install into the")
            print("one Curator actually launches:")
            print(f'  "{curator_target_path}" -m pip install {" ".join(REQUIRED_PACKAGES)}')
        print("\nThen fully restart Curator (the worker only spawns at startup).")
    print("=" * 70)

    if args.fix and resolved:
        print()
        print(f'About to run:\n  "{resolved}" -m pip install {" ".join(REQUIRED_PACKAGES)}')
        if input("Proceed? [y/N] ").strip().lower() == "y":
            subprocess.run([resolved, "-m", "pip", "install"] + REQUIRED_PACKAGES)
            print("\nDone. Re-run this script (without --fix) to confirm, then restart Curator.")
        else:
            print("Skipped.")


if __name__ == "__main__":
    sys.exit(main() or 0)
