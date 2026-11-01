# Agent instructions

This repository automates remote disk unlock over SSH and Kubernetes. Treat keys, unlock material, endpoints, and cluster details as sensitive; use role names and placeholders in shared examples.

## Working and validation

- Read the README and affected scripts together before changing unlock or orchestration behavior.
- Validate shell and manifest changes with static checks and isolated fixtures; do not contact or unlock a live system as a test.
- Never print secret-bearing configuration or commit credentials.
- Before changing GitHub Actions, check whether [pschmitt/ci-workflows](https://github.com/pschmitt/ci-workflows) provides a reusable workflow or action. Preserve current behavior and check coverage, and use `@main` for shared workflow or action references.
