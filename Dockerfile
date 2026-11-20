# syntax=docker/dockerfile:1

# The static initrd measurers (x86_64 + aarch64) that initrd checks upload to
# the target.
ARG NIX_VERSION=2.36.0
FROM docker.io/nixos/nix:${NIX_VERSION} AS measure
ENV NIX_CONFIG="experimental-features = nix-command flakes"
WORKDIR /src
COPY flake.nix flake.lock Cargo.toml Cargo.lock ./
COPY nix ./nix
COPY initrd-measure ./initrd-measure
RUN nix --option filter-syscalls false build '.#initrd-measure' --out-link /tmp/measure \
    && mkdir -p /out \
    && cp -aL /tmp/measure/. /out

# cargo-chef splits dependency compilation into its own layer, keyed only on
# the dependency graph, so the layer cache skips rebuilding dependencies when
# only our own sources change. Alpine's Rust targets musl: a static binary.
FROM docker.io/library/rust:1.100-alpine AS chef
# hadolint ignore=DL3018
RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --locked --version 0.1.78
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY initrd-measure ./initrd-measure
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY initrd-measure ./initrd-measure
RUN cargo build --release --locked \
    && install -Dm755 target/release/luks-ssh-unlock /out/luks-ssh-unlock

FROM docker.io/library/alpine:3.24
# OpenSSH for every connection, msmtp as sendmail, and what initrd-checksum
# needs (bash, coreutils, diffutils).
# hadolint ignore=DL3018
RUN apk add --no-cache \
      bash \
      coreutils \
      diffutils \
      msmtp \
      openssh-client \
    && ln -sf /usr/bin/msmtp /usr/sbin/sendmail \
    && mkdir -p /data/events /data/initrd-checksum

COPY --from=build /out/luks-ssh-unlock /usr/local/bin/luks-ssh-unlock
COPY --chmod=0755 initrd-checksum.sh /usr/local/bin/initrd-checksum
COPY --chmod=0755 entrypoint.sh /entrypoint.sh
COPY --from=measure /out /usr/local/libexec/initrd-measure

ENV INITRD_CHECKSUM_DIR=/data/initrd-checksum \
    INITRD_CHECKSUM_MEASURE_DIR=/usr/local/libexec/initrd-measure \
    MSMTPRC=/config/msmtprc

VOLUME ["/config", "/data"]
ENTRYPOINT ["/entrypoint.sh"]
# Watch and unlock, configured through the environment; arguments replace it.
CMD ["run"]
