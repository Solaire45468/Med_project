#!/usr/bin/env python3
"""Repair stale Tauri plugin permission references in source configuration."""
from __future__ import annotations

import json
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src-tauri"
TARGET = SRC / "target"
BAD = "opener:default"


def main() -> int:
    if not SRC.is_dir():
        print(f"ERROR: {SRC} not found")
        return 2

    changed = False
    found: list[Path] = []

    for path in SRC.rglob("*"):
        if not path.is_file() or TARGET in path.parents or ".git" in path.parts or "gen" in path.parts:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if BAD not in text:
            continue
        found.append(path)
        if path.suffix.lower() in {".json", ".json5"}:
            try:
                data = json.loads(text)
            except json.JSONDecodeError as exc:
                print(f"ERROR: invalid JSON in {path}: {exc}")
                return 3
            if isinstance(data, dict) and isinstance(data.get("permissions"), list):
                new_permissions = [item for item in data["permissions"] if item != BAD]
                if new_permissions != data["permissions"]:
                    data["permissions"] = new_permissions
                    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                    changed = True
        else:
            print(f"ERROR: stale permission found in non-JSON source file: {path}")
            return 4

    if found:
        print("Found stale opener:default reference(s):")
        for path in sorted(set(found)):
            print(f"  {path}")
    else:
        print("No opener:default references found.")

    remaining = []
    for path in SRC.rglob("*"):
        if not path.is_file() or TARGET in path.parents or ".git" in path.parts or "gen" in path.parts:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if BAD in text:
            remaining.append(path)

    if remaining:
        print("ERROR: stale permission still remains:")
        for path in remaining:
            print(f"  {path}")
        return 5

    if changed and TARGET.exists():
        print("Removing stale src-tauri/target ...")
        shutil.rmtree(TARGET)

    print("OK: Tauri source capabilities are clean.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
