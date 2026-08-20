# Contributing to Longwatch

Participation in this project is governed by the
[Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).

Thanks for helping make Longwatch safer and more dependable. Reliability changes are judged primarily by whether they preserve the core invariant: one persistent task may own at most one active Codex turn.

## Before opening an issue

- Search existing issues and release notes.
- Remove task text, credentials, environment variables, personal paths, and other sensitive data from logs.
- Include your OS, Longwatch version, `codex --version`, transport shown in diagnostics, expected behavior, and the smallest reproducible timeline.
- For vulnerabilities, follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## Development setup

1. Install Rust 1.85 or newer.
2. Install the native GPUI dependencies for your platform.
3. Fork and clone the repository.
4. Build with all features:

```bash
cargo build --locked --all-features
```

Linux contributors can copy the package list from `.github/workflows/ci.yml`.

## Making changes

- Keep transport behavior deterministic and serialized.
- Never add a code path that silently activates GUI automation.
- Persist safety-critical state before crossing a process or network boundary.
- Treat Codex app-server state as authoritative during recovery.
- Keep protocol decoders tolerant of optional fields and strict about invariants.
- Do not log prompts, credentials, environment dumps, or full configuration documents.
- Add focused tests for every recovery, retry-classification, persistence, or state-transition change.
- Preserve existing user changes when working in a non-clean checkout.

## Validation

Run before submitting:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features --all
cargo build --locked --release --all-features
```

Cross-platform changes should be exercised on the affected OS when possible. CI checks Windows, Linux, macOS Intel, and macOS Apple Silicon.

## Pull requests

Keep pull requests focused. Explain:

- what changed;
- why the change is needed;
- how duplicate submissions or concurrent turns remain impossible;
- privacy or platform-permission impact;
- tests and manual checks performed.

Screenshots are encouraged for visible UI changes. Do not include real task content or private paths.

By contributing, you agree that your contribution is licensed under the repository's Apache License 2.0.
