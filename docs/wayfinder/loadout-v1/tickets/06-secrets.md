# Secrets in a loadout

Type: grilling
Status: resolved
Blocked by: none

## Question

How does a loadout refer to secrets that must stay on the machine?

Covers MCP server definitions that need API keys or tokens and user files that need credentials. Settle: whether a loadout may ever hold a secret (encrypted or not); how a definition references one that lives locally (environment variable, OS keychain or Credential Manager, password manager CLI); and what a run does when a referenced secret is missing.

## Answer

Resolved 2026-10-24 under delegation. Full contract: [architecture §3, Secret references](../../../architecture.md#secret-references).

- A loadout never holds a secret value, encrypted or not.
- MCP `env` and `headers` values may be `{ env = "NAME", prefix = "..." }`. Loadout writes the reference in each agent's own env syntax, so the agent reads the variable at start. No resolved credential reaches a config, journal, backup, plan, or output.
- An agent with no documented env syntax gets an `unsupported` action for such a server, not a guessed syntax.
- Keychain and password-manager sources are deferred, because writing their values into agent configs would put secrets on disk.
- `validate` warns when a credential-looking value is written as plain text.
