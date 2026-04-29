# Security Policy

## Supported versions

`buffr-modal` is pre-1.0. Only the latest 0.x patch release receives security
fixes. Once 1.0.0 ships, the latest minor receives fixes; older minors are
best-effort.

## Reporting a vulnerability

**Do not open a public GitHub issue for security reports.**

Email `mxaddict@kryptic.sh` with:

- Affected version(s)
- Description of the issue and impact
- Reproduction steps or proof-of-concept
- Disclosure timeline preference

Acknowledgment within 72 hours. Coordinated disclosure window is typically 30
days from acknowledgment, extendable for complex issues.

## Dependencies

`cargo deny` runs in CI checking RUSTSEC advisories. Vulnerable transitive
dependencies trigger an issue automatically.
