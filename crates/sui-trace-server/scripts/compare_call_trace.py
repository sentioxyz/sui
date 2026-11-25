#!/usr/bin/env python3
# Copyright (c) Sentio
# SPDX-License-Identifier: Apache-2.0

"""Compare Sentio call trace JSON responses from old and new trace servers."""

from __future__ import annotations

import argparse
import difflib
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


def main() -> int:
    args = parse_args()
    validate_args(args)
    digests = load_digests(args.digest, args.digests_file)
    if not digests:
        raise SystemExit("no transaction digests provided")

    results: list[dict[str, Any]] = []
    mismatches: list[str] = []
    fetch_failures: list[str] = []
    captured: list[str] = []
    matched: list[str] = []
    for digest in digests:
        print(f"[fetch] {digest}", flush=True)
        digest_dir = output_dir(args.out_dir, digest)

        if args.capture_only:
            trace_json, fetch_error = load_trace(args.capture_only, args, digest)
            if digest_dir:
                write_trace_artifacts(digest_dir, args.capture_only, trace_json, fetch_error)
            if fetch_error:
                fetch_failures.append(digest)
                results.append(
                    {
                        "digest": digest,
                        "status": "fetch_failed",
                        "side": args.capture_only,
                        "error": format_error(fetch_error).strip(),
                    }
                )
                print(
                    f"[error] {args.capture_only} {digest}: {fetch_error}",
                    file=sys.stderr,
                    flush=True,
                )
                continue
            captured.append(digest)
            results.append(
                {
                    "digest": digest,
                    "status": "captured",
                    "side": args.capture_only,
                    "raw_path": str(digest_dir / f"{args.capture_only}.raw.json")
                    if digest_dir
                    else None,
                    "canonical_path": str(
                        digest_dir / f"{args.capture_only}.canonical.json"
                    )
                    if digest_dir
                    else None,
                }
            )
            print(f"[ok] captured {args.capture_only} {digest}", flush=True)
            continue

        old_json, old_error = load_trace("old", args, digest)
        new_json, new_error = load_trace("new", args, digest)

        if digest_dir:
            write_trace_artifacts(digest_dir, "old", old_json, old_error)
            write_trace_artifacts(digest_dir, "new", new_json, new_error)

        if old_error or new_error:
            fetch_failures.append(digest)
            errors = {}
            if old_error:
                errors["old"] = format_error(old_error).strip()
                print(f"[error] old {digest}: {old_error}", file=sys.stderr, flush=True)
            if new_error:
                errors["new"] = format_error(new_error).strip()
                print(f"[error] new {digest}: {new_error}", file=sys.stderr, flush=True)
            results.append(
                {
                    "digest": digest,
                    "status": "fetch_failed",
                    "errors": errors,
                }
            )
            continue

        assert old_json is not None
        assert new_json is not None
        old_canonical = canonical_json(old_json)
        new_canonical = canonical_json(new_json)

        if old_canonical == new_canonical:
            matched.append(digest)
            results.append({"digest": digest, "status": "matched"})
            print(f"[ok] {digest}", flush=True)
            continue

        mismatches.append(digest)
        diff_path = digest_dir / "diff.patch" if digest_dir else None
        diff, diff_truncated = limited_unified_diff(
            pretty_json(old_json).splitlines(),
            pretty_json(new_json).splitlines(),
            fromfile=f"old/{digest}",
            tofile=f"new/{digest}",
            max_lines=args.max_diff_lines,
        )
        print(diff, flush=True)
        if diff_path:
            diff_path.write_text(diff + "\n", encoding="utf-8")
        results.append(
            {
                "digest": digest,
                "status": "mismatched",
                "diff_path": str(diff_path) if diff_path else None,
                "diff_truncated": diff_truncated,
            }
        )

    write_summary(
        args.summary_json,
        {
            "mode": "capture" if args.capture_only else "compare",
            "capture_side": args.capture_only,
            "chain_id": args.chain_id,
            "total": len(digests),
            "matched": matched,
            "mismatched": mismatches,
            "fetch_failed": fetch_failures,
            "captured": captured,
            "results": results,
        },
    )

    if fetch_failures:
        print(
            f"[fail] {len(fetch_failures)} / {len(digests)} digests could not be fetched: "
            + ", ".join(fetch_failures),
            file=sys.stderr,
        )
    if mismatches:
        print(
            f"[fail] {len(mismatches)} / {len(digests)} digests differed: "
            + ", ".join(mismatches),
            file=sys.stderr,
        )
    if fetch_failures or mismatches:
        return 1

    if args.capture_only:
        print(f"[ok] captured all {len(digests)} {args.capture_only} digests")
        return 0

    print(f"[ok] all {len(digests)} digests matched")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Fetch call trace responses from an old and new sentio tracer server, "
            "canonicalize JSON, and fail on any semantic diff."
        )
    )
    parser.add_argument("--old-base-url", help="old tracer base URL")
    parser.add_argument("--new-base-url", help="new tracer base URL")
    parser.add_argument(
        "--old-json-dir",
        type=Path,
        help=(
            "directory with saved old JSON outputs; accepts files produced by --out-dir "
            "or <digest>.json files"
        ),
    )
    parser.add_argument(
        "--new-json-dir",
        type=Path,
        help=(
            "directory with saved new JSON outputs; accepts files produced by --out-dir "
            "or <digest>.json files"
        ),
    )
    parser.add_argument("--chain-id", default="sui_mainnet", help="trace server chain id")
    parser.add_argument(
        "--digest",
        action="append",
        default=[],
        help="transaction digest to compare; may be passed more than once",
    )
    parser.add_argument(
        "--digests-file",
        type=Path,
        help="file with one transaction digest per line; blank lines and # comments are ignored",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        help="optional directory for raw JSON, canonical JSON, and diffs",
    )
    parser.add_argument(
        "--summary-json",
        type=Path,
        help="optional machine-readable summary of matched, mismatched, and failed digests",
    )
    parser.add_argument(
        "--capture-only",
        choices=["old", "new"],
        help="only fetch and save one side; requires --out-dir and the matching source",
    )
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--retries", type=int, default=0)
    parser.add_argument("--retry-delay", type=float, default=2.0)
    parser.add_argument(
        "--max-diff-lines",
        type=int,
        default=2000,
        help="maximum unified diff lines to print/write for each mismatch; <=0 means unlimited",
    )
    return parser.parse_args()


def validate_args(args: argparse.Namespace) -> None:
    old_sources = [args.old_base_url is not None, args.old_json_dir is not None]
    new_sources = [args.new_base_url is not None, args.new_json_dir is not None]
    if args.capture_only:
        if args.out_dir is None:
            raise SystemExit("--capture-only requires --out-dir")
        if args.capture_only == "old":
            if sum(old_sources) != 1:
                raise SystemExit("provide exactly one of --old-base-url or --old-json-dir")
            if sum(new_sources) != 0:
                raise SystemExit("--capture-only old does not accept new sources")
        else:
            if sum(new_sources) != 1:
                raise SystemExit("provide exactly one of --new-base-url or --new-json-dir")
            if sum(old_sources) != 0:
                raise SystemExit("--capture-only new does not accept old sources")
    else:
        if sum(old_sources) != 1:
            raise SystemExit("provide exactly one of --old-base-url or --old-json-dir")
        if sum(new_sources) != 1:
            raise SystemExit("provide exactly one of --new-base-url or --new-json-dir")


def load_digests(cli_digests: list[str], digests_file: Path | None) -> list[str]:
    digests: list[str] = []
    digests.extend(digest.strip() for digest in cli_digests if digest.strip())
    if digests_file:
        for line in digests_file.read_text(encoding="utf-8").splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                digests.append(line)
    return list(dict.fromkeys(digests))


def load_trace(
    kind: str,
    args: argparse.Namespace,
    digest: str,
) -> tuple[Any | None, Exception | None]:
    try:
        json_dir = args.old_json_dir if kind == "old" else args.new_json_dir
        if json_dir:
            return read_saved_json(json_dir, kind, digest), None

        base_url = args.old_base_url if kind == "old" else args.new_base_url
        assert base_url is not None
        return (
            fetch_with_retries(
                call_trace_url(base_url, args.chain_id, digest),
                args.timeout,
                args.retries,
                args.retry_delay,
            ),
            None,
        )
    except Exception as exc:  # noqa: BLE001 - preserve the exact source failure.
        return None, exc


def output_dir(base_dir: Path | None, digest: str) -> Path | None:
    if base_dir is None:
        return None
    digest_dir = base_dir / safe_path_name(digest)
    digest_dir.mkdir(parents=True, exist_ok=True)
    return digest_dir


def write_trace_artifacts(
    digest_dir: Path,
    kind: str,
    value: Any | None,
    error: Exception | None,
) -> None:
    if value is not None:
        write_json(digest_dir / f"{kind}.raw.json", value, pretty=True)
        (digest_dir / f"{kind}.canonical.json").write_text(
            canonical_json(value), encoding="utf-8"
        )
    if error:
        (digest_dir / f"{kind}.fetch-error.txt").write_text(
            format_error(error), encoding="utf-8"
        )


def read_saved_json(json_dir: Path, kind: str, digest: str) -> Any:
    safe_digest = safe_path_name(digest)
    candidates = [
        json_dir / safe_digest / f"{kind}.raw.json",
        json_dir / safe_digest / f"{kind}.json",
        json_dir / safe_digest / "raw.json",
        json_dir / f"{safe_digest}.{kind}.json",
        json_dir / f"{safe_digest}.json",
    ]
    for candidate in candidates:
        if candidate.exists():
            return json.loads(candidate.read_text(encoding="utf-8"))
    searched = ", ".join(str(candidate) for candidate in candidates)
    raise FileNotFoundError(f"no saved {kind} JSON for {digest}; searched: {searched}")


def call_trace_url(base_url: str, chain_id: str, digest: str) -> str:
    base = base_url.rstrip("/")
    chain = urllib.parse.quote(chain_id, safe="")
    tx = urllib.parse.quote(digest, safe="")
    return f"{base}/{chain}/call_trace/by_tx_digest/{tx}"


def fetch_with_retries(url: str, timeout: float, retries: int, retry_delay: float) -> Any:
    attempts = retries + 1
    last_error: Exception | None = None
    for attempt in range(1, attempts + 1):
        try:
            return fetch_json(url, timeout)
        except Exception as exc:  # noqa: BLE001 - surface the final fetch error verbatim.
            last_error = exc
            if attempt == attempts:
                break
            time.sleep(retry_delay)
    assert last_error is not None
    raise last_error


def fetch_json(url: str, timeout: float) -> Any:
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
    except urllib.error.HTTPError as err:
        body = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{url} returned HTTP {err.code}: {body}") from err
    return json.loads(body)


def canonical_json(value: Any) -> str:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ) + "\n"


def pretty_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2)


def limited_unified_diff(
    old_lines: list[str],
    new_lines: list[str],
    *,
    fromfile: str,
    tofile: str,
    max_lines: int,
) -> tuple[str, bool]:
    diff_lines: list[str] = []
    truncated = False
    diff = difflib.unified_diff(
        old_lines,
        new_lines,
        fromfile=fromfile,
        tofile=tofile,
        lineterm="",
    )
    for index, line in enumerate(diff):
        if max_lines > 0 and index >= max_lines:
            truncated = True
            break
        diff_lines.append(line)
    if truncated:
        diff_lines.append(
            f"... diff truncated after {max_lines} lines; inspect raw/canonical JSON artifacts."
        )
    return "\n".join(diff_lines), truncated


def write_json(path: Path, value: Any, *, pretty: bool) -> None:
    text = pretty_json(value) if pretty else canonical_json(value)
    path.write_text(text + "\n", encoding="utf-8")


def write_summary(path: Path | None, summary: dict[str, Any]) -> None:
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    write_json(path, summary, pretty=True)


def format_error(error: Exception) -> str:
    return f"{type(error).__name__}: {error}\n"


def safe_path_name(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in value)


if __name__ == "__main__":
    raise SystemExit(main())
