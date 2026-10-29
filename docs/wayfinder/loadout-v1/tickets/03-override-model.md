# Override model

Type: grilling
Status: resolved
Blocked by: none

## Question

How does a loadout make one machine differ from the global baseline?

Settle: which layers exist between global and a single machine (OS, groups or tags such as "work" or "laptop", the machine itself); how a machine is identified and how it joins a group; what an override can do to an item (add, remove, replace, change one field); how conflicts between layers resolve; and whether user files support per-machine values inside the file (templating), and in what form.

## Answer

Resolved 2026-10-23 with Dan, through three grilling rounds he answered live.

- **Layers**, broadest first: global → OS → tag → machine. A narrower layer wins.
- **OS layer**: exactly three, `macos`, `linux`, and `windows`. Distro and CPU architecture are machine facts, not layers.
- **Machine identity**: a machine name chosen on the first run, defaulting to the hostname and kept on the machine. Renaming the computer doesn't change it.
- **Tags**: a machine chooses its tags itself as **profiles**, saved on the machine, so a new machine needs no edit to the loadout and gets no profile it wasn't given. The loadout may still list a machine's tags under `[machines]` for a machine that needs its own layer; those come first, then its chosen profiles. When two of a machine's tags set the same thing differently, the later one wins. (Changed 2026-10-29 by the owner, who wants machines to get profiles locally instead of being declared; the earlier answer had the loadout list every machine's tags.)
- **Override operations**: a narrower layer can add an item, remove an inherited item, replace one whole, or change any field of it.
- **Lists**: assigning a list replaces it. Explicit add-to and remove-from operations edit an inherited list instead.
- **Conditions**: any item in any layer may carry a condition on machine facts (OS, distro, arch), for example `when os = windows`. This covers combinations such as "work tag, Windows only" without extra tags.
- **Templates**: only user files marked as templates are rendered, for example with a `.tmpl` suffix. Every other file is copied byte for byte. Templates support variables and if/else. Values come from variables set in any layer, where the narrower layer wins, plus built-in machine facts (machine name, OS, distro, arch, home path). Environment variables are not a source.
- **Missing variable**: if a template uses a variable that no layer sets, the run stops before writing anything and names the file and the variable.
