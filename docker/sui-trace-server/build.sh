#!/bin/sh
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0

set -e

DIR="$( cd "$( dirname "$0" )" && pwd )"
REPO_ROOT="$(git rev-parse --show-toplevel)"
DOCKERFILE="$DIR/Dockerfile"
GIT_REVISION="$(git describe --always --abbrev=12 --dirty --exclude '*')"
BUILD_DATE="$(date -u +'%Y-%m-%d')"
PROFILE="${PROFILE:-release}"

echo
echo "Building sui-trace-server docker image"
echo "Dockerfile: \t$DOCKERFILE"
echo "docker context: $REPO_ROOT"
echo "build date: \t$BUILD_DATE"
echo "git revision: \t$GIT_REVISION"
echo "profile: \t$PROFILE"
echo

docker build -f "$DOCKERFILE" "$REPO_ROOT" \
  --platform linux/amd64 \
  --tag ghcr.io/sentioxyz/sui-tracer:latest \
  --build-arg PROFILE="$PROFILE" \
  --build-arg GIT_REVISION="$GIT_REVISION" \
  --build-arg BUILD_DATE="$BUILD_DATE" \
  "$@"
