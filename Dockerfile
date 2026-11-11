# syntax=docker/dockerfile:1

# Static BusyBox bundle (amd64 + arm64) that paranoid initrd checks upload to
# the target.
ARG NIX_VERSION=2.35.5
FROM docker.io/nixos/nix:${NIX_VERSION} AS busybox
ENV NIX_CONFIG="experimental-features = nix-command flakes"
WORKDIR /src
COPY flake.nix flake.lock ./
COPY nix ./nix
RUN nix --option filter-syscalls false build '.#busybox-static' --out-link /tmp/busybox \
    && mkdir -p /out \
    && cp -aL /tmp/busybox/. /out

# cargo-chef splits dependency compilation into its own layer, keyed only on
# the dependency graph, so the layer cache skips rebuilding dependencies when
# only our own sources change. Alpine's Rust targets musl: a static binary.
FROM docker.io/library/rust:1.99-alpine AS chef
# hadolint ignore=DL3018
RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --locked --version 0.1.78
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked \
    && install -Dm755 target/release/luks-ssh-unlock /out/luks-ssh-unlock

FROM docker.io/library/alpine:3.23
# OpenSSH for every connection, msmtp as sendmail, and what initrd-checksum
# needs (bash, coreutils, cpio, gzip, zstd).
# hadolint ignore=DL3018
RUN apk add --no-cache \
      bash \
      coreutils \
      cpio \
      diffutils \
      findutils \
      gzip \
      msmtp \
      openssh-client \
      util-linux \
      zstd \
    && ln -sf /usr/bin/msmtp /usr/sbin/sendmail \
    && mkdir -p /data/events /data/initrd-checksum

COPY --from=build /out/luks-ssh-unlock /usr/local/bin/luks-ssh-unlock
COPY --chmod=0755 initrd-checksum.sh /usr/local/bin/initrd-checksum
COPY --chmod=0755 entrypoint.sh /entrypoint.sh
COPY --from=busybox /out /busybox

ENV INITRD_CHECKSUM_DIR=/data/initrd-checksum \
    INITRD_CHECKSUM_BUSYBOX_DIR=/busybox \
    MSMTPRC=/config/msmtprc

VOLUME ["/config", "/data"]
ENTRYPOINT ["/entrypoint.sh"]
