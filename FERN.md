# FERN for the loadout CLI

How the code in `src/` is laid out. FERN stands for **F**ractal, **E**ncapsulated, **R**ooted,
**N**ested. It comes from Dan's own projects. Crabstock first wrote it as a backend spec
(`docs/FERN.md`, commit `7bab40f`), then rewrote it for frontend routes (#59) and folded it into
`docs/FRONTEND.md`. Its Rust counterpart there is Argo's `modules/<subject>/mod.rs` layout. Loadout
keeps the four properties and swaps HTTP routes for CLI commands.

| Letter | Property | Rule here |
| --- | --- | --- |
| F | Fractal | A file becomes a folder when it grows children worth their own files: `manifest.rs` became `manifest/{mod,schema,check}.rs`. When the children go away, it collapses back. |
| E | Encapsulated | A subject's `mod.rs` is its public API. Its other files are private (`mod check;`), and outsiders call only what `mod.rs` exposes. |
| R | Rooted | Imports point down the tree. A subject never imports a command, and subjects import each other only along the graph in [docs/architecture.md](docs/architecture.md#2-code-layout). There are no cycles. |
| N | Nested | Code lives with what uses it. Shared code moves up to the lowest common owner, only once it has a second user. |

## Layout

```
src/
  main.rs            binary entry; only calls cli::main
  cli.rs             parses the command line and dispatches to a command
  commands/<cmd>.rs  one command: clap Args + run. No domain logic
  <subject>/mod.rs   one subject (manifest, resolve, plan, ...): its public API
  <subject>/*.rs     the subject's private parts
  platform.rs, id.rs, diagnostic.rs, context.rs, machine.rs
                     the floor every subject stands on; they import no subject
```

- **Commands are the routes.** A command file turns flags into calls on subjects, prints the
  result, and returns an `Exit`. When a command needs logic, that logic belongs to a subject.
- **A subject is a noun.** It owns its types, its error enum (`thiserror`), and its rules. A failure
  the user can act on is a variant of that enum. Anything else is an `anyhow` error with context,
  added at the command.
- **Shared types live with their owner.** `plan::Desired` is defined in `plan` even though `agents`
  produces it. Other subjects import the type, never the owner's functions, when importing the
  function would close a cycle.
- **No speculative layers.** No traits with one implementation, no plugin registry, no service
  container. Per-agent and per-manager differences are data tables (`match` on an enum), not dynamic
  dispatch.
- **Comments say why**, never what. Tests check behavior through the public API or the real binary.
