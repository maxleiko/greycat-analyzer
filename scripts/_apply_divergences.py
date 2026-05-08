#!/usr/bin/env python3
"""P18.3 — apply the parity allow-list (`tests/parity/divergences.toml`)
to a JSONL stream of records (TS or Rust `dump-types` output).

For every record, walk the allow-list. If the record matches a TS-side
or Rust-side shape, rewrite its `type` field to the agreed-upon
canonical form (usually the Rust shape). Otherwise, pass it through
unchanged.

The result is that both TS and Rust streams converge to the same
canonical text where the divergence is intentional, and the gauntlet
diff only fires on *unintended* drift.

Allow-list TOML schema:

    # tests/parity/divergences.toml
    # One [[entry]] per intentional divergence.
    [[entry]]
    kind = "InstanceAccessExpr"
    rust_type = "core::function | null"
    ts_type   = "core::int"
    canonical_type = "core::function | null"  # optional; defaults to rust_type
    reason    = "method-ref typing — methods are first-class function values"

Match logic:
- `kind` is required and must match the record's `kind`.
- If `rust_type` is set and the record's `type` equals it, the record
  matches.
- If `ts_type` is set and the record's `type` equals it, the record
  matches.
- If `match_by_position` is `true` (a special widening flag), the
  filter looks at *both* streams in a pre-pass: any record with
  rust_type matching is registered by `(file, range, kind)`, and any
  TS record with the same (file, range, kind) is rewritten to
  canonical regardless of its `type`. This is what we use for
  TS-side auto-evaluations that produce different `type` fields per
  call site (e.g. bare-fn-name idents → return-type-of-call).

Usage:
    _apply_divergences.py <divergences.toml> [--rust-pre <jsonl>] < input.jsonl > output.jsonl
"""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

try:
    import tomllib  # py3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(f"usage: {argv[0]} <divergences.toml> [--rust-pre <jsonl>]", file=sys.stderr)
        return 2
    cfg_path = Path(argv[1])
    if not cfg_path.is_file():
        for line in sys.stdin:
            sys.stdout.write(line)
        return 0
    with cfg_path.open("rb") as fh:
        cfg = tomllib.load(fh)
    entries: list[dict[str, Any]] = cfg.get("entry", [])

    # --rust-pre: pre-load the Rust JSONL so position-based widening
    # entries can register `(file, range, kind)` keys to rewrite on
    # both streams. The driver scripts pass this when filtering the
    # TS output; for the Rust filter pass it can be omitted (since
    # the Rust types are already canonical).
    rust_position_keys: set[tuple[str, int, int, str]] = set()
    rust_pre_path: Path | None = None
    if "--rust-pre" in argv:
        idx = argv.index("--rust-pre")
        if idx + 1 < len(argv):
            rust_pre_path = Path(argv[idx + 1])
    if rust_pre_path and rust_pre_path.is_file():
        position_widening_entries = [
            ent for ent in entries if ent.get("match_by_position") is True
        ]
        if position_widening_entries:
            with rust_pre_path.open() as fh:
                for raw in fh:
                    line = raw.rstrip("\n")
                    if not line:
                        continue
                    try:
                        rec = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    kind = rec.get("kind")
                    ty = rec.get("type")
                    for ent in position_widening_entries:
                        ek = ent.get("kind")
                        et_rust = ent.get("rust_type")
                        if ek and ek != kind:
                            continue
                        if et_rust and ty != et_rust:
                            continue
                        rng = rec.get("range")
                        f = rec.get("file")
                        if isinstance(rng, list) and len(rng) == 2 and isinstance(f, str):
                            rust_position_keys.add((f, int(rng[0]), int(rng[1]), kind))
                        break

    for raw in sys.stdin:
        line = raw.rstrip("\n")
        if not line:
            sys.stdout.write("\n")
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            sys.stdout.write(raw)
            continue
        kind = rec.get("kind")
        ty = rec.get("type")
        rng = rec.get("range")
        f = rec.get("file")
        rewrote = False
        # Position-widening: if this record's (file, range, kind)
        # appears in the Rust pre-pass set, rewrite to canonical.
        if isinstance(rng, list) and len(rng) == 2 and isinstance(f, str):
            key = (f, int(rng[0]), int(rng[1]), kind or "")
            if key in rust_position_keys:
                # Find the canonical from a position-widening entry.
                for ent in entries:
                    if not ent.get("match_by_position"):
                        continue
                    if ent.get("kind") != kind:
                        continue
                    canon = ent.get("canonical_type") or ent.get("rust_type")
                    if canon:
                        rec["type"] = canon
                        rec["nullable"] = bool(
                            isinstance(canon, str) and canon.endswith("| null")
                        )
                        rewrote = True
                        break
        if not rewrote:
            for ent in entries:
                ek = ent.get("kind")
                et_ts = ent.get("ts_type")
                et_rust = ent.get("rust_type")
                canon = ent.get("canonical_type", et_rust)
                if ek and ek != kind:
                    continue
                if ty == et_ts or ty == et_rust:
                    rec["type"] = canon
                    rec["nullable"] = bool(
                        canon and isinstance(canon, str) and canon.endswith("| null")
                    )
                    break
        sys.stdout.write(json.dumps(rec, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
