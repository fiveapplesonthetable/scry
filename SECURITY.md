# Security policy

## Supported versions

Only the latest release is supported with security fixes. scry is
single-binary and trivial to upgrade — there's no point patching
older releases when `git pull && cargo build --release` lands you
on the fix.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

## Reporting a vulnerability

**Do not open a public GitHub issue for security reports.**

Email security reports to the project maintainer with subject
`[scry security] <short description>`. If you don't have a contact
address, open a GitHub *security advisory* draft via the
"Security" tab on the repo — that's private to maintainers until
published.

In your report, include:

- A description of the vulnerability and its impact.
- Steps to reproduce (a minimal index + invocation is best).
- The scry version (`scry --version`) and platform.
- Any patches or mitigations you've already identified.

Expect an acknowledgement within 72 hours. Coordinated disclosure
timeline is typically 30 days from acknowledgement to public
patch, faster for critical issues.

## Threat model

scry is a **read-only** tool against trusted source trees. It is
not a sandbox, not a network service in its default mode, and not
intended to process attacker-controlled input.

Specifically:

- **Source tree trust.** `scry index` parses every file under the
  given roots. A maliciously crafted tree-sitter input can stress
  the parser (memory + CPU); the 60 s per-file parse budget and
  the cgroup envelope bound the damage but do not prevent it. Do
  not index untrusted source.
- **Network surface.** `scry serve --listen tcp:HOST:PORT` and
  `scry serve --listen unix:PATH` are intended for local-host
  agent loops, not internet exposure. There's no auth, no rate
  limiting, no TLS. Bind to `127.0.0.1` or restrict via firewall
  / socket permissions if you must expose it.
- **MCP transport.** `scry mcp` reads JSON-RPC on stdin from a
  parent process (the agent runtime). The trust boundary is the
  agent runtime's: a malicious MCP client can issue any query, but
  scry only reads from disk — it cannot mutate the indexed source
  tree.
- **Query log.** `~/.scry/queries.log` records every query (cmd,
  args, latency, hit count). Treat as you would shell history —
  it may contain sensitive identifiers if you've grepped for
  them. The directory is created with default permissions
  (~/.scry/ is 0755, queries.log is 0644).

If you find a way to make scry read or write outside of these
boundaries, that's a security issue and we want to hear about it.
