# Security policy

## Supported versions

Only the `main` branch is supported. There are no released versions yet.

## Reporting a vulnerability

Report vulnerabilities privately to **jaber@rightnowai.co**. Include a
description, affected files or commands, and steps to reproduce.

Please do not open a public issue for a security report.

There is no bug bounty. We will acknowledge your report and keep you updated
on the fix.

## Scope notes

Auto runs untrusted, recorded agent behavior through synthesis and
verification. Two properties are load-bearing and in scope:

- Synthesis and verification sandboxes never touch the network (wasmtime, no
  imports granted). A network escape from a sandbox is a vulnerability.
- The only paid-API path is `crates/auto-frontier`, gated behind a hard spend
  cap and an append-only ledger. A path that spends against a provider without
  passing the cap and the ledger is a vulnerability.
