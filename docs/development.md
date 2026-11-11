# Development

```sh
nix develop                                   # toolchain and linters
cargo test                                    # unit tests
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo deny check                              # advisories, licenses, sources
shellcheck initrd-checksum.sh entrypoint.sh
nix flake check                               # package (runs the tests) and module check
```

## Layout

| Path | Contents |
| --- | --- |
| `src/settings.rs` | Options, environment variables, environment files |
| `src/ctx.rs` | Per-target context and the OpenSSH command lines |
| `src/probe.rs` | Healthcheck, port probe, initrd test |
| `src/checksum.rs` | Baseline signature, initrd comparison, baseline fetch |
| `src/unlock.rs` | The unlock methods and output sanitizing |
| `src/daemon.rs` | The check loop and its states |
| `src/status.rs` | The status report |
| `src/tui.rs` | The dashboard |
| `src/dhcp.rs` | The DHCP listener |
| `initrd-checksum.sh` | The checksum helper; stays a shell script because it runs on the target |

## Testing unlocks

Never test against a live host by accident. For end-to-end tests, run a
throwaway `sshd` on loopback with a forced command that plays the initrd
(answer `test -e /etc/initrd-release`, read the passphrase from stdin) and
point the unlocker at it with a pinned host key.

Record user-visible changes in [`CHANGELOG.md`](../CHANGELOG.md).
