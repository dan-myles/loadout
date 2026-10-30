# Dispatch store

- Format: 1
- Store ID: t-0096df81139729cf701a0b358edcc923e50610f61b4c5adf15f23cafcf5853cb
- Initial thread ID: 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f
- Repository: https://github.com/dan-myles/loadout
- Created: 2026-10-29T21:23:23.983596+00:00
- Current owner thread ID: 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f
- Current owner link: <t3-thread://v2/ad3e89c1-ca86-4c3a-aaba-64e20f7e412c/32c7d5bd-3cfa-4aef-ac95-07b34326ae2f>
- Current owner user: unknown
- Ownership generation: 1
- Ownership source: user message 02d4a63a-242e-496d-8df6-ef61de7edaf9 in thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f at 2026-10-29T21:22:37.859Z; exact request `$dispatch setup`

## Working rules

- In-flight cap: 4
- Stalled after hours: 24
- Auto-dispatch contained bugs: enabled, at most 3 per pass
- Never paths: none
- Evidence sources: delivered items and their linked context only; email/calendar disabled
- Tracker: unconfigured
- Validator selection: unconfigured
- Standing orders: none

## Setup observations

- Repository identity and checkout verified through T3 project metadata and Git origin.
- Project ID observation: a164c39a-b7c1-4a98-a1ec-941e1f22d48c
- Coordinator pinned through t3_thread_organize; successful result sequence 1318.
- Thread/project read, send and launch tools available.
- Cooper selection verified in live capabilities: claudeAgent family, claude-opus-5-5, effort medium; available instance claude-pool. Reverify routing and availability before launch.
- Automatic contained-bug dispatch requires a configured tracker and validator.
- Workers I001, I002, I005 and I008 launched under explicit user approval; see board and saved briefs. No recurring triggers configured.

## Save, share and takeover

- Initialized on main at 393afd3, clean and equal to origin/main after fetch.
- Established state destination: origin/main in https://github.com/dan-myles/loadout.
- Always commit meaningful updates to this owned store with [skip ci] and push directly to main, without a state PR. Batch related updates as one coherent snapshot. No per-save approval is required under the installed Dispatch project-state contract verified on 2026-10-30.
- Fetch and reconcile ownership before writing; review privacy and staged paths; check every outgoing commit. Leave unrelated changes and outgoing commits untouched. Never force-push, change repository protection settings or request extra bypass privileges. A normal server-accepted push counts as published even if it reports an existing rule bypass.
- This authorization covers coordination documents only. It grants no new product work, merges or deployments. [skip ci] suppresses supported CI triggers, not every integration.
- See D011/D012 and the journal for policy provenance. Uncommitted or unpushed state cannot be pulled elsewhere.
- A recipient must pull the shared state, stop the previous writer, and explicitly request `dispatch takeover t-0096df81139729cf701a0b358edcc923e50610f61b4c5adf15f23cafcf5853cb`. Pulling files alone does not transfer ownership. Preserve the stable ID and increment the ownership generation during takeover.
