# Secrets in a loadout

Type: grilling
Status: open
Blocked by: none

## Question

How does a loadout refer to secrets that must stay on the machine?

Covers MCP server definitions that need API keys or tokens and user files that need credentials. Settle: whether a loadout may ever hold a secret (encrypted or not); how a definition references one that lives locally (environment variable, OS keychain or Credential Manager, password manager CLI); and what a run does when a referenced secret is missing.
