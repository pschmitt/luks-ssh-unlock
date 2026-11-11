# Container

Images for linux/amd64 and linux/arm64, rebuilt daily:

- `ghcr.io/pschmitt/luks-ssh-unlock`
- `docker.io/pschmitt/luks-ssh-unlock`

Tags: `latest` and `main` follow the main branch, `sha-<commit>` pins one
build. The GHCR image carries a build provenance attestation:

```sh
gh attestation verify oci://ghcr.io/pschmitt/luks-ssh-unlock:latest -R pschmitt/luks-ssh-unlock
```

## Run

```sh
docker run --rm \
  -v /path/to/secrets:/run/secrets:ro \
  -e SSH_HOSTNAME=target.example.com \
  -e SSH_KEY=/run/secrets/id_ed25519 \
  -e SSH_INITRD_KNOWN_HOSTS_FILE=/run/secrets/known_hosts_initrd \
  -e LUKS_PASSPHRASE_FILE=/run/secrets/luks_target \
  ghcr.io/pschmitt/luks-ssh-unlock:latest
```

Arguments after the image name are passed to `luks-ssh-unlock`, for example
`status` or `run --once`.

| Path | Purpose |
| --- | --- |
| `/config/msmtprc` | msmtp configuration for mail notifications, installed to `/etc/msmtprc` on start |
| `/data/initrd-checksum` | `INITRD_CHECKSUM_DIR`: baselines fetched from booted hosts |
| `/data/events` | A place for `EVENTS_FILE` |
| `/busybox` | The BusyBox bundle for paranoid checks |

[`docker-compose.yaml`](../docker-compose.yaml) shows several targets with
health checks, notifications, a jumphost and initrd validation.
