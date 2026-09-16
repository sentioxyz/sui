# sui-trace-server

Sentio-compatible HTTP wrapper around upstream Sui replay tracing.

The public endpoint is kept compatible with the previous Sentio tracer:

```text
/{chain_id}/call_trace/by_tx_digest/{hash}
```

The implementation replays the transaction with `sui-replay-2 --trace`, reads
the generated `trace.json.zst` through `MoveTraceReader`, and converts upstream
Move trace events into the previous `CallTraceWithSource` JSON shape.

Replay execution is isolated in a child invocation of the same binary. This
keeps the HTTP server alive if upstream replay hits a storage/GQL error path
that panics or aborts while unwinding; the parent process returns the existing
`{ "error": ... }` response shape instead.

## Running

Default network configuration is compatible with the old server:

```sh
cargo run -p sui-trace-server -- "sui_mainnet=https://fullnode.mainnet.sui.io:443"
```

The default port is still `9301`. For golden comparisons, run the migrated
server on a second port:

```sh
SUI_TRACE_SERVER_PORT=9302 cargo run -p sui-trace-server -- \
  "sui_mainnet=https://fullnode.mainnet.sui.io:443"
```

The Docker build wiring is available at `docker/sui-trace-server`:

```sh
docker/sui-trace-server/build.sh
```

The build defaults to the `release` Cargo profile. Override it for local image
checks with `PROFILE=dev docker/sui-trace-server/build.sh`.

Known fullnode and GraphQL mainnet/testnet URLs are mapped to the corresponding
`sui-data-store::Node` variants. If the chain id looks like mainnet/testnet and
the endpoint is a custom JSON-RPC URL, the migrated server uses a hybrid archive
store: transaction, epoch, setup, and checkpoint reads stay on the upstream
mainnet/testnet GraphQL node, while exact-version and root-version object reads
use the configured archive JSON-RPC endpoint. To force a custom GraphQL endpoint,
pass a URL containing `/graphql` or prefix it with `gql:`.

## Migration Audit

Migrated surface:

- `sui-trace-server` HTTP API with the old
  `/{chain_id}/call_trace/by_tx_digest/{hash}` route.
- The old public `CallTraceWithSource` / `CallTraceError` JSON shape.
- Sentio-compatible value formatting for trace v2 structs, integers, addresses,
  vectors, and variants.
- A non-invasive PTB sidecar for `TransferObjects` pseudo traces, using upstream
  `Transfer` trace events plus replayed `transaction_data.json` recipients when
  the recipient is an input pure address. Transfer coin inputs are flattened to
  the old `{ id, balance }` trace-v2 shape.
- A conservative `TransferObjects` fallback for empty/missing trace roots that
  can reconstruct direct input/gas coin values from replay-readable historical
  objects. Unsupported object arguments are emitted as the old unknown value
  marker (`"?"`) instead of inventing a precise value.
- Docker build wiring for `ghcr.io/sentioxyz/sui-tracer:latest`.
- Golden comparison and candidate collection scripts.

Intentionally not carried forward:

- Old invasive Move VM / execution-engine `call_trace` patches.
- Old `LocalExec::replay_with_network_config_for_trace` plumbing.
- Old VM-side `InternalCallTrace` accumulation.

The migrated implementation uses upstream `sui-replay-2` tracing, reads
`trace.json.zst` with `MoveTraceReader`, and converts upstream events into the
old public JSON shape. The replay call is executed in an isolated child process
so upstream replay panics do not terminate the HTTP server. The only VM-level
change kept in this branch is a generic upstream trace event fix: `WriteRef` and
vector mutation instructions emit write events with exact post-write root
snapshots, including `VecPopBack`. It does not inspect package, module, or
function names.

Current golden status:

- New server smoke replay succeeded for current mainnet and protocol 111
  historical transactions.
- Strict old-vs-new golden comparison matches the five representative protocol
  111 mainnet transactions below exactly after canonical JSON normalization,
  including the migrated server's archive JSON-RPC object-read path:
  - `2pgjjduPkw1B7b34KqwDeSuCdej9XSCgka6pzZMjWEHj`
  - `ADWn1eLak5DndiAr12PNY9mYjchALvzwfwhDS61cE8pj`
  - `3zC1bpHicPuQuu7KWBpFvbqzJmUmfF7GrVdU3Tg2Q7HB`
  - `CWuBtNaquFCQvTqSQNx79vz6hkZMR627pP1GnE9Dg9Ap`
  - `DmsyTa7ofMgrKjc1KzVc3zzZy5oG2UJjRcR1gRUbbHjq`
- The matched set covers successful Move traces, nested/generic/native-heavy
  execution, PTB/storage-state reads, staking-state reads, and an abort/error
  trace with legacy gas/error formatting.
- `golden-digests.example.txt` contains additional protocol 111 candidates for
  simple MoveCall, multi MoveCall, generic type args, Sui framework/native
  candidates, TransferObjects, SplitCoins/MergeCoins, multi-command PTBs, and
  abort/error cases.
- No mismatches remain in the five-digest regression set above. A final focused
  archive replay also matched those five digests plus `C2`, `E9`, and `9a`
  against isolated old-output captures. Final release acceptance should still
  expand the strict comparison across more categories in
  `golden-digests.example.txt`, especially pure non-Move PTBs, `Publish`/init,
  and `Upgrade` if protocol-compatible candidates are available. The old
  baseline must run against an archive/full-history JSON-RPC endpoint because
  the public mainnet fullnode can prune historical object versions needed by
  `sentio-260210`.
- Additional focused archive comparisons now match for
  `C2szS2WkUGeaht6sdcy1Nr9ZTBSFw4JwPAojtkcmQpej` and
  `E9ZFeq9L2oCW36eaXcdHoZDGzDYzuAGCRMjxUopEngQ6`, and
  `9aJTH2v9HpU8JufsniTDFZBRd9XcuCGAneQYLycuMCib`. Those matches are covered by
  generic instruction-level memory replay and the VM trace event fixes above,
  not package/function-specific compatibility code. Older saved E9 and 9a
  baselines showed stale object snapshots from the old server's shared local
  object cache, so final acceptance should prefer fresh old-server captures per
  digest or otherwise isolate the old server between digest captures.
- A wider 15-digest protocol 111 set also matches strict old-vs-new comparison.
  It covers pure `TransferObjects`, pure non-Move Split/Merge/Transfer PTBs,
  MakeMoveVec/result-derived coin vectors, non-coin transfer object values,
  complex PTB failure traces, and heavy router/cell/skip_list traces. These
  digests are included in `golden-digests.example.txt`. The heaviest case needs
  a longer compare timeout.

## Golden Comparison

Use two worktrees so the old baseline and migrated branch can run side by side:

```sh
git worktree add ../sui-old-trace sentio-260210

(cd ../sui-old-trace && \
  cargo run -p sui-trace-server -- \
  "sui_mainnet=https://fullnode.mainnet.sui.io:443")

SUI_TRACE_SERVER_PORT=9302 cargo run -p sui-trace-server -- \
  "sui_mainnet=https://fullnode.mainnet.sui.io:443"
```

The old `sentio-260210` baseline only supports protocol versions up to 111.
Golden digests must therefore be historical transactions from protocol 111 or
earlier. A normal public fullnode may not be enough for the old baseline,
because old `sui-replay` needs historical object versions through JSON-RPC and
public nodes can return `ObjectVersionNotFound` for pruned data. Use a
full-history/archive JSON-RPC endpoint for the old server when running the final
acceptance comparison.

Then compare the same transaction digests:

```sh
python3 crates/sui-trace-server/scripts/compare_call_trace.py \
  --old-base-url http://127.0.0.1:9301 \
  --new-base-url http://127.0.0.1:9302 \
  --chain-id sui_mainnet \
  --digests-file crates/sui-trace-server/golden-digests.example.txt \
  --out-dir /tmp/sui-trace-golden \
  --summary-json /tmp/sui-trace-golden/summary.json
```

If the archive/full-history endpoint is only available in another environment,
save old outputs there first with the same `--out-dir` layout, then compare the
saved files locally:

```sh
python3 crates/sui-trace-server/scripts/compare_call_trace.py \
  --capture-only old \
  --old-base-url http://127.0.0.1:9301 \
  --chain-id sui_mainnet \
  --digests-file crates/sui-trace-server/golden-digests.example.txt \
  --out-dir /tmp/sui-trace-old-capture \
  --summary-json /tmp/sui-trace-old-capture/summary.json

python3 crates/sui-trace-server/scripts/compare_call_trace.py \
  --old-json-dir /tmp/sui-trace-old-capture \
  --new-base-url http://127.0.0.1:9302 \
  --chain-id sui_mainnet \
  --digests-file crates/sui-trace-server/golden-digests.example.txt \
  --out-dir /tmp/sui-trace-golden \
  --summary-json /tmp/sui-trace-golden/summary.json
```

The script saves raw JSON, canonical JSON, and a unified diff per digest. The
acceptance rule is strict equality after canonical JSON normalization with
sorted object keys. Fetch failures are saved as `old.fetch-error.txt` or
`new.fetch-error.txt` in the digest output directory. When `--summary-json` is
provided, it also writes a machine-readable status list for every digest.

The same side-by-side flow can be run with one command after creating the old
worktree. The old baseline hard-codes port `9301`, so the runner starts the
migrated server on `9302` and requires the old server's archive/full-history
JSON-RPC endpoint explicitly:

```sh
OLD_NETWORK_CONFIG="sui_mainnet=https://archive-rpc.example" \
  crates/sui-trace-server/scripts/run_golden_compare.sh
```

Useful overrides are `OLD_REPO`, `NEW_NETWORK_CONFIG`, `DIGESTS_FILE`,
`OUT_DIR`, `TIMEOUT`, `RETRIES`, and `RETRY_DELAY`. Logs are written under
`$OUT_DIR/logs`, and comparison artifacts plus `summary.json` are written under
`$OUT_DIR`.

If an archive provider requires request headers that the old baseline cannot
configure, run the old server through the local JSON-RPC proxy:

```sh
python3 crates/sui-trace-server/scripts/json_rpc_proxy.py \
  --upstream https://archive-rpc.example \
  --port 9399

OLD_NETWORK_CONFIG="sui_mainnet=http://127.0.0.1:9399" \
  crates/sui-trace-server/scripts/run_golden_compare.sh
```

For secret-bearing archive URLs, prefer the environment variable form so the URL
does not appear in the proxy process arguments:

```sh
SUI_TRACE_PROXY_UPSTREAM="https://archive-rpc.example" \
  python3 crates/sui-trace-server/scripts/json_rpc_proxy.py --port 9399
```

To expand the golden set, scan transaction metadata from a checkpoint range and
group candidate digests by PTB shape:

```sh
python3 crates/sui-trace-server/scripts/collect_golden_candidates.py \
  --checkpoint-start 248414600 \
  --checkpoint-end 248753200 \
  --epoch 1049 \
  --max-checkpoints 50 \
  --max-txs-per-checkpoint 40 \
  --batch-size 50 \
  --out-digests-file /tmp/sui-trace-candidates.txt
```

For `sentio-260210`, keep collected candidates at protocol 111 or earlier. The
script only reads JSON-RPC metadata; replaying old outputs still requires the
archive/full-history endpoint described above.

## Compatibility Notes

The old trace server called `execute_call_trace(..., enable_trace_v2 = true)`.
The converter therefore emits struct values as `{ "type": ..., "fields": ... }`
and keeps integer/string/address/vector formatting aligned with the old
converter.

The old static PTB path created a `transfer_objects` pseudo trace for
`TransferObjects`. The migrated converter reconstructs that root from upstream
`Transfer` external events and `transaction_data.json` when the recipient is an
input pure address. If upstream replay returns no Move trace roots, the wrapper
also attempts a conservative fallback from `transaction_data.json` plus
historical object reads for direct input/gas coin objects. More complex
recipient or object arguments (`Result`/`NestedResult`) are intentionally
represented with the old unknown value marker until a golden diff proves how
they should be recreated without VM-level patches.

Current parity risks to verify with golden outputs:

- `pc` is derived from the parent frame's last instruction event.
- `gasUsed` is derived from open/close frame `gas_left`; this matches the
  tested abort/error golden case but should remain covered by regression tests.
- Error traces parse upstream's execution error string back into the old
  `CallTraceError` shape; this matches the tested abort/error golden case but
  should remain covered by regression tests.
- Stateful by-reference traces rely on VM trace write events plus a generic
  converter-side memory replay keyed by trace locations. Keep expanded golden
  coverage around nested mutable structs before changing that replay logic.
- `TransferObjects` pseudo traces have exact coin flattening for upstream
  `Transfer` events and direct input/gas coin fallback values. Non-coin
  transferred objects may still differ because the old path stringified
  annotated Move values for those inputs.
- Pure non-Move PTBs that transfer `Result`/`NestedResult` objects can still
  contain `"?"` placeholders. Exact old compatibility for those cases likely
  needs either a small PTB result-value simulator for SplitCoins/MergeCoins or a
  replay artifact that exposes the old object values without VM-level patches.
- Other non-Move PTB pseudo traces, if any old clients depended on them, are not
  emitted yet.
- Early execution errors can skip replay execution and may not produce a trace
  artifact.
