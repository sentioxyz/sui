#!/usr/bin/env bash
# Copyright (c) Sentio
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

OLD_REPO="${OLD_REPO:-$REPO_ROOT/../sui-old-trace}"
CHAIN_ID="${CHAIN_ID:-sui_mainnet}"
OLD_NETWORK_CONFIG="${OLD_NETWORK_CONFIG:-${OLD_NETWORKS:-}}"
NEW_NETWORK_CONFIG="${NEW_NETWORK_CONFIG:-${NEW_NETWORKS:-${OLD_NETWORK_CONFIG:-sui_mainnet=https://fullnode.mainnet.sui.io:443}}}"
DIGESTS_FILE="${DIGESTS_FILE:-$REPO_ROOT/crates/sui-trace-server/golden-digests.example.txt}"
OUT_DIR="${OUT_DIR:-/tmp/sui-trace-golden}"
SUMMARY_JSON="${SUMMARY_JSON:-$OUT_DIR/summary.json}"
LOG_DIR="${LOG_DIR:-$OUT_DIR/logs}"
OLD_PORT="${OLD_PORT:-9301}"
NEW_PORT="${NEW_PORT:-9302}"
WAIT_SECONDS="${WAIT_SECONDS:-600}"
TIMEOUT="${TIMEOUT:-120}"
RETRIES="${RETRIES:-1}"
RETRY_DELAY="${RETRY_DELAY:-5}"
OLD_CARGO_TARGET_DIR="${OLD_CARGO_TARGET_DIR:-$OLD_REPO/target}"
NEW_CARGO_TARGET_DIR="${NEW_CARGO_TARGET_DIR:-$REPO_ROOT/target}"

OLD_PID=""
NEW_PID=""

if [[ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]]; then
  GCC_STDBOOL="$(find /usr/lib/gcc -name stdbool.h -print 2>/dev/null | sort -V | tail -n 1 || true)"
  if [[ -n "$GCC_STDBOOL" ]]; then
    export BINDGEN_EXTRA_CLANG_ARGS="-I$(dirname "$GCC_STDBOOL")"
  fi
fi

usage() {
  cat <<'EOF'
Run the old sentio-260210 tracer and the migrated tracer, then compare call trace JSON.

Required:
  OLD_NETWORK_CONFIG  Old-server network config. Use an archive/full-history JSON-RPC URL.

Optional:
  OLD_REPO            Old baseline worktree. Default: ../sui-old-trace
  NEW_NETWORK_CONFIG  New-server network config. Default: OLD_NETWORK_CONFIG, then public mainnet fullnode
  DIGESTS_FILE        Digest list. Default: crates/sui-trace-server/golden-digests.example.txt
  OUT_DIR             Output directory. Default: /tmp/sui-trace-golden
  CHAIN_ID            Trace server chain id. Default: sui_mainnet

Example:
  OLD_NETWORK_CONFIG="sui_mainnet=https://archive-rpc.example" \
    crates/sui-trace-server/scripts/run_golden_compare.sh
EOF
}

fail() {
  echo "error: $*" >&2
  exit 2
}

cleanup() {
  for pid in "$NEW_PID" "$OLD_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  wait "$NEW_PID" "$OLD_PID" 2>/dev/null || true
}

trap cleanup EXIT
trap 'trap - EXIT; cleanup; exit 130' INT
trap 'trap - EXIT; cleanup; exit 143' TERM

port_in_use() {
  python3 - "$1" <<'PY'
import socket
import sys

port = int(sys.argv[1])
sock = socket.socket()
try:
    sock.settimeout(0.5)
    sock.connect(("127.0.0.1", port))
except OSError:
    sys.exit(1)
finally:
    sock.close()
PY
}

ensure_port_free() {
  local port="$1"
  local name="$2"
  if port_in_use "$port"; then
    fail "$name port $port is already in use"
  fi
}

build_server() {
  local name="$1"
  local repo="$2"
  local target_dir="$3"

  echo "[build] $name server in $repo" >&2
  mkdir -p "$target_dir"
  if ! (
    cd "$repo"
    CARGO_TARGET_DIR="$target_dir" cargo build -p sui-trace-server >&2
  ); then
    echo "error: failed to build $name server in $repo" >&2
    return 1
  fi

  local binary="$target_dir/debug/sui-trace-server"
  [[ -x "$binary" ]] || fail "built binary not found: $binary"
  printf '%s\n' "$binary"
}

start_server() {
  local name="$1"
  local binary="$2"
  local port="$3"
  local network_config="$4"
  local log_file="$5"

  echo "[start] $name server on port $port" >&2
  if [[ "$name" == "new" ]]; then
    SUI_TRACE_SERVER_PORT="$port" "$binary" "$network_config" >"$log_file" 2>&1 &
  else
    SUI_TRACE_ALLOW_LARGE_PACKAGES_FOR_GOLDEN="${SUI_TRACE_ALLOW_LARGE_PACKAGES_FOR_GOLDEN:-1}" \
    SUI_TRACE_EPOCH_EVENT_PAGE_SIZE="${SUI_TRACE_EPOCH_EVENT_PAGE_SIZE:-100}" \
    SUI_TRACE_PROTOCOL_CHECKPOINT_CONCURRENCY="${SUI_TRACE_PROTOCOL_CHECKPOINT_CONCURRENCY:-8}" \
      "$binary" "$network_config" >"$log_file" 2>&1 &
  fi
  printf '%s\n' "$!"
}

wait_for_port() {
  local name="$1"
  local port="$2"
  local pid="$3"
  local log_file="$4"
  local deadline=$((SECONDS + WAIT_SECONDS))

  while (( SECONDS < deadline )); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "error: $name server exited before port $port opened" >&2
      echo "see log file: $log_file" >&2
      exit 1
    fi
    if port_in_use "$port"; then
      echo "[ready] $name server on port $port"
      return
    fi
    sleep 1
  done

  echo "error: timed out waiting for $name server on port $port" >&2
  echo "see log file: $log_file" >&2
  exit 1
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

[[ -n "$OLD_NETWORK_CONFIG" ]] || {
  usage >&2
  fail "OLD_NETWORK_CONFIG is required; use an archive/full-history JSON-RPC endpoint for sentio-260210"
}
[[ -d "$OLD_REPO" ]] || fail "old baseline worktree not found: $OLD_REPO"
[[ -f "$DIGESTS_FILE" ]] || fail "digests file not found: $DIGESTS_FILE"
[[ "$OLD_PORT" == "9301" ]] || fail "OLD_PORT cannot be changed because sentio-260210 hard-codes port 9301"
[[ "$NEW_PORT" != "$OLD_PORT" ]] || fail "NEW_PORT must differ from OLD_PORT"

mkdir -p "$OUT_DIR" "$LOG_DIR"
OLD_LOG="$LOG_DIR/old-server.log"
NEW_LOG="$LOG_DIR/new-server.log"
COMPARE_LOG="$OUT_DIR/compare.log"

ensure_port_free "$OLD_PORT" "old"
ensure_port_free "$NEW_PORT" "new"

if ! OLD_BINARY="$(build_server old "$OLD_REPO" "$OLD_CARGO_TARGET_DIR")"; then
  exit 2
fi
if ! NEW_BINARY="$(build_server new "$REPO_ROOT" "$NEW_CARGO_TARGET_DIR")"; then
  exit 2
fi

OLD_PID="$(start_server old "$OLD_BINARY" "$OLD_PORT" "$OLD_NETWORK_CONFIG" "$OLD_LOG")"
NEW_PID="$(start_server new "$NEW_BINARY" "$NEW_PORT" "$NEW_NETWORK_CONFIG" "$NEW_LOG")"

wait_for_port old "$OLD_PORT" "$OLD_PID" "$OLD_LOG"
wait_for_port new "$NEW_PORT" "$NEW_PID" "$NEW_LOG"

echo "[compare] writing artifacts to $OUT_DIR"
python3 "$REPO_ROOT/crates/sui-trace-server/scripts/compare_call_trace.py" \
  --old-base-url "http://127.0.0.1:$OLD_PORT" \
  --new-base-url "http://127.0.0.1:$NEW_PORT" \
  --chain-id "$CHAIN_ID" \
  --digests-file "$DIGESTS_FILE" \
  --out-dir "$OUT_DIR" \
  --summary-json "$SUMMARY_JSON" \
  --timeout "$TIMEOUT" \
  --retries "$RETRIES" \
  --retry-delay "$RETRY_DELAY" \
  2>&1 | tee "$COMPARE_LOG"

echo "[done] summary: $SUMMARY_JSON"
