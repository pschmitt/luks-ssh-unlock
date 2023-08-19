#!/usr/bin/env bash

cd "$(readlink -f "$(dirname "$0")")" || exit 9

docker buildx build \
  --platform=linux/amd64,linux/386,linux/arm/v6,linux/arm/v7,linux/arm64/v8 \
  --tag pschmitt/luks-ssh-unlock \
  --push .
