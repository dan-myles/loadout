# Dispatch board

State policy updated at 2026-10-30T18:16:10.630082+00:00. Product evidence last reconciled at 2026-10-29T23:21:02.153676+00:00. Store t-0096df81139729cf701a0b358edcc923e50610f61b4c5adf15f23cafcf5853cb, ownership generation 1.

## Now

No workers executing. Approved implementation and release work is complete.

## Backlog

### I003: Homebrew cask password-prompt behavior

- State: proposed; execution: waiting on user decision; kind: investigation; proposed rank: 3.
- Repository: dan-myles/loadout; source: I002-a1-r1 position 94; dependency: I002.
- Acceptance: verify whether a Homebrew cask invoking sudo keeps its prompt readable while the spinner redraws; fix only if authorized.
- Worker/attempt/PR/head: none; gate: proposal only; report cursor: none.
- Last update: 2026-10-29T21:44:43.128730+00:00.

### I004: Clear interrupted progress line

- State: proposed; execution: waiting on user decision; kind: feature; proposed rank: 4.
- Repository: dan-myles/loadout; source: I002-a1-r1 position 94; dependency: I002.
- Acceptance: optionally clear the spinner line on Ctrl-C while preserving exit 130 and subprocess signal behavior. Current implementation stops drawing and leaves the interrupted step visible.
- Worker/attempt/PR/head: none; gate: optional follow-up not approved; report cursor: none.
- Last update: 2026-10-29T21:44:43.128730+00:00.

### I006: Doctor validation of removed profiles

- State: proposed; execution: waiting on user decision; kind: improvement; proposed rank: unset.
- Repository: dan-myles/loadout; dependency: I005; source: reports/I005-a1-r1.md, worker report position 270.
- Acceptance: Have doctor identify a locally selected profile removed from the checkout, consistent with plan/apply/sync diagnostics.
- Worker/attempt/PR/head: none; gate: no follow-up implementation authorized; report cursor: none.
- Last update: 2026-10-29T22:53:00.924386+00:00.

### I007: Automated profile terminal and background-sync coverage

- State: proposed; execution: waiting on user decision; kind: improvement; proposed rank: unset.
- Repository: dan-myles/loadout; dependency: I005; source: reports/I005-a1-r1.md, worker report position 270.
- Acceptance: Convert the verified interactive onboarding and background-sync flows into durable automated regression checks.
- Worker/attempt/PR/head: none; gate: no follow-up implementation authorized; report cursor: none.
- Last update: 2026-10-29T22:53:00.924386+00:00.

## What landed

### I008: Publish stable release with profiles and CLI improvements

- State: done; execution: finished; kind: release; proposed rank: 1; attempt: 1.
- Repository: https://github.com/dan-myles/loadout; dependencies: I001, I002, I005 all merged.
- Source: user message 8ca530bf-9074-4da8-8964-a1b7c1c13400; decision D009.
- Worker title: cooper: release profiles and CLI improvements
- Exact worker ID: mcp:launch:e5b9199e001ddb725fbd74118772ba64ec125279a7e86f011cb619178b1900a5
- Worker: <t3-thread://v2/ad3e89c1-ca86-4c3a-aaba-64e20f7e412c/mcp:launch:e5b9199e001ddb725fbd74118772ba64ec125279a7e86f011cb619178b1900a5>
- Model: claude-opus-5-5, medium effort, claude-pool.
- Branch: cooper/i008-release-profiles; PR: https://github.com/dan-myles/loadout/pull/22 merged; tag v0.1.6 at b3002cf1e3c4fd15c72cab2aa8ad613405ca65c5. Published stable/latest at 2026-10-29T23:19:51Z: https://github.com/dan-myles/loadout/releases/tag/v0.1.6. Workflow run 40971492945 succeeded; Release workflow disabled.
- Acceptance: publish next stable release including all three changes, verify six-platform artifacts/checksums/smoke and latest release, restore disabled Release workflow. [Exact brief](briefs/I008-a1.md).
- Gates: none. All six archives verified against checksums and public latest downloads per worker. Dispatch verified published/latest status and 13 assets.
- Report cursor: position 126, message 0a42822b-39e8-4925-a5ba-99a03bf45794. [Complete report](reports/I008-a1-r1.md).
- No machine upgrade or personal-loadout migration performed. Worker completion observed; settlement not changed by Dispatch.
- Last update: 2026-10-29T23:21:02.153676+00:00.

### I005: Interactive profiles without machine registration

- State: done; execution: finished; kind: Feature; proposed rank: 1; attempt: 1.
- Repository: https://github.com/dan-myles/loadout; dependencies: none; base current origin/main includes I001/I002.
- Source: user message 5132928f-ded5-4930-8d38-1be9412e8da9, decision D006.
- Worker title: cooper: interactive profiles without machine registration
- Exact worker ID: mcp:launch:22537f126e407f1098ac2bf9b114606041c25493940469b55a5a680721af2395
- Worker: <t3-thread://v2/ad3e89c1-ca86-4c3a-aaba-64e20f7e412c/mcp:launch:22537f126e407f1098ac2bf9b114606041c25493940469b55a5a680721af2395>
- Model: claude-opus-5-5, medium effort, claude-pool.
- Branch: cooper/i005-interactive-profiles; PR: https://github.com/dan-myles/loadout/pull/21; head: e07db17295ac6e87080fac70e97945d1121611cc; MERGED at 2026-10-29T22:53:56Z; merge commit fd1365479ea11111f7ada7731b149fc1262d09ff; no GitHub checks/reviews.
- Acceptance: fresh undeclared machines select profiles interactively and retain selection locally for plan/apply/sync; base-only setup; deterministic noninteractive operation; legacy declared machines preserved; profiles recommended in CLI/docs/examples. See [exact brief](briefs/I005-a1.md).
- Gate: none; merge authorized by D008. Existing tag files are profiles; declared tags precede local selections; machine layer remains last. Older binaries reject state after profiles are saved.
- Verification: worker reports full check script and final-build PTY init, plan/apply/background sync; proof transcript in PR. No automated PTY/background-sync cases.
- Report cursor: position 279, message eb7afa94-1402-49d0-83db-59d59df5fd95. [Merge report](reports/I005-a1-r2.md); [validation report](reports/I005-a1-r1.md).
- Worker already settled by T3 at 2026-10-29T22:54:17.681Z. Worktree clean/pushed, children finished and nothing pending per worker; branch/worktree retained.
- Last update: 2026-10-29T22:54:37.319898+00:00.

### I002: cooper: show progress during init and apply

- State: done; execution: finished; attempt: 1; proposed rank: 2.
- Kind: Feature; repository: https://github.com/dan-myles/loadout; dependencies: none.
- Source: user message e66dddaf-d369-4cfc-843b-1d8064e8b489; parallel authorization 67912cb6-fef6-4441-aff7-f54d8844ac04. See decisions D002/D003.
- Exact worker ID: mcp:launch:8097d5be997213ba5d5b3d7146fd288405b0cac7ee3891b57d32d3ebe7af475e
- Worker: <t3-thread://v2/ad3e89c1-ca86-4c3a-aaba-64e20f7e412c/mcp:launch:8097d5be997213ba5d5b3d7146fd288405b0cac7ee3891b57d32d3ebe7af475e>
- Model: claude-opus-5-5, medium effort, claude-pool.
- PR: https://github.com/dan-myles/loadout/pull/20; head: 986a408359a832dd82e7118482c2a9d2c3f93db6; MERGED at 2026-10-29T21:47:32Z; merge commit 284f5c3156c8cd78c21e8e0f803909aa829b4968; no GitHub checks or reviews. Local checks passed per worker.
- Gate: none; merge authorized by D005. Report cursor: position 105, message 652277ac-8634-487b-8bcb-1fe6feffb128; [merge report](reports/I002-a1-r2.md); [validation report](reports/I002-a1-r1.md).
- Worker already settled by T3 at 2026-10-29T21:47:52.888Z. Branch/worktree retained, clean and pushed per worker; no pending work. Checks were not rerun on merge commit.
- Validation: Linux PTY and non-TTY checks; macOS/Windows untested. Worker skipped blast-radius helper in favor of direct output comparison; see report.
- Acceptance: Identify and cover meaningful silent waits in both init and apply using the existing UI conventions. Show what is running while it runs, stop/clear indicators on success, failure and cancellation, and keep prompts and subprocess output readable. Preserve non-TTY/CI output and existing quiet/structured output contracts where present. Avoid a broad UI redesign.
- Scope, branch, exact brief and launch provenance: [brief](briefs/I002-a1.md).
- Last update: 2026-10-29T21:48:10.580382+00:00.

### I001: cooper: prevent Homebrew installation as root

- State: done; execution: finished; attempt: 1; proposed rank: 1.
- Kind: Bug; repository: https://github.com/dan-myles/loadout; dependencies: none.
- Source: user message e66dddaf-d369-4cfc-843b-1d8064e8b489; parallel authorization 67912cb6-fef6-4441-aff7-f54d8844ac04. See decisions D002/D003.
- Exact worker ID: mcp:launch:a1b9fb6db5b9523a2c786ff1b14391866c7220bb71a00f3dea69eacff402111c
- Worker: <t3-thread://v2/ad3e89c1-ca86-4c3a-aaba-64e20f7e412c/mcp:launch:a1b9fb6db5b9523a2c786ff1b14391866c7220bb71a00f3dea69eacff402111c>
- Model: claude-opus-5-5, medium effort, claude-pool.
- PR: https://github.com/dan-myles/loadout/pull/19; head: bb1839e5035c3816973daa62e4e29121b29738e3; MERGED at 2026-10-29T21:37:40Z; merge commit 642f338d45c1a991a21247200863e9b2a95e6192. No GitHub checks or reviews. All four local gates passed per worker.
- Gate: none; merge authorized by D004; report cursor: position 87, message f9968785-4b73-4cfb-8f9a-2713366c8997. [Merge report](reports/I001-a1-r2.md); [validation report](reports/I001-a1-r1.md).
- Worker already settled by T3 at 2026-10-29T21:43:25.003Z. Worktree clean and pushed per worker; branch/worktree retained.
- Validation limits: macOS hardware untested; root/sudo checks run in isolated Linux namespaces. Temporary reproduction harness is supplemental; regression tests are in PR.
- Acceptance: Reproduce or demonstrate the current problematic path. Ensure Homebrew installation never runs with effective UID 0. Preserve ordinary non-root installation and existing-brew behavior. Cover direct-root and sudo invocation behavior as applicable; fail early with actionable guidance when no safe supported non-root execution is possible. Do not invent a user or weaken privilege checks.
- Scope, branch, exact brief and launch provenance: [brief](briefs/I001-a1.md).
- Last update: 2026-10-29T21:35:48.682964+00:00.

## Watch and links

- I001 and I002 merged. I003/I004 are optional proposals only; no follow-up implementation authorized.
- [Store](store.md), [journal](journal.md), [decisions](decisions.md).
- Tracker and validator unconfigured; these items have explicit user approval.
- State destination: origin/main; meaningful owned-store updates are always committed with [skip ci] and pushed directly to main without a state PR. See D012. Product status above retains its recorded verification timestamps.
