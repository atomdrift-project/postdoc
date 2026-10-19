#!/usr/bin/env python3
"""Emit deterministic level decisions for server-response parity checks."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("response_dir", type=pathlib.Path)
    parser.add_argument("--content", action="store_true",
                        help="also hash complete raw findings and ML output, excluding only ML analyzed_at")
    args = parser.parse_args()
    response_dir = args.response_dir
    fingerprint = []
    for path in sorted(response_dir.glob("*.json")):
        text = path.read_text(encoding="utf-8")
        try:
            response = json.loads(text)
        except json.JSONDecodeError:
            # v1 switches long requests to NDJSON progress plus a final result.
            messages = [json.loads(line) for line in text.splitlines() if line.strip()]
            if not messages or messages[-1].get("state") == "analyzing":
                raise ValueError(f"{path}: stream ended without a terminal result")
            response = messages[-1]
        ml = response.get("ml")
        if not isinstance(ml, dict):
            # Worker result dumps carry an `error` instead of `ml` when the
            # analysis failed; that outcome is part of the fingerprint.
            error = response.get("error")
            if error is None:
                raise ValueError(f"{path}: response has no ml object")
            fingerprint.append(
                {"file": path.name.removesuffix(".json"), "error": error}
            )
            continue
        content = {}
        if args.content:
            raw = response.get("raw")
            if not isinstance(raw, dict):
                raise ValueError(f"{path}: --content requires a raw report")
            for name, value in (("raw", raw), ("ml", {k: v for k, v in ml.items() if k != "analyzed_at"})):
                encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
                content[name + "_sha256"] = hashlib.sha256(encoded).hexdigest()
        fingerprint.append(
            {
                "file": path.name.removesuffix(".json"),
                **content,
                "lvl": ml.get("lvl"),
                "files": [
                    {
                        "id": member.get("id"),
                        "type": member.get("type"),
                        # Missing means the member was not evaluated. Preserve
                        # that distinction from a manual-threshold null.
                        **({"lvl": member["lvl"]} if "lvl" in member else {}),
                    }
                    for member in ml.get("files", [])
                ],
            }
        )

    json.dump(fingerprint, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
