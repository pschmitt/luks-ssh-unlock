# Development

```sh
nix develop                                   # toolchain and linters
cargo test                                    # unit tests (both crates)
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo deny check                              # advisories, licenses, sources
shellcheck initrd-checksum.sh entrypoint.sh
nix flake check                               # package, measurers, module and initrd VM test
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
| `initrd-checksum.sh` | The checksum helper: uploads the measurer, runs it, diffs the result |
| `initrd-measure/` | The static measurer: unpacks images like the kernel, walks a live root |
| `nix/measure.nix` | Its static x86_64 and aarch64 builds |
| `nix/vm-test.nix` | A VM in its initrd, checked over SSH and tampered with |

## Testing unlocks

Never test against a live host by accident. For end-to-end tests, run a
throwaway `sshd` on loopback with a forced command that plays the initrd
(answer `test -e /etc/initrd-release`, read the passphrase from stdin) and
point the unlocker at it with a pinned host key.

`nix build .#checks.x86_64-linux.initrd-checksum-vm` boots a NixOS VM into
its systemd initrd with SSH and checks it with `initrd-checksum` against the
baseline of its image, in both formats. It then adds a file and redirects a
symlink, and boots the VM to compare the baseline its activation wrote.

Record user-visible changes in [`CHANGELOG.md`](../CHANGELOG.md).
