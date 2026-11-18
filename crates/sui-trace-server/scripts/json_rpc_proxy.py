#!/usr/bin/env python3
# Copyright (c) Sentio
# SPDX-License-Identifier: Apache-2.0

"""Tiny local JSON-RPC proxy for golden tests.

Some archive RPC providers reject clients without an explicit User-Agent. The
old sentio-260210 replay stack does not expose request-header configuration, so
this proxy keeps the old binary unchanged while adding transport headers.
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import time
import urllib.error
import urllib.request


def main() -> int:
    args = parse_args()
    upstream = args.upstream or os.environ.get("SUI_TRACE_PROXY_UPSTREAM")
    if not upstream:
        raise SystemExit("missing --upstream or SUI_TRACE_PROXY_UPSTREAM")
    cache: dict[bytes, tuple[int, str, bytes]] = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self) -> None:  # noqa: N802 - stdlib handler API.
            length = int(self.headers.get("content-length", "0"))
            body = self.rfile.read(length)
            if args.verbose:
                try:
                    method = json.loads(body).get("method")
                except Exception:  # noqa: BLE001 - best-effort logging only.
                    method = "<invalid-json>"
                print(f"proxying {method}", flush=True)
            request = urllib.request.Request(
                upstream,
                data=body,
                headers={
                    "content-type": self.headers.get(
                        "content-type", "application/json"
                    ),
                    "accept": "application/json",
                    "user-agent": args.user_agent,
                },
            )
            if body in cache:
                status, content_type, response_body = cache[body]
                self.write_response(status, content_type, response_body)
                return

            response = forward_with_retries(request, args.timeout, args.retries, args.backoff)
            if 200 <= response[0] < 300:
                cache[body] = response
            self.write_response(*response)

        def write_response(
            self, status: int, content_type: str, response_body: bytes
        ) -> None:
            try:
                self.send_response(status)
                self.send_header("content-type", content_type)
                self.send_header("content-length", str(len(response_body)))
                self.end_headers()
                self.wfile.write(response_body)
            except BrokenPipeError:
                pass

        def log_message(self, _format: str, *_args: object) -> None:
            if args.verbose:
                super().log_message(_format, *_args)

    server = http.server.ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"listening on http://{args.host}:{args.port}", flush=True)
    server.serve_forever()
    return 0


def forward_with_retries(
    request: urllib.request.Request,
    timeout: float,
    retries: int,
    backoff: float,
) -> tuple[int, str, bytes]:
    last_error: Exception | None = None
    for attempt in range(retries + 1):
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return (
                    response.status,
                    response.headers.get("content-type", "application/json"),
                    response.read(),
                )
        except urllib.error.HTTPError as error:
            response_body = error.read()
            if error.code < 500 and error.code != 429:
                return (
                    error.code,
                    error.headers.get("content-type", "application/json"),
                    response_body,
                )
            last_error = error
            content_type = error.headers.get("content-type", "application/json")
            if attempt == retries:
                return error.code, content_type, response_body
        except Exception as error:  # noqa: BLE001 - surface proxy failure to RPC client.
            last_error = error
            if attempt == retries:
                response_body = str(error).encode("utf-8", errors="replace")
                return 502, "text/plain; charset=utf-8", response_body

        if backoff > 0:
            time.sleep(backoff * (2**attempt))

    response_body = str(last_error).encode("utf-8", errors="replace")
    return 502, "text/plain; charset=utf-8", response_body


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Local JSON-RPC forwarding proxy")
    parser.add_argument(
        "--upstream",
        help="upstream JSON-RPC URL, or set SUI_TRACE_PROXY_UPSTREAM",
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=9399)
    parser.add_argument("--timeout", type=float, default=130.0)
    parser.add_argument("--retries", type=int, default=4)
    parser.add_argument("--backoff", type=float, default=1.0)
    parser.add_argument("--user-agent", default="sui-trace-golden/1")
    parser.add_argument("--verbose", action="store_true")
    return parser.parse_args()


if __name__ == "__main__":
    raise SystemExit(main())
