# Security Policy

## Supported versions

Security fixes are applied to the latest released version and the default branch. Older binaries may not receive backports.

## Reporting a vulnerability

Please do not open a public issue for a vulnerability involving command execution, credential exposure, unsafe GUI automation, duplicate task submission, local data disclosure, notification/deep-link spoofing, or release integrity.

Use GitHub's private vulnerability reporting for this repository:

https://github.com/wintopic/codex-longwatch/security/advisories/new

Include:

- affected version and operating system;
- attack prerequisites and impact;
- a minimal reproduction or proof of concept;
- relevant logs with prompts, credentials, tokens, environment variables, usernames, and personal paths removed;
- any suggested mitigation.

You should receive an acknowledgement within 7 days. Please allow time for validation and coordinated remediation before public disclosure.

## Security boundaries

- Longwatch inherits the local Codex authentication context; it does not manage or store API keys.
- The primary transport is a local stdio child process.
- GUI automation is disabled by default and requires explicit opt-in.
- Configuration contains the user's task text and should be treated as private.
- Support reports are intentionally redacted, but users should still review them before sharing.
- Automated release artifacts are currently unsigned/not notarized; verify the published SHA-256 checksums.

This policy does not cover vulnerabilities in OpenAI services, Codex CLI itself, Rust, GPUI, operating systems, or other third-party dependencies. Report those issues to the appropriate upstream project.
