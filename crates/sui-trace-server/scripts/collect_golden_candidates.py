#!/usr/bin/env python3
# Copyright (c) Sentio
# SPDX-License-Identifier: Apache-2.0

"""Collect transaction digest candidates for Sentio call trace golden tests."""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable


MAINNET_RPC_URL = "https://fullnode.mainnet.sui.io:443"


@dataclass
class Candidate:
    digest: str
    checkpoint: str
    status: str
    executed_epoch: str
    commands: list[str]
    move_calls: list[dict[str, Any]] = field(default_factory=list)


def main() -> int:
    args = parse_args()
    checkpoints = sample_checkpoints(
        args.checkpoint_start,
        args.checkpoint_end,
        args.max_checkpoints,
    )

    grouped: dict[str, list[Candidate]] = {name: [] for name in category_predicates()}
    seen_digests: set[str] = set()
    client = RpcClient(args.rpc_url, args.timeout)

    for checkpoint in checkpoints:
        try:
            checkpoint_data = client.call("sui_getCheckpoint", [str(checkpoint)])
        except Exception as exc:  # noqa: BLE001 - keep scanning nearby checkpoints.
            print(f"[warn] checkpoint {checkpoint}: {exc}", file=sys.stderr)
            continue

        txs = checkpoint_data.get("transactions", [])[: args.max_txs_per_checkpoint]
        print(f"[scan] checkpoint {checkpoint}: {len(txs)} txs", file=sys.stderr)
        new_txs = [digest for digest in txs if digest not in seen_digests]
        seen_digests.update(new_txs)

        for batch in chunks(new_txs, args.batch_size):
            for digest, tx in fetch_transaction_batch(client, batch):
                candidate = candidate_from_response(digest, tx)
                if args.epoch and candidate.executed_epoch != str(args.epoch):
                    continue
                if not candidate.commands:
                    continue

                for name, predicate in category_predicates().items():
                    if len(grouped[name]) >= args.per_category:
                        continue
                    if predicate(candidate):
                        grouped[name].append(candidate)

        if all(len(values) >= args.per_category for values in grouped.values()):
            break

    print_report(grouped)
    if args.out_json:
        write_json_report(args.out_json, grouped)
    if args.out_digests_file:
        write_digests_file(args.out_digests_file, grouped)
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Scan Sui JSON-RPC checkpoint metadata and group transaction digests "
            "that are useful for Sentio call trace golden comparisons."
        )
    )
    parser.add_argument("--rpc-url", default=MAINNET_RPC_URL)
    parser.add_argument("--checkpoint-start", type=int, required=True)
    parser.add_argument("--checkpoint-end", type=int, required=True)
    parser.add_argument(
        "--epoch",
        type=int,
        help="optional executedEpoch filter; useful for protocol <=111 old baseline ranges",
    )
    parser.add_argument("--max-checkpoints", type=int, default=25)
    parser.add_argument("--max-txs-per-checkpoint", type=int, default=50)
    parser.add_argument(
        "--batch-size",
        type=int,
        default=50,
        help="transaction digests per sui_multiGetTransactionBlocks request",
    )
    parser.add_argument("--per-category", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--out-json", type=Path)
    parser.add_argument(
        "--out-digests-file",
        type=Path,
        help="write a compare_call_trace.py-compatible digest file with category comments",
    )
    return parser.parse_args()


class RpcClient:
    def __init__(self, rpc_url: str, timeout: float) -> None:
        self.rpc_url = rpc_url
        self.timeout = timeout
        self.request_id = 0

    def call(self, method: str, params: list[Any]) -> Any:
        self.request_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self.request_id,
            "method": method,
            "params": params,
        }
        request = urllib.request.Request(
            self.rpc_url,
            data=json.dumps(payload).encode("utf-8"),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            body = json.loads(response.read())
        if "error" in body:
            raise RuntimeError(body["error"])
        return body["result"]


def sample_checkpoints(start: int, end: int, max_count: int) -> list[int]:
    if start > end:
        start, end = end, start
    total = end - start + 1
    if total <= max_count:
        return list(range(start, end + 1))
    if max_count <= 1:
        return [start]
    return sorted(
        {
            start + round(index * (total - 1) / (max_count - 1))
            for index in range(max_count)
        }
    )


def candidate_from_response(digest: str, tx: dict[str, Any]) -> Candidate:
    effects = tx.get("effects", {})
    status = effects.get("status", {}).get("status", "unknown")
    executed_epoch = str(effects.get("executedEpoch", "unknown"))
    commands = transaction_commands(tx)
    return Candidate(
        digest=digest,
        checkpoint=str(tx.get("checkpoint", "unknown")),
        status=status,
        executed_epoch=executed_epoch,
        commands=command_kinds(commands),
        move_calls=move_calls(commands),
    )


def fetch_transaction_batch(
    client: RpcClient,
    digests: list[str],
) -> list[tuple[str, dict[str, Any]]]:
    if not digests:
        return []
    options = {
        "showInput": True,
        "showEffects": True,
    }
    try:
        results = client.call("sui_multiGetTransactionBlocks", [digests, options])
        return [
            (digest, tx)
            for digest, tx in zip(digests, results, strict=False)
            if isinstance(tx, dict)
        ]
    except Exception as exc:  # noqa: BLE001 - fall back to per-tx reads.
        print(
            f"[warn] multiGet {digests[0]}..{digests[-1]}: {exc}; falling back",
            file=sys.stderr,
        )

    fetched: list[tuple[str, dict[str, Any]]] = []
    for digest in digests:
        try:
            tx = client.call("sui_getTransactionBlock", [digest, options])
        except Exception as exc:  # noqa: BLE001 - keep scanning other txs.
            print(f"[warn] tx {digest}: {exc}", file=sys.stderr)
            continue
        if isinstance(tx, dict):
            fetched.append((digest, tx))
    return fetched


def chunks(values: list[str], size: int) -> list[list[str]]:
    size = max(size, 1)
    return [values[index : index + size] for index in range(0, len(values), size)]


def transaction_commands(tx: dict[str, Any]) -> list[Any]:
    try:
        transaction = tx["transaction"]["data"]["transaction"]
    except KeyError:
        return []
    return transaction.get("transactions", [])


def command_kinds(commands: list[Any]) -> list[str]:
    kinds: list[str] = []
    for command in commands:
        if isinstance(command, dict):
            kinds.extend(command.keys())
    return kinds


def move_calls(commands: list[Any]) -> list[dict[str, Any]]:
    calls: list[dict[str, Any]] = []
    for command in commands:
        if isinstance(command, dict) and isinstance(command.get("MoveCall"), dict):
            calls.append(command["MoveCall"])
    return calls


def type_arguments(move_call: dict[str, Any]) -> list[Any]:
    for key in ("type_arguments", "typeArguments", "type_args", "typeArgs"):
        value = move_call.get(key)
        if value:
            return value
    return []


def package_id(move_call: dict[str, Any]) -> str:
    return str(move_call.get("package", move_call.get("packageId", ""))).lower()


def is_system_package(move_call: dict[str, Any]) -> bool:
    package = package_id(move_call).removeprefix("0x").rjust(64, "0")
    return package in {
        "0" * 63 + "1",
        "0" * 63 + "2",
        "0" * 63 + "3",
    }


def category_predicates() -> dict[str, Callable[[Candidate], bool]]:
    return {
        "simple_move_call": lambda c: c.status == "success"
        and c.commands == ["MoveCall"],
        "multi_move_call": lambda c: c.status == "success"
        and c.commands.count("MoveCall") >= 2,
        "generic_type_args": lambda c: c.status == "success"
        and any(type_arguments(call) for call in c.move_calls),
        "sui_framework_or_native_candidate": lambda c: c.status == "success"
        and any(is_system_package(call) for call in c.move_calls),
        "transfer_objects": lambda c: c.status == "success"
        and "TransferObjects" in c.commands,
        "split_or_merge_coins": lambda c: c.status == "success"
        and ("SplitCoins" in c.commands or "MergeCoins" in c.commands),
        "publish": lambda c: c.status == "success" and "Publish" in c.commands,
        "upgrade": lambda c: c.status == "success" and "Upgrade" in c.commands,
        "multi_command_ptb": lambda c: c.status == "success" and len(c.commands) > 1,
        "abort_or_error": lambda c: c.status != "success" and "MoveCall" in c.commands,
    }


def print_report(grouped: dict[str, list[Candidate]]) -> None:
    for category, candidates in grouped.items():
        print(f"\n# {category}")
        if not candidates:
            print("# no candidates found")
            continue
        for candidate in candidates:
            print(candidate_line(candidate))


def candidate_line(candidate: Candidate) -> str:
    return (
        f"{candidate.digest}  # checkpoint={candidate.checkpoint} "
        f"epoch={candidate.executed_epoch} status={candidate.status} "
        f"commands={','.join(candidate.commands)}"
    )


def write_json_report(path: Path, grouped: dict[str, list[Candidate]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        category: [candidate.__dict__ for candidate in candidates]
        for category, candidates in grouped.items()
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_digests_file(path: Path, grouped: dict[str, list[Candidate]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Candidate digests collected by collect_golden_candidates.py.",
        "# Review before adding to the committed golden set.",
        "",
    ]
    written: set[str] = set()
    for category, candidates in grouped.items():
        lines.append(f"# {category}")
        if not candidates:
            lines.append("# no candidates found")
            lines.append("")
            continue
        wrote_category = False
        for candidate in candidates:
            if candidate.digest in written:
                lines.append(f"# already listed: {candidate_line(candidate)}")
                continue
            written.add(candidate.digest)
            lines.append(candidate_line(candidate))
            wrote_category = True
        if not wrote_category:
            lines.append("# covered by digests already listed above")
        lines.append("")
    path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
