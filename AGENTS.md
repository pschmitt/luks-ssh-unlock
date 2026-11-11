# Agent instructions

This repository automates remote disk unlock over SSH and Kubernetes. Treat keys, unlock material, endpoints, and cluster details as sensitive; use role names and placeholders in shared examples.

## Working and validation

- Read the README and the affected modules (`src/`, `nix/module.nix`, `initrd-checksum.sh`) together before changing unlock or orchestration behavior.
- Validate with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo deny check`, `shellcheck` and `nix flake check` (on a build host, not locally), and test unlock flows against isolated fixtures such as a throwaway loopback sshd; do not contact or unlock a live system as a test.
- Record user-visible behavior changes in `CHANGELOG.md`.
- Never print secret-bearing configuration or commit credentials.
- Before changing GitHub Actions, check whether [pschmitt/ci-workflows](https://github.com/pschmitt/ci-workflows) provides a reusable workflow or action. Preserve current behavior and check coverage, and use `@main` for shared workflow or action references.
