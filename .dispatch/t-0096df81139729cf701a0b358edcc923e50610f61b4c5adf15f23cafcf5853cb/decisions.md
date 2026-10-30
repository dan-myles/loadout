# Human decisions

## D001 | 2026-10-29T21:22:37.859Z

- Exact words: `$dispatch setup`
- Source: user message 02d4a63a-242e-496d-8df6-ef61de7edaf9 in thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f at 2026-10-29T21:22:37.859Z
- Scope: initialize this repository's Dispatch coordination store for the verified current thread, generation 1.
- Supersedes: none
- Implementation, publication and recurring chase authorization: none supplied by this request.

## D002 | 2026-10-29T21:24:16.875Z

- Source: user message e66dddaf-d369-4cfc-843b-1d8064e8b489 in current owner thread.
- Exact words:

```text
kk have $cooper take care of:

* Dont install brew as root for load out cli :o 
* Both load out init and load out apply dont show enough spinners for when something is working
```

- Scope: I001 Homebrew root safety; I002 init/apply progress.
- Supersedes: none.

## D003 | 2026-10-29T21:24:21.118Z

- Source: user message 67912cb6-fef6-4441-aff7-f54d8844ac04 in current owner thread.
- Exact words: `parallel`
- Scope: run I001 and I002 concurrently in separate worktrees.
- Supersedes: none.

## D004 | 2026-10-29T21:36:52.884Z

- Exact words: `okayt perfect that merge that over please`
- Source: owner thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f, user message 0e3bc2e6-8f66-4ee2-81e8-8e5a19af6837.
- Scope: merge PR #19, approved head bb1839e5035c3816973daa62e4e29121b29738e3 into main.
- Supersedes: I001 brief no-merge restriction for PR #19 only.

## D005 | 2026-10-29T21:46:55.076Z

- Exact words: `yeah merge that shit as well please`
- Source: user message 618e141d-4810-43de-9b22-6ef7ddd19b1f, owner thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f.
- Scope: merge PR #20 at approved head 986a408359a832dd82e7118482c2a9d2c3f93db6 into main, after reported caveats.
- Supersedes: no-merge restriction for I002 PR #20 only. No new follow-up implementation authorized.

## D006 | 2026-10-29T22:26:12.905Z

- Exact words: yeah we want to NOT really use machine name mapping we just want to have profiles and my only profile should be development, but that doesnt matter for now, go ahead and start $cooper working on profiles, and in general get rid of machine specific config as a recommendation, we can keep it for now with delcared machines but we are not going to be normally declaring machines, just profiles that machines get and the CLi will be interactive!
- Source: user message 5132928f-ded5-4930-8d38-1be9412e8da9, owner thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f.
- Scope: I005 interactive locally selected profiles, no required machine registration; retain declared-machine compatibility. Personal development profile/migration deferred.
- Supersedes: previous recommendations to assign coding to all machines and make machine-name registration the normal setup. Prior personal profile naming preference coding becomes development; no personal repo changes authorized here.


## D007 | 2026-10-29T22:41:18.548Z

- Exact words: `make sure to verify that this works end to end please`
- Source: user message 99777a71-c184-4b73-9c07-4e001cffaed6, worker mcp:launch:22537f126e407f1098ac2bf9b114606041c25493940469b55a5a680721af2395, position 170.
- Scope: I005 end-to-end verification; supersedes: none; grants no merge approval.

## D008 | 2026-10-29T22:53:23.158Z

- Exact words: `merge it please`
- Source: user message e875a260-e27a-45b8-ba30-804dc3afc311 in owner thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f.
- Scope: merge PR #21 at reviewed head e07db17295ac6e87080fac70e97945d1121611cc after report and compatibility caveat.
- Supersedes: original no-merge restriction for I005 PR #21 only. No migration of personal loadout or follow-up implementation authorized.

## D009 | 2026-10-29T23:06:17.060Z

- Exact words: `yueah ge ta new release going`
- Source: user message 8ca530bf-9074-4da8-8964-a1b7c1c13400, owner thread 32c7d5bd-3cfa-4aef-ac95-07b34326ae2f.
- Scope: next stable release of merged profiles, progress and Homebrew changes; normal necessary release preparation, merge/tag/publication and workflow cleanup. No personal loadout migration or machine upgrades.
- Supersedes: no-publication restriction for this scoped release only.

## D010 | 2026-10-29T23:18:14.332Z

- Exact words: `make sure to compoletely release it, seems like its still a draft`
- Source: user message 3d722db8-b14e-48a1-818d-4948b07e6cb8 in owner thread.
- Scope: finish I008 stable publication; reiterates D009.

## D011 | 2026-10-30T17:02:27.225Z

- Source: relayed policy notice message:mcp:988894ba-85e2-4325-a959-13e43a2dc5f9:thread-send:loadout-09e2f39-state-push-notify-loadout, owner thread position 273, sender thread 6dfe19f6-f80a-4ebf-bf07-00d13d57d822. The sender attributes the policy to Dan; this is not a direct human message.
- Exact policy excerpt: "Dispatch now automatically commits and pushes its owned .dispatch/<id>/ state docs, no per-save approval. Batch meaningful updates, use [skip ci], and push to the established state branch. Leave unrelated changes and outgoing commits untouched; no force-push or bypassing branch protection."
- Verification: reread installed dispatch/references/project-state.md, Save and share through Git, plus dispatch-setup and dispatch-digest. All explicitly authorize automatic state commits and pushes. Reported upstream skills revision: 09e2f39.
- Scope: existing owned store only, established origin/main. No new product work, merges or deployments. [skip ci] is not universal CI suppression.
- Supersedes: the current save/share approval wording in store.md; historical setup observations remain intact.
