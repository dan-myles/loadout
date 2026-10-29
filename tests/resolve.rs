use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::diagnostic::{Diagnostics, Severity};
use loadout::machine::Facts;
use loadout::manifest::{BuiltinAgent, Loadout, Manager, Value, Variable};
use loadout::platform::{Arch, Os};
use loadout::resolve::{self, Agent, Resolved, Transport};

struct Tree {
    dir: tempfile::TempDir,
    loadout: Loadout,
}

const SOURCES: [&str; 5] = [
    "f",
    "g",
    "skills/a/SKILL.md",
    "skills/b/SKILL.md",
    "notes.md",
];

fn tree(files: &[(&str, &str)]) -> Tree {
    let dir = tempfile::tempdir().unwrap();
    for source in SOURCES {
        write(dir.path(), source, "x");
    }
    for (path, text) in files {
        write(dir.path(), path, text);
    }
    let loadout = match Loadout::load(dir.path()) {
        Ok((loadout, _)) => loadout,
        Err(diagnostics) => panic!(
            "fixture must pass static validation:\n{}",
            lines(&diagnostics)
        ),
    };
    Tree { dir, loadout }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn root(body: &str) -> String {
    format!("[loadout]\nversion = 1\n{body}")
}

fn facts(name: &str, os: Os) -> Facts {
    Facts {
        name: name.to_owned(),
        os,
        distro: None,
        arch: Arch::X86_64,
        home: PathBuf::from("/home/synthetic"),
        profiles: Vec::new(),
    }
}

fn lines(diagnostics: &Diagnostics) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.to_string().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tree {
    fn ok(&self, name: &str, os: Os) -> (Resolved, String) {
        match resolve::resolve(&self.loadout, &facts(name, os)) {
            Ok((resolved, warnings)) => (resolved, lines(&warnings)),
            Err(diagnostics) => panic!("{name} on {os} must resolve:\n{}", lines(&diagnostics)),
        }
    }

    fn clean(&self, name: &str, os: Os) -> Resolved {
        let (resolved, warnings) = self.ok(name, os);
        assert_eq!(warnings, "", "{name} on {os}");
        resolved
    }

    fn errors(&self, name: &str, os: Os) -> String {
        match resolve::resolve(&self.loadout, &facts(name, os)) {
            Ok((resolved, _)) => panic!("{name} on {os} must fail, got {resolved:#?}"),
            Err(diagnostics) => {
                assert!(diagnostics.has_errors());
                lines(&diagnostics)
            }
        }
    }

    fn all(&self) -> String {
        lines(&resolve::check_all(&self.loadout))
    }

    fn source(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.loadout.root.clone(), |path, part| path.join(part))
    }
}

fn text(value: &str) -> Variable {
    Variable::Text(value.to_owned())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn stdio(resolved: &Resolved, id: &str) -> (String, Vec<String>, BTreeMap<String, Value>) {
    match &resolved.mcp[id].transport {
        Transport::Stdio { command, args, env } => (command.clone(), args.clone(), env.clone()),
        Transport::Http { .. } => panic!("{id} is not stdio"),
    }
}

#[test]
fn layers_apply_global_then_os_then_tags_in_listed_order_then_machine() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"late\", \"early\"]\n\
                 [variables]\nos = \"global\"\ntag = \"global\"\nmachine_set = \"global\"\nkept = \"global\"\n",
            ),
        ),
        (
            "os/linux.toml",
            "[variables]\nos = \"linux\"\ntag = \"linux\"\nmachine_set = \"linux\"\n",
        ),
        ("os/macos.toml", "[variables]\nos = \"macos\"\n"),
        (
            "tags/late.toml",
            "[variables]\ntag = \"late\"\nmachine_set = \"late\"\n",
        ),
        (
            "tags/early.toml",
            "[variables]\ntag = \"early\"\nmachine_set = \"early\"\n",
        ),
        ("machines/box.toml", "[variables]\nmachine_set = \"box\"\n"),
    ]);
    let resolved = tree.clean("box", Os::Linux);
    assert_eq!(resolved.tags, strings(&["late", "early"]));
    assert_eq!(resolved.variables["os"], text("linux"));
    assert_eq!(
        resolved.variables["tag"],
        text("early"),
        "the tag listed later wins, whatever its file name"
    );
    assert_eq!(resolved.variables["machine_set"], text("box"));
    assert_eq!(resolved.variables["kept"], text("global"));

    let other = tree.clean("box", Os::Macos);
    assert_eq!(other.variables["os"], text("macos"));
    assert_eq!(other.variables["tag"], text("early"));
}

#[test]
fn an_unlisted_machine_gets_only_the_global_and_os_layers() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root("[machines.box]\ntags = [\"work\"]\n[variables]\nwho = \"global\"\n"),
        ),
        ("os/windows.toml", "[variables]\nwho = \"windows\"\n"),
        (
            "tags/work.toml",
            "[variables]\nwho = \"work\"\n[programs.p]\nwinget = \"P\"\n",
        ),
        ("machines/box.toml", "[variables]\nwho = \"box\"\n"),
    ]);
    let stranger = tree.clean("stranger", Os::Windows);
    assert!(stranger.tags.is_empty());
    assert_eq!(stranger.variables["who"], text("windows"));
    assert!(stranger.programs.is_empty());
    assert_eq!(tree.clean("box", Os::Windows).variables["who"], text("box"));
}

#[test]
fn chosen_profiles_follow_the_declared_tags_and_the_machine_layer_still_wins() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root("[machines.box]\ntags = [\"work\"]\n[variables]\nv = \"global\"\n"),
        ),
        ("tags/work.toml", "[variables]\nv = \"work\"\nwork = true\n"),
        ("tags/dev.toml", "[variables]\nv = \"dev\"\ndev = true\n"),
        ("tags/games.toml", "[variables]\nv = \"games\"\n"),
        ("machines/box.toml", "[variables]\nmine = \"box\"\n"),
    ]);
    let chosen = |name: &str, profiles: &[&str]| {
        let facts = Facts {
            profiles: strings(profiles),
            ..facts(name, Os::Linux)
        };
        resolve::resolve(&tree.loadout, &facts).map(|(resolved, _)| resolved)
    };

    let stranger = chosen("stranger", &["dev"]).unwrap();
    assert_eq!(stranger.tags, strings(&["dev"]));
    assert_eq!(stranger.variables["v"], text("dev"));
    assert!(!stranger.variables.contains_key("work"));

    let ordered = chosen("stranger", &["games", "dev"]).unwrap();
    assert_eq!(
        ordered.variables["v"],
        text("dev"),
        "the profile chosen later wins"
    );

    let declared = chosen("box", &["dev", "work"]).unwrap();
    assert_eq!(
        declared.tags,
        strings(&["work", "dev"]),
        "declared tags come first and a profile already declared is not applied twice"
    );
    assert_eq!(declared.variables["v"], text("dev"));
    assert_eq!(declared.variables["work"], Variable::Boolean(true));
    assert_eq!(declared.variables["mine"], text("box"));

    let missing = chosen("stranger", &["gone"]).unwrap_err();
    assert!(
        lines(&missing).contains(
            "error: profiles: this machine chose profile \"gone\" (saved in its state dir's machine.toml), but the loadout has no tags/gone.toml"
        ),
        "{}",
        lines(&missing)
    );
}

#[test]
fn the_last_listed_tag_wins_for_every_order_and_the_machine_beats_every_tag() {
    let names = ["a", "b", "c"];
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for order in orders {
        for machine_layer in [false, true] {
            let listed: Vec<&str> = order.iter().map(|index| names[*index]).collect();
            let mut files = vec![(
                "loadout.toml".to_owned(),
                root(&format!(
                    "[machines.box]\ntags = {listed:?}\n[variables]\nv = \"global\"\n[files.x]\nsource = \"f\"\ntarget = \"~/.global\"\n"
                )),
            )];
            for name in names {
                files.push((
                    format!("tags/{name}.toml"),
                    format!(
                        "[variables]\nv = \"{name}\"\nseen_{name} = true\n[files.x]\ntarget = \"~/.{name}\"\n"
                    ),
                ));
            }
            if machine_layer {
                files.push((
                    "machines/box.toml".to_owned(),
                    "[variables]\nv = \"machine\"\n".to_owned(),
                ));
            }
            let borrowed: Vec<(&str, &str)> = files
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_str()))
                .collect();
            let resolved = tree(&borrowed).clean("box", Os::Linux);
            let last = listed.last().unwrap();
            let expected = if machine_layer { "machine" } else { last };
            assert_eq!(resolved.variables["v"], text(expected), "{listed:?}");
            assert_eq!(
                resolved.files["x"].target,
                format!("~/.{last}"),
                "{listed:?}"
            );
            for name in names {
                assert_eq!(
                    resolved.variables[&format!("seen_{name}")],
                    Variable::Boolean(true),
                    "keys no later layer sets survive: {listed:?}"
                );
            }
        }
    }
}

#[test]
fn a_narrower_layer_adds_edits_replaces_and_removes_items() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"work\"]\n[agents]\nenabled = [\"claude\"]\n\
                 [files.edited]\nsource = \"f\"\ntarget = \"~/.edited\"\n\
                 [files.dropped]\nsource = \"f\"\ntarget = \"~/.dropped\"\n\
                 [mcp.swapped]\ncommand = \"old\"\nargs = [\"-v\"]\nenv = { A = \"1\" }\n\
                 [programs.tool]\nbrew = \"tool\"\napt = \"tool\"\n\
                 [programs.whole]\nbrew = \"whole\"\napt = \"whole\"\n",
            ),
        ),
        (
            "tags/work.toml",
            "[files.edited]\ntarget = \"~/.work-edited\"\n\
             [files.dropped]\nremove = true\n\
             [files.added]\nsource = \"g\"\ntarget = \"~/.added\"\n\
             [mcp.swapped]\nreplace = true\nurl = \"https://example.com/mcp\"\n\
             [programs.tool]\napt = \"tool-ng\"\ndnf = \"tool\"\n\
             [programs.whole]\nreplace = true\ndnf = \"whole\"\n",
        ),
    ]);
    let resolved = tree.clean("box", Os::Linux);
    let edited = &resolved.files["edited"];
    assert_eq!(edited.target, "~/.work-edited");
    assert_eq!(
        edited.source,
        tree.source("f"),
        "an edit keeps the fields it doesn't write"
    );
    assert!(!resolved.files.contains_key("dropped"));
    assert_eq!(resolved.files["added"].source, tree.source("g"));
    assert_eq!(
        resolved.mcp["swapped"].transport,
        Transport::Http {
            url: "https://example.com/mcp".to_owned(),
            headers: BTreeMap::new()
        },
        "replace drops every inherited field, so switching transports is allowed"
    );
    assert_eq!(
        resolved.programs["tool"].packages,
        BTreeMap::from([
            (Manager::Brew, "tool".to_owned()),
            (Manager::Apt, "tool-ng".to_owned()),
            (Manager::Dnf, "tool".to_owned()),
        ])
    );
    assert_eq!(
        resolved.programs["whole"].packages,
        BTreeMap::from([(Manager::Dnf, "whole".to_owned())])
    );

    let untagged = tree.clean("other", Os::Linux);
    assert_eq!(untagged.files["edited"].target, "~/.edited");
    assert!(untagged.files.contains_key("dropped"));
    assert!(!untagged.files.contains_key("added"));
}

#[test]
fn a_removed_item_can_be_declared_again_but_only_complete() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.good]\ntags = [\"off\"]\n[machines.bad]\ntags = [\"off\"]\n\
                 [files.x]\nsource = \"f\"\ntarget = \"~/.x\"\n",
            ),
        ),
        ("tags/off.toml", "[files.x]\nremove = true\n"),
        (
            "machines/good.toml",
            "[files.x]\nsource = \"g\"\ntarget = \"~/.again\"\n",
        ),
        ("machines/bad.toml", "[files.x]\ntarget = \"~/.again\"\n"),
    ]);
    let good = tree.clean("good", Os::Linux);
    assert_eq!(good.files["x"].source, tree.source("g"));
    let errors = tree.errors("bad", Os::Linux);
    assert!(
        errors.contains("machines/bad.toml: error: files.x: adds this item here because no broader layer declares it for this machine, but it is incomplete: missing source"),
        "{errors}"
    );
}

#[test]
fn an_added_item_must_be_complete_in_the_layer_that_adds_it() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root("[machines.box]\ntags = [\"work\"]\n[mcp.kept]\ncommand = \"c\"\n"),
        ),
        (
            "tags/work.toml",
            "[files.half]\ntarget = \"~/.half\"\n\
             [skills.half]\nagents = [\"claude\"]\n\
             [instructions.half]\nwhen = { os = \"linux\" }\n\
             [mcp.half]\nargs = [\"x\"]\n\
             [programs.half]\nwhen = { os = \"linux\" }\n\
             [mcp.kept]\nargs = [\"fine\"]\n",
        ),
        ("machines/box.toml", "[files.half]\nsource = \"f\"\n"),
    ]);
    let errors = tree.errors("box", Os::Linux);
    for (key, field) in [
        ("files.half", "source"),
        ("skills.half", "source"),
        ("instructions.half", "source"),
        ("mcp.half", "command (stdio server) or url (remote server)"),
        ("programs.half", "a package for at least one manager"),
    ] {
        assert!(
            errors.contains(&format!(
                "tags/work.toml: error: {key}: adds this item here because no broader layer declares it for this machine, but it is incomplete: missing {field}"
            )),
            "{key}: {errors}"
        );
    }
    assert!(
        !errors.contains("error: mcp.kept"),
        "an edit of an inherited item is not an add: {errors}"
    );
    assert!(
        errors.contains("machines/box.toml: error: files.half: adds this item here because no broader layer declares it for this machine, but it is incomplete: missing target"),
        "a dropped incomplete item is not completed by a later partial layer: {errors}"
    );
}

#[test]
fn field_edits_that_leave_mixed_transports_are_errors_naming_every_layer() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"work\"]\n[agents]\nenabled = [\"claude\"]\n\
                 [mcp.both]\ncommand = \"c\"\n\
                 [mcp.stdio_on_url]\nurl = \"https://example.com\"\n\
                 [mcp.env_on_url]\nurl = \"https://example.com\"\n\
                 [mcp.headers_on_stdio]\ncommand = \"c\"\n",
            ),
        ),
        (
            "tags/work.toml",
            "[mcp.both]\nurl = \"https://example.com\"\n\
             [mcp.stdio_on_url]\nargs = { add = [\"-v\"] }\n\
             [mcp.env_on_url]\nenv = { A = \"1\" }\n\
             [mcp.headers_on_stdio]\nheaders = { H = \"v\" }\n",
        ),
    ]);
    let errors = tree.errors("box", Os::Linux);
    for (id, problem) in [
        ("both", "has both command (stdio) and url (remote)"),
        (
            "stdio_on_url",
            "is a url server but carries stdio args or env",
        ),
        (
            "env_on_url",
            "is a url server but carries stdio args or env",
        ),
        (
            "headers_on_stdio",
            "is a stdio server but carries url headers",
        ),
    ] {
        assert!(
            errors.contains(&format!(
                "tags/work.toml: error: mcp.{id}: {problem} after merging loadout.toml, tags/work.toml; to switch transports, write replace = true"
            )),
            "{id}: {errors}"
        );
    }
    tree.clean("other", Os::Linux);
}

#[test]
fn a_list_assignment_replaces_and_an_edit_removes_then_appends_what_is_missing() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[agents]\nenabled = [\"claude\"]\n[machines.box]\ntags = [\"one\", \"two\"]\n[machines.reset]\ntags = [\"one\", \"assign\"]\n\
                 [mcp.s]\ncommand = \"c\"\nargs = [\"-v\", \"-v\", \"--old\"]\n",
            ),
        ),
        (
            "tags/one.toml",
            "[mcp.s]\nargs = { remove = [\"--old\"], add = [\"-v\", \"--a\", \"--a\"] }\n",
        ),
        (
            "tags/two.toml",
            "[mcp.s]\nargs = { add = [\"--old\", \"--a\"], remove = [\"-v\"] }\n",
        ),
        ("tags/assign.toml", "[mcp.s]\nargs = [\"--x\", \"--x\"]\n"),
    ]);
    let (_, args, _) = stdio(&tree.clean("other", Os::Linux), "s");
    assert_eq!(
        args,
        strings(&["-v", "-v", "--old"]),
        "an assigned list is kept verbatim"
    );
    let (_, args, _) = stdio(&tree.clean("box", Os::Linux), "s");
    assert_eq!(args, strings(&["--a", "--old"]));
    let (_, args, _) = stdio(&tree.clean("reset", Os::Linux), "s");
    assert_eq!(args, strings(&["--x", "--x"]));
}

#[test]
fn tables_and_conditions_assign_rather_than_merge() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"work\"]\n[agents]\nenabled = [\"claude\"]\n\
                 [mcp.s]\ncommand = \"c\"\nenv = { A = \"1\", B = \"2\" }\n\
                 [mcp.r]\nurl = \"https://example.com\"\nheaders = { X = \"1\", Y = \"2\" }\n\
                 [files.x]\nsource = \"f\"\ntarget = \"~/.x\"\nwhen = { os = \"linux\", arch = \"aarch64\" }\n",
            ),
        ),
        (
            "tags/work.toml",
            "[mcp.s]\nenv = { B = { env = \"TOKEN_B\" } }\n\
             [mcp.r]\nheaders = { Y = \"3\" }\n\
             [files.x]\nwhen = { os = \"linux\" }\n",
        ),
    ]);
    let resolved = tree.clean("box", Os::Linux);
    let (_, _, env) = stdio(&resolved, "s");
    assert_eq!(env.keys().collect::<Vec<_>>(), ["B"]);
    let Transport::Http { headers, .. } = &resolved.mcp["r"].transport else {
        panic!("r is a url server");
    };
    assert_eq!(
        headers,
        &BTreeMap::from([("Y".to_owned(), Value::Text("3".to_owned()))])
    );
    assert!(
        resolved.files.contains_key("x"),
        "the narrower when replaced the arch condition"
    );
    assert!(!tree.clean("other", Os::Linux).files.contains_key("x"));
}

#[test]
fn variables_and_update_merge_per_key_and_field_over_defaults() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root("[machines.box]\ntags = [\"t\"]\n[variables]\na = 1\nb = true\n"),
        ),
        ("os/linux.toml", "[update]\ninterval_minutes = 30\n"),
        (
            "tags/t.toml",
            "[variables]\nb = \"text\"\n[update]\nauto = false\n",
        ),
    ]);
    let resolved = tree.clean("box", Os::Linux);
    assert_eq!(resolved.variables["a"], Variable::Integer(1));
    assert_eq!(resolved.variables["b"], text("text"));
    assert!(!resolved.update.auto);
    assert_eq!(resolved.update.interval_minutes, 30);
    let windows = tree.clean("other", Os::Windows);
    assert!(windows.update.auto);
    assert_eq!(windows.update.interval_minutes, 60);

    let bare = self::tree(&[("loadout.toml", &root(""))]).clean("any", Os::Macos);
    assert_eq!(bare.update, resolve::Update::default());
    assert!(bare.variables.is_empty());
}

/// `[update] cli` once turned the CLI self-update after each update on or off. Existing loadouts
/// that set it stay valid, but it does nothing: the settings resolve as if it weren't there, and
/// a warning says so, in every layer that sets it.
#[test]
fn the_legacy_update_cli_key_is_accepted_with_a_warning_and_changes_nothing() {
    for value in ["true", "false"] {
        let tree = tree(&[
            (
                "loadout.toml",
                &root(&format!("[update]\ncli = {value}\ninterval_minutes = 30\n")),
            ),
            ("os/linux.toml", "[update]\ncli = true\n"),
        ]);
        // The key is checked when the loadout is read, so its warnings come from loading.
        let (_, loaded) = Loadout::load(tree.dir.path()).unwrap();
        let warnings = lines(&loaded);
        let resolved = tree.clean("any", Os::Linux);
        assert_eq!(
            resolved.update,
            resolve::Update {
                auto: true,
                interval_minutes: 30,
            },
            "cli = {value}"
        );
        assert_eq!(
            warnings.matches("update.cli").count(),
            2,
            "one warning for each layer: {warnings}"
        );
        assert!(!loaded.has_errors(), "{warnings}");
        assert!(warnings.contains("`loadout update`"), "{warnings}");
    }
}

#[test]
fn no_agent_is_enabled_by_default_and_enabled_lists_follow_the_list_rules() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n[machines.bare]\n\
                 [skills.a]\nsource = \"skills/a\"\n",
            ),
        ),
        (
            "os/macos.toml",
            "[agents]\nenabled = [\"claude\", \"codex\", \"claude\"]\n",
        ),
        (
            "tags/t.toml",
            "[agents]\nenabled = { remove = [\"claude\"], add = [\"cursor\"] }\n",
        ),
    ]);
    let (linux, warnings) = tree.ok("bare", Os::Linux);
    assert!(linux.agents.is_empty());
    assert!(linux.skills["a"].agents.is_empty());
    assert_eq!(
        warnings,
        "loadout.toml: warning: skills.a: reaches no agent, because no agent is enabled for this machine ([agents] enabled)"
    );
    let mac = tree.clean("bare", Os::Macos);
    let ids: Vec<&str> = mac.agents.iter().map(Agent::id).collect();
    assert_eq!(
        ids,
        ["claude", "codex"],
        "an assigned id list keeps each agent once"
    );
    let tagged = tree.clean("box", Os::Macos);
    let ids: Vec<&str> = tagged.agents.iter().map(Agent::id).collect();
    assert_eq!(ids, ["codex", "cursor"]);
    assert_eq!(tagged.skills["a"].agents, strings(&["codex", "cursor"]));
    assert_eq!(
        tagged.agents[0],
        Agent::Builtin {
            agent: BuiltinAgent::Codex
        }
    );
}

#[test]
fn an_item_agent_list_narrows_the_enabled_set_and_warns_about_disabled_names() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n\
                 [agents]\nenabled = [\"claude\", \"codex\", \"pi\"]\n\
                 [skills.a]\nsource = \"skills/a\"\nagents = [\"codex\", \"cursor\"]\n\
                 [skills.b]\nsource = \"skills/b\"\n\
                 [mcp.m]\ncommand = \"c\"\nagents = [\"claude\"]\n",
            ),
        ),
        (
            "tags/t.toml",
            "[skills.b]\nagents = { remove = [\"pi\"] }\n[mcp.m]\nagents = { add = [\"grok\", \"pi\"] }\n",
        ),
    ]);
    let (resolved, warnings) = tree.ok("box", Os::Linux);
    assert_eq!(resolved.skills["a"].agents, strings(&["codex"]));
    assert_eq!(
        resolved.skills["b"].agents,
        strings(&["claude", "codex"]),
        "an edit of an unset list edits the enabled agents"
    );
    assert_eq!(resolved.mcp["m"].agents, strings(&["claude", "pi"]));
    assert_eq!(
        warnings,
        "loadout.toml: warning: skills.a.agents: names \"cursor\", which is not enabled for this machine, so it gets nothing\n\
         tags/t.toml: warning: mcp.m.agents: names \"grok\", which is not enabled for this machine, so it gets nothing"
    );
}

#[test]
fn a_custom_agent_declared_again_is_replaced_whole_and_must_be_declared_where_enabled() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n\
                 [agents]\nenabled = [\"devin\"]\n\
                 [instructions.i]\nsource = \"notes.md\"\n",
            ),
        ),
        (
            "os/linux.toml",
            "[agents.custom.devin]\nevidence = \"linux docs\"\nskills = { linux = \"~/.devin/skills\" }\n\
             mcp = { linux = \"~/.devin/mcp.json\", format = \"json\", map = \"/mcpServers\", shape = \"cursor-json\", env_syntax = \"none\" }\n",
        ),
        (
            "tags/t.toml",
            "[agents.custom.devin]\nevidence = \"tag docs\"\ninstructions = { linux = \"~/.devin/AGENTS.md\" }\n",
        ),
    ]);
    let tagged = tree.clean("box", Os::Linux);
    let Agent::Custom { id, declaration } = &tagged.agents[0] else {
        panic!("devin is custom");
    };
    assert_eq!(id, "devin");
    assert_eq!(declaration.evidence, "tag docs");
    assert!(
        declaration.skills.is_none(),
        "nothing of the broader declaration survives"
    );
    assert!(declaration.mcp.is_none());
    assert_eq!(
        declaration.instructions.as_ref().unwrap().linux.as_deref(),
        Some("~/.devin/AGENTS.md")
    );
    assert_eq!(tagged.instructions["i"].agents, strings(&["devin"]));

    let errors = tree.errors("other", Os::Macos);
    assert_eq!(
        errors,
        "loadout.toml: error: agents.enabled: enables \"devin\", but no layer that applies to this machine declares [agents.custom.devin]\n\
         loadout.toml: warning: instructions.i: reaches no agent, because no agent is enabled for this machine ([agents] enabled)"
    );
}

#[test]
fn conditions_apply_after_the_merge() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n\
                 [files.mac]\nsource = \"f\"\ntarget = \"~/.mac\"\nwhen = { os = \"macos\" }\n\
                 [files.box_only]\nsource = \"f\"\ntarget = \"~/.box\"\nwhen = { machine = \"box\" }\n\
                 [programs.ubuntu]\napt = \"x\"\nwhen = { distro = \"ubuntu\" }\n",
            ),
        ),
        (
            "os/linux.toml",
            "[files.mac]\ntarget = \"~/.edited-on-linux\"\n",
        ),
        (
            "tags/t.toml",
            "[files.mac]\nwhen = { os = [\"macos\", \"linux\"] }\n",
        ),
    ]);
    let other = tree.clean("other", Os::Linux);
    assert!(
        !other.files.contains_key("mac"),
        "an edit of an item absent on this OS is still an edit, not an add"
    );
    assert!(!other.files.contains_key("box_only"));
    let tagged = tree.clean("box", Os::Linux);
    assert_eq!(tagged.files["mac"].target, "~/.edited-on-linux");
    assert!(tagged.files.contains_key("box_only"));
    assert!(!tagged.programs.contains_key("ubuntu"));
    let mut ubuntu = facts("box", Os::Linux);
    ubuntu.distro = Some("ubuntu".to_owned());
    let (resolved, _) = resolve::resolve(&tree.loadout, &ubuntu).unwrap();
    assert!(resolved.programs.contains_key("ubuntu"));
}

#[test]
fn removing_an_item_no_broader_layer_declares_is_a_warning() {
    let tree = tree(&[
        ("loadout.toml", &root("[machines.box]\ntags = [\"t\"]\n")),
        (
            "os/macos.toml",
            "[files.mac]\nsource = \"f\"\ntarget = \"~/.mac\"\n",
        ),
        ("tags/t.toml", "[files.mac]\nremove = true\n"),
    ]);
    tree.clean("box", Os::Macos);
    let (_, warnings) = tree.ok("box", Os::Linux);
    assert_eq!(
        warnings,
        "tags/t.toml: warning: files.mac: removes an item that no broader layer declares for this machine"
    );
}

#[test]
fn at_most_one_instructions_item_reaches_each_agent() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n\
                 [agents]\nenabled = [\"claude\", \"codex\", \"pi\"]\n\
                 [instructions.main]\nsource = \"notes.md\"\n",
            ),
        ),
        (
            "tags/t.toml",
            "[instructions.extra]\nsource = \"f\"\nagents = [\"codex\", \"pi\"]\n",
        ),
        (
            "os/macos.toml",
            "[instructions.main]\nagents = [\"claude\"]\n",
        ),
    ]);
    let errors = tree.errors("box", Os::Linux);
    assert_eq!(
        errors,
        "loadout.toml: error: instructions.main: instructions.extra, instructions.main all reach codex, pi; an agent takes one instructions file, so narrow them with agents = [...]"
    );
    let resolved = tree.clean("box", Os::Macos);
    assert_eq!(resolved.instructions["main"].agents, strings(&["claude"]));
    assert_eq!(
        resolved.instructions["extra"].agents,
        strings(&["codex", "pi"])
    );
}

#[test]
fn a_final_target_with_no_valid_windows_path_fails_only_on_windows() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.fixed]\n\
                 [files.notes]\nsource = \"f\"\ntarget = \"~/notes:2026\"\n\
                 [files.device]\nsource = \"f\"\ntarget = \"~/logs/aux.txt\"\n",
            ),
        ),
        (
            "machines/fixed.toml",
            "[files.notes]\nremove = true\n[files.device]\ntarget = \"~/logs/aux-log.txt\"\n",
        ),
    ]);
    let linux = tree.ok("other", Os::Linux).0;
    assert_eq!(linux.files["notes"].target, "~/notes:2026");
    tree.ok("other", Os::Macos);
    let errors = tree.errors("other", Os::Windows);
    assert!(
        errors.contains("loadout.toml: error: files.notes.target: \"notes:2026\" contains ':'"),
        "{errors}"
    );
    assert!(
        errors.contains("so ~/notes:2026 has no valid Windows path for files.notes"),
        "{errors}"
    );
    assert!(
        errors.contains("files.device.target: \"aux.txt\" is a Windows device name"),
        "{errors}"
    );
    let fixed = tree.ok("fixed", Os::Windows).0;
    assert_eq!(fixed.files["device"].target, "~/logs/aux-log.txt");
}

#[test]
fn expanded_targets_collide_across_layers_under_each_os_case_rule() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.box]\ntags = [\"t\"]\n\
                 [files.home_config]\nsource = \"f\"\ntarget = \"~/.config/app/conf\"\n\
                 [files.lower]\nsource = \"f\"\ntarget = \"~/.profile\"\n\
                 [files.folder]\nsource = \"f\"\ntarget = \"~/.tool\"\n",
            ),
        ),
        (
            "tags/t.toml",
            "[files.native_config]\nsource = \"g\"\ntarget = \"{config}/app/conf\"\n\
             [files.upper]\nsource = \"g\"\ntarget = \"~/.PROFILE\"\n\
             [files.inside]\nsource = \"g\"\ntarget = \"~/.tool/settings\"\n",
        ),
    ]);
    let linux = tree.errors("box", Os::Linux);
    assert!(
        linux.contains(
            "tags/t.toml: error: files.native_config.target: files.native_config ({config}/app/conf) and files.home_config (~/.config/app/conf) both write ~/.config/app/conf"
        ),
        "{linux}"
    );
    assert!(
        !linux.contains("files.upper"),
        "Linux is case-sensitive: {linux}"
    );
    assert!(
        linux.contains(
            "files.inside.target: files.inside writes ~/.tool/settings inside files.folder (~/.tool); one path cannot be both a file and a folder"
        ),
        "{linux}"
    );
    for os in [Os::Macos, Os::Windows] {
        let errors = tree.errors("box", os);
        assert!(
            errors.contains(
                "files.upper.target: files.upper (~/.PROFILE) and files.lower (~/.profile) both write ~/.PROFILE"
            ),
            "{errors}"
        );
        assert!(
            !errors.contains("native_config"),
            "{{config}} is not ~/.config on {os}: {errors}"
        );
    }
    tree.clean("other", Os::Linux);
}

#[test]
fn file_targets_collide_with_the_paths_an_enabled_custom_agent_receives() {
    let manifest = |enabled: &str| {
        root(&format!(
            "[agents]\nenabled = [{enabled}]\n\
             [agents.custom.devin]\nevidence = \"e\"\nskills = {{ linux = \"~/.devin/skills\" }}\n\
             instructions = {{ linux = \"~/.devin/AGENTS.md\" }}\n\
             mcp = {{ linux = \"~/.devin/mcp.json\", format = \"json\", map = \"/mcpServers\", shape = \"cursor-json\", env_syntax = \"none\" }}\n\
             [skills.a]\nsource = \"skills/a\"\n\
             [instructions.i]\nsource = \"notes.md\"\n\
             [mcp.m]\ncommand = \"c\"\n\
             [files.skill]\nsource = \"f\"\ntarget = \"~/.devin/skills/a/SKILL.md\"\n\
             [files.rules]\nsource = \"f\"\ntarget = \"~/.devin/AGENTS.md\"\n\
             [files.servers]\nsource = \"f\"\ntarget = \"~/.devin/mcp.json\"\n\
             [files.beside]\nsource = \"f\"\ntarget = \"~/.devin/skills/b/SKILL.md\"\n"
        ))
    };
    let errors = tree(&[("loadout.toml", &manifest("\"devin\""))]).errors("any", Os::Linux);
    assert!(
        errors.contains(
            "loadout.toml: error: agents.custom.devin.skills.linux: files.skill writes ~/.devin/skills/a/SKILL.md inside skills.a for devin (~/.devin/skills/a); one path"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("instructions.i for devin (~/.devin/AGENTS.md) and files.rules (~/.devin/AGENTS.md) both write"),
        "{errors}"
    );
    assert!(
        errors.contains("the MCP servers for devin (~/.devin/mcp.json) and files.servers (~/.devin/mcp.json) both write"),
        "{errors}"
    );
    assert!(!errors.contains("files.beside"), "{errors}");
    let (_, warnings) = tree(&[("loadout.toml", &manifest(""))]).ok("any", Os::Linux);
    assert!(
        !warnings.contains("error"),
        "a disabled agent receives nothing: {warnings}"
    );
}

#[test]
fn check_all_names_each_machine_and_os_a_problem_appears_on() {
    let tree = tree(&[
        (
            "loadout.toml",
            &root(
                "[machines.laptop]\ntags = [\"work\"]\n[machines.desk]\n\
                 [files.ghost]\nsource = \"f\"\ntarget = \"~/ghost:file\"\nwhen = { machine = \"ghost\" }\n\
                 [files.arm]\nsource = \"f\"\ntarget = \"~/.arm\"\nwhen = { arch = \"aarch64\" }\n",
            ),
        ),
        (
            "tags/work.toml",
            "[files.bad]\nsource = \"f\"\ntarget = \"~/a|b\"\n",
        ),
        (
            "os/linux.toml",
            "[files.distro]\nsource = \"g\"\ntarget = \"~/.arm\"\nwhen = { distro = \"fedora\" }\n",
        ),
    ]);
    let all = tree.all();
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines.len(), 3, "{all}");
    assert!(
        lines.contains(&"tags/work.toml: error: files.bad.target: \"a|b\" contains '|', which Windows paths cannot hold (drive, stream, or reserved character), so ~/a|b has no valid Windows path for files.bad; remove or override it in os/windows.toml, or add a when condition (on laptop: windows; a machine with only profile work: windows)"),
        "{all}"
    );
    assert!(
        lines.iter().any(
            |line| line.starts_with("loadout.toml: error: files.ghost.target:")
                && line.ends_with("(on unlisted machine ghost: windows)")
        ),
        "{all}"
    );
    assert!(
        lines.contains(&"os/linux.toml: error: files.distro.target: files.distro (~/.arm) and files.arm (~/.arm) both write ~/.arm (on desk: linux aarch64 fedora; laptop: linux aarch64 fedora; unlisted machine ghost: linux aarch64 fedora; a machine with only profile work: linux aarch64 fedora; any other unlisted machine: linux aarch64 fedora)"),
        "{all}"
    );
}

#[test]
fn check_all_is_empty_for_a_clean_loadout_and_reports_a_shared_problem_once() {
    let clean = tree(&[(
        "loadout.toml",
        &root(
            "[machines.a]\n[agents]\nenabled = [\"claude\"]\n[skills.a]\nsource = \"skills/a\"\n",
        ),
    )]);
    assert_eq!(clean.all(), "");
    let everywhere = tree(&[(
        "loadout.toml",
        &root("[machines.a]\n[skills.a]\nsource = \"skills/a\"\n"),
    )]);
    assert_eq!(
        everywhere.all(),
        "loadout.toml: warning: skills.a: reaches no agent, because no agent is enabled for this machine ([agents] enabled) (on every machine and OS)"
    );
}

#[test]
fn resolved_paths_are_absolute_inside_the_loadout_and_mark_templates() {
    let tree = tree(&[
        ("files/git.tmpl", "{{ email }}"),
        (
            "loadout.toml",
            &root(
                "[agents]\nenabled = [\"claude\"]\n\
                 [files.git]\nsource = \"files/git.tmpl\"\ntarget = \"~/.gitconfig\"\n\
                 [files.plain]\nsource = \"f\"\ntarget = \"~/.plain\"\n\
                 [skills.a]\nsource = \"skills/a\"\n[instructions.i]\nsource = \"notes.md\"\n",
            ),
        ),
    ]);
    let resolved = tree.clean("any", Os::Linux);
    assert!(resolved.files["git"].template);
    assert!(!resolved.files["plain"].template);
    assert_eq!(resolved.files["git"].source, tree.source("files/git.tmpl"));
    assert_eq!(resolved.skills["a"].source, tree.source("skills/a"));
    assert_eq!(resolved.instructions["i"].source, tree.source("notes.md"));
    assert!(resolved.skills["a"].source.join("SKILL.md").is_file());
    assert!(tree.dir.path().exists());
}

mod targets {
    use loadout::context::Context;

    use super::*;

    fn isolated(os: Os, home: &Path) -> Context {
        let home = home.as_os_str().to_owned();
        Context::resolve(
            os,
            move |name| (name == "LOADOUT_HOME").then(|| home.clone()),
            None::<PathBuf>,
        )
        .unwrap()
    }

    fn manifest() -> String {
        root(
            "[agents]\nenabled = [\"devin\"]\n\
             [agents.custom.devin]\nevidence = \"e\"\nskills = { linux = \"~/.devin/skills\", macos = \"~/.devin/skills\", windows = \"~/.devin/skills\" }\n\
             mcp = { linux = \"~/.claude.json\", macos = \"~/.claude.json\", windows = \"~/.claude.json\", format = \"json\", map = \"/mcpServers\", shape = \"claude-json\", env_syntax = \"${VAR}\" }\n\
             [files.plain]\nsource = \"f\"\ntarget = \"~/.gitconfig\"\n",
        )
    }

    #[test]
    fn clean_targets_and_a_shared_config_key_merge_pass() {
        let tree = tree(&[("loadout.toml", &manifest())]);
        let home = tempfile::tempdir().unwrap();
        for os in Os::ALL {
            let resolved = tree.clean("any", os);
            let context = isolated(os, home.path());
            let diagnostics = resolve::check_targets(&resolved, &context, &tree.loadout.root);
            assert!(diagnostics.is_empty(), "{os}: {}", lines(&diagnostics));
        }
    }

    #[test]
    fn a_target_inside_the_active_checkout_is_reserved() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("dotfiles");
        fs::create_dir(&checkout).unwrap();
        write(&checkout, "f", "x");
        write(
            &checkout,
            "loadout.toml",
            &root("[files.self]\nsource = \"f\"\ntarget = \"~/dotfiles/f\"\n"),
        );
        let (loadout, _) = Loadout::load(&checkout).unwrap();
        let (resolved, _) = resolve::resolve(&loadout, &facts("any", Os::Linux)).unwrap();
        let context = isolated(Os::Linux, home.path());
        let diagnostics = resolve::check_targets(&resolved, &context, &loadout.root);
        let text = lines(&diagnostics);
        assert_eq!(diagnostics.count(Severity::Error), 1, "{text}");
        assert!(text.contains("error: files.self.target:"), "{text}");
        assert!(
            text.contains("loadout's own state, identity, or clone"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_of_a_protected_dir_is_caught_through_the_link() {
        let tree = tree(&[(
            "loadout.toml",
            &root("[files.alias]\nsource = \"f\"\ntarget = \"~/alias/auth.json\"\n"),
        )]);
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".codex")).unwrap();
        std::os::unix::fs::symlink(home.path().join(".codex"), home.path().join("alias")).unwrap();
        let resolved = tree.clean("any", Os::Linux);
        let context = isolated(Os::Linux, home.path());
        let text = lines(&resolve::check_targets(
            &resolved,
            &context,
            &tree.loadout.root,
        ));
        assert!(text.contains("files.alias.target"), "{text}");
        assert!(text.contains("auth or OAuth store"), "{text}");
        assert!(
            !home.path().join(".codex/auth.json").exists(),
            "the check only stats paths"
        );
    }

    #[test]
    fn a_resolved_target_windows_cannot_hold_fails_the_windows_runtime_check() {
        let tree = tree(&[(
            "loadout.toml",
            &root("[files.colon]\nsource = \"f\"\ntarget = \"~/a:b\"\n"),
        )]);
        let resolved = tree.ok("any", Os::Linux).0;
        let home = tempfile::tempdir().unwrap();
        let text = lines(&resolve::check_targets(
            &resolved,
            &isolated(Os::Windows, home.path()),
            &tree.loadout.root,
        ));
        assert_eq!(
            text,
            "error: files.colon.target: ~/a:b is not a valid path on windows"
        );
        let linux = lines(&resolve::check_targets(
            &resolved,
            &isolated(Os::Linux, home.path()),
            &tree.loadout.root,
        ));
        if cfg!(windows) {
            // A native Windows join reads the part `a:b` as the drive-relative path `a:b`, which
            // leaves the home dir, so even a context claiming Linux must refuse it, not write it.
            assert!(
                linux.starts_with(
                    "error: files.colon.target: a:b cannot verify a:b is not a reserved destination"
                ),
                "{linux}"
            );
        } else {
            assert_eq!(
                linux, "",
                "a colon is an ordinary POSIX file name character"
            );
        }
    }
}
