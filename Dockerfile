# hadolint ignore=DL3002
ARG NIX_VERSION=2.24.9
FROM nixos/nix:${NIX_VERSION} AS busybox-builder

SHELL ["/bin/sh", "-euxo", "pipefail", "-c"]

ENV NIX_CONFIG="experimental-features = nix-command flakes" \
    PATH="/nix/var/nix/profiles/default/bin:${PATH}"

# Pre-build static busybox for amd64/arm64; copy binaries out
RUN busybox_amd64=$(nix build --print-out-paths --no-link 'nixpkgs#legacyPackages.x86_64-linux.pkgsStatic.busybox') && \
    busybox_arm64=$(nix build --print-out-paths --no-link 'nixpkgs#legacyPackages.aarch64-linux.pkgsStatic.busybox') && \
    mkdir -p /out && \
    cp "$busybox_amd64"/bin/busybox /out/busybox-amd64 && \
    cp "$busybox_arm64"/bin/busybox /out/busybox-arm64 && \
    cp "$busybox_amd64"/bin/busybox /out/busybox && \
    chmod +x /out/busybox*

FROM alpine:edge

SHELL ["/bin/sh", "-eux", "-c"]

# hadolint ignore=DL3018
RUN apk add --no-cache \
    bash \
    curl \
    bind-tools \
    jq \
    msmtp \
    openssh-client \
    coreutils \
    findutils \
    util-linux \
    cpio \
    gzip \
    zstd \
    netcat-openbsd && \
  ln -sf /usr/bin/msmtp /usr/sbin/sendmail

WORKDIR /app

COPY entrypoint.sh /entrypoint.sh
COPY luks-ssh-unlock.sh initrd-checksum.sh /usr/local/bin/
COPY --from=busybox-builder /out /busybox

RUN chmod +x /entrypoint.sh /usr/local/bin/luks-ssh-unlock.sh /usr/local/bin/initrd-checksum.sh

ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    SSH_HOSTNAME=example.com \
    SSH_KEY=/run/secrets/ssh_key \
    SSH_PORT=22 \
    FORCE_IPV4= \
    FORCE_IPV6= \
    SSH_KNOWN_HOSTS= \
    SSH_KNOWN_HOSTS_FILE= \
    SSH_INITRD_KNOWN_HOSTS= \
    SSH_INITRD_KNOWN_HOSTS_FILE= \
    SSH_JUMPHOST= \
    SSH_JUMPHOST_USERNAME= \
    SSH_JUMPHOST_PORT= \
    SSH_JUMPHOST_KEY= \
    SSH_USERNAME=root \
    LUKS_PASSPHRASE= \
    LUKS_PASSPHRASE_FILE= \
    LUKS_TYPE=raw \
    EVENTS_FILE= \
    SLEEP_INTERVAL=5 \
    HEALTHCHECK_PORT= \
    HEALTHCHECK_REMOTE_HOSTNAME= \
    HEALTHCHECK_REMOTE_USERNAME= \
    HEALTHCHECK_REMOTE_CMD= \
    INITRD_CHECKSUM_FILE= \
    INITRD_CHECKSUM_SCRIPT=/usr/local/bin/initrd-checksum.sh \
    APPRISE_URL= \
    APPRISE_TAG= \
    APPRISE_TITLE= \
    EMAIL_FROM= \
    EMAIL_RECIPIENT= \
    EMAIL_SUBJECT= \
    MSMTPRC=/config/msmtprc \
    MSMTP_ACCOUNT=

VOLUME /config
VOLUME /data/events

ENTRYPOINT ["/entrypoint.sh"]
