use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use loadout::agents::{self, Error};
use loadout::context::Context;
use loadout::machine::Facts;
use loadout::manifest::{Loadout, McpFormat, McpShape};
use loadout::plan::{Action, Change, Content, Desired, ItemKind, Observed, Target};
use loadout::platform::{Arch, Os};
use loadout::resolve;
use serde_json::{Value as Json, json};

const ALL: &str =
    r#"["claude", "codex", "cursor", "grok", "muse", "pi", "opencode", "antigravity"]"#;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    loadout: PathBuf,
}

fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let loadout = dir.path().join("loadout");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&loadout).unwrap();
    let sandbox = Sandbox {
        _dir: dir,
        home,
        loadout,
    };
    sandbox.source(
        "skills/review/SKILL.md",
        "---\nname: review\n---\nReview {{ code }}.\n",
    );
    sandbox.source("skills/review/notes/checklist.md", "- read it\n");
    sandbox.source("instructions/AGENTS.md", "Be brief. {{ not_rendered }}\n");
    sandbox
}

fn manifest(body: &str) -> String {
    format!(
        "[loadout]\nversion = 1\n\n[skills.review]\nsource = \"skills/review\"\n\n[instructions.global]\nsource = \"instructions/AGENTS.md\"\n\n[mcp.files]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\nenv = {{ LOG_LEVEL = \"warn\" }}\n\n[mcp.github]\nurl = \"https://api.example.com/mcp/\"\nheaders = {{ Authorization = {{ env = \"GITHUB_TOKEN\", prefix = \"Bearer \" }} }}\n\n{body}"
    )
}

impl Sandbox {
    fn relocated(mut self, to: PathBuf) -> Self {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(&self.loadout, &to).unwrap();
        self.loadout = to;
        self
    }

    fn source(&self, path: &str, text: &str) {
        let path = self.loadout.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn home_file(&self, path: &str, text: &str) -> PathBuf {
        let path = under(&self.home, path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn isolated(&self, os: Os) -> Context {
        let home = self.home.clone();
        Context::resolve(
            os,
            move |name| (name == "LOADOUT_HOME").then(|| home.clone().into_os_string()),
            None,
        )
        .unwrap()
    }

    fn ambient(&self, os: Os, vars: &[(&str, PathBuf)]) -> Context {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone().into_os_string()))
            .collect();
        Context::resolve(
            os,
            move |name| {
                vars.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            },
            Some(self.home.clone()),
        )
        .unwrap()
    }

    fn run(&self, text: &str, context: &Context) -> Result<(Vec<Desired>, Vec<Action>), Error> {
        fs::write(self.loadout.join("loadout.toml"), text).unwrap();
        let (loadout, _) = Loadout::load(&self.loadout)
            .unwrap_or_else(|diagnostics| panic!("fixture must validate: {diagnostics:?}"));
        let facts = Facts {
            name: "box".to_owned(),
            os: context.os,
            distro: None,
            arch: Arch::X86_64,
            home: context.home.clone(),
        };
        let (resolved, _) = resolve::resolve(&loadout, &facts)
            .unwrap_or_else(|diagnostics| panic!("fixture must resolve: {diagnostics:?}"));
        agents::materialize(&resolved, context, &self.loadout)
    }

    /// The path U4 takes: the guarded load, resolve, then the runtime reserved check on targets.
    fn checked(&self, text: &str, context: &Context) -> Result<resolve::Resolved, String> {
        fs::write(self.loadout.join("loadout.toml"), text).unwrap();
        let (loadout, _) = Loadout::load_guarded(&self.loadout, context)
            .unwrap_or_else(|diagnostics| panic!("fixture must validate: {diagnostics:?}"));
        let facts = Facts {
            name: "box".to_owned(),
            os: context.os,
            distro: None,
            arch: Arch::X86_64,
            home: context.home.clone(),
        };
        let (resolved, _) = resolve::resolve(&loadout, &facts).map_err(|errors| lines(&errors))?;
        let targets = resolve::check_targets(&resolved, context, &self.loadout);
        assert!(!targets.has_errors(), "{}", lines(&targets));
        Ok(resolved)
    }

    fn ok(&self, text: &str, context: &Context) -> (Vec<Desired>, Vec<Action>) {
        self.run(text, context)
            .unwrap_or_else(|error| panic!("materialize must succeed: {error}: {error:?}"))
    }

    fn invalid(&self, text: &str, context: &Context) -> String {
        match self.run(text, context) {
            Ok(_) => panic!("materialize must refuse this loadout"),
            Err(Error::Invalid(diagnostics)) => diagnostics
                .iter()
                .map(|diagnostic| diagnostic.to_string().replace('\\', "/"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.home)
            .unwrap_or_else(|_| panic!("{} is outside the home", path.display()))
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn snapshot(&self) -> BTreeSet<(PathBuf, Option<Vec<u8>>)> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![self.home.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                    seen.insert((path, None));
                } else if meta.is_file() {
                    let bytes = fs::read(&path).unwrap();
                    seen.insert((path, Some(bytes)));
                } else {
                    seen.insert((path, None));
                }
            }
        }
        seen
    }
}

fn lines(diagnostics: &loadout::diagnostic::Diagnostics) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.to_string().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn refusal(resolved: &resolve::Resolved, context: &Context, checkout: &Path) -> String {
    match agents::materialize(resolved, context, checkout) {
        Ok(_) => panic!("materialize must refuse this checkout"),
        Err(Error::Invalid(diagnostics)) => lines(&diagnostics),
    }
}

fn under(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

fn find<'a>(desired: &'a [Desired], kind: ItemKind, id: &str, agent: &str) -> &'a Desired {
    desired
        .iter()
        .find(|one| {
            one.item.kind == kind && one.item.id == id && one.item.agent.as_deref() == Some(agent)
        })
        .unwrap_or_else(|| panic!("no desired {kind:?} {id} for {agent}"))
}

fn refused<'a>(actions: &'a [Action], kind: ItemKind, id: &str, agent: &str) -> &'a Action {
    actions
        .iter()
        .find(|one| {
            one.item.kind == kind && one.item.id == id && one.item.agent.as_deref() == Some(agent)
        })
        .unwrap_or_else(|| panic!("no unsupported {kind:?} {id} for {agent}"))
}

fn reason(action: &Action) -> &str {
    match &action.change {
        Change::Unsupported(reason) => reason.as_str(),
        other => panic!("expected unsupported, got {other:?}"),
    }
}

fn path_of(desired: &Desired) -> &Path {
    match &desired.target {
        Target::Path { path } => path.as_path(),
        other => panic!("expected a path target, got {other:?}"),
    }
}

fn entry_of(desired: &Desired) -> (&Path, McpFormat, &str, &str, McpShape) {
    match &desired.target {
        Target::Entry {
            file,
            format,
            pointer,
            key,
            shape,
        } => (
            file.as_path(),
            *format,
            pointer.as_str(),
            key.as_str(),
            *shape,
        ),
        other => panic!("expected an entry target, got {other:?}"),
    }
}

fn entry_json(desired: &Desired) -> &Json {
    match &desired.content {
        Content::Entry(value) => value,
        other => panic!("expected entry content, got {other:?}"),
    }
}

fn agents_line(list: &str) -> String {
    format!("[agents]\nenabled = {list}\n")
}

#[test]
fn every_builtin_agent_takes_each_kind_at_its_documented_linux_path() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let (desired, unsupported) = sandbox.ok(&manifest(&agents_line(ALL)), &context);

    let expected = [
        (
            "claude",
            ".claude/skills/review",
            Some(".claude/CLAUDE.md"),
            ".claude.json",
            "/mcpServers",
            McpShape::ClaudeJson,
            McpFormat::Json,
        ),
        (
            "codex",
            ".agents/skills/review",
            Some(".codex/AGENTS.md"),
            ".codex/config.toml",
            "/mcp_servers",
            McpShape::CodexToml,
            McpFormat::Toml,
        ),
        (
            "cursor",
            ".cursor/skills/review",
            None,
            ".cursor/mcp.json",
            "/mcpServers",
            McpShape::CursorJson,
            McpFormat::Json,
        ),
        (
            "grok",
            ".grok/skills/review",
            Some(".grok/AGENTS.md"),
            ".grok/config.toml",
            "/mcp_servers",
            McpShape::GrokToml,
            McpFormat::Toml,
        ),
        (
            "muse",
            ".config/muse/skills/review",
            None,
            ".config/muse/settings.json",
            "/mcp_servers",
            McpShape::MuseJson,
            McpFormat::Json,
        ),
        (
            "pi",
            ".pi/agent/skills/review",
            Some(".pi/agent/AGENTS.md"),
            ".pi/agent/mcp.json",
            "/mcpServers",
            McpShape::PiJson,
            McpFormat::Json,
        ),
        (
            "opencode",
            ".config/opencode/skills/review",
            Some(".config/opencode/AGENTS.md"),
            ".config/opencode/opencode.json",
            "/mcp",
            McpShape::OpencodeJson,
            McpFormat::Json,
        ),
        (
            "antigravity",
            ".gemini/antigravity-cli/skills/review",
            Some(".gemini/AGENTS.md"),
            ".gemini/config/mcp_config.json",
            "/mcpServers",
            McpShape::AntigravityJson,
            McpFormat::Json,
        ),
    ];
    for (agent, skill, instructions, mcp, map, shape, format) in expected {
        let one = find(&desired, ItemKind::Skill, "review", agent);
        assert_eq!(sandbox.relative(path_of(one)), skill, "{agent} skill");
        if let Some(path) = instructions {
            let one = find(&desired, ItemKind::Instructions, "global", agent);
            assert_eq!(sandbox.relative(path_of(one)), path, "{agent} instructions");
        } else {
            let action = refused(&unsupported, ItemKind::Instructions, "global", agent);
            assert_eq!(
                action.target,
                Target::None,
                "{agent} has no instructions file"
            );
        }
        let one = find(&desired, ItemKind::Mcp, "files", agent);
        let (file, actual_format, pointer, key, actual_shape) = entry_of(one);
        assert_eq!(sandbox.relative(file), mcp, "{agent} mcp file");
        assert_eq!(
            (actual_format, pointer, key, actual_shape),
            (format, map, "files", shape),
            "{agent} mcp map"
        );
    }

    let cursor = refused(&unsupported, ItemKind::Instructions, "global", "cursor");
    assert!(
        reason(cursor).contains("app settings"),
        "{}",
        reason(cursor)
    );
    let muse = refused(&unsupported, ItemKind::Instructions, "global", "muse");
    assert!(reason(muse).contains("no path"), "{}", reason(muse));

    let antigravity = refused(&unsupported, ItemKind::Mcp, "github", "antigravity");
    assert!(matches!(
        antigravity.target,
        Target::Entry {
            shape: McpShape::AntigravityJson,
            ..
        }
    ));
    assert!(
        reason(antigravity).contains("no documented env syntax"),
        "{}",
        reason(antigravity)
    );

    for action in &unsupported {
        assert_eq!(action.prior, Observed::Absent);
        assert!(action.content.is_none());
        assert!(matches!(action.change, Change::Unsupported(_)));
    }
    assert_eq!(unsupported.len(), 3, "{unsupported:#?}");
    assert_eq!(desired.len(), 8 + 6 + 8 + 7);
}

#[test]
fn mcp_entries_carry_each_agents_env_reference_never_a_value() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let (desired, _) = sandbox.ok(&manifest(&agents_line(ALL)), &context);

    let claude = entry_json(find(&desired, ItemKind::Mcp, "github", "claude"));
    assert_eq!(
        claude,
        &json!({ "type": "http", "url": "https://api.example.com/mcp/", "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" } })
    );
    let codex = entry_json(find(&desired, ItemKind::Mcp, "github", "codex"));
    assert_eq!(codex["bearer_token_env_var"], "GITHUB_TOKEN");
    let cursor = entry_json(find(&desired, ItemKind::Mcp, "github", "cursor"));
    assert_eq!(
        cursor["headers"]["Authorization"],
        "Bearer ${env:GITHUB_TOKEN}"
    );
    let opencode = entry_json(find(&desired, ItemKind::Mcp, "files", "opencode"));
    assert_eq!(opencode["command"], json!(["npx", "-y", "server"]));
    let antigravity = entry_json(find(&desired, ItemKind::Mcp, "files", "antigravity"));
    assert_eq!(antigravity["env"]["LOG_LEVEL"], "warn");
}

#[test]
fn skills_copy_the_whole_folder_and_instructions_are_never_rendered() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let (desired, _) = sandbox.ok(&manifest(&agents_line(r#"["claude"]"#)), &context);

    let Content::Tree(tree) = &find(&desired, ItemKind::Skill, "review", "claude").content else {
        panic!("a skill is a tree");
    };
    let keys: Vec<String> = tree
        .keys()
        .map(|key| key.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(keys, ["SKILL.md", "notes/checklist.md"]);
    assert_eq!(
        tree[Path::new("SKILL.md")].bytes,
        b"---\nname: review\n---\nReview {{ code }}.\n"
    );
    let Content::File(file) = &find(&desired, ItemKind::Instructions, "global", "claude").content
    else {
        panic!("instructions are one file");
    };
    assert_eq!(file.bytes, b"Be brief. {{ not_rendered }}\n");
    assert!(!file.executable);
}

#[cfg(unix)]
#[test]
fn skill_files_keep_their_executable_bit_and_file_links_are_copied_as_bytes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let sandbox = sandbox();
    sandbox.source("skills/review/run.sh", "#!/bin/sh\n");
    let script = sandbox.loadout.join("skills/review/run.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.source("shared/style.md", "shared text\n");
    symlink(
        "../../shared/style.md",
        sandbox.loadout.join("skills/review/style.md"),
    )
    .unwrap();

    let context = sandbox.isolated(Os::Linux);
    let (desired, _) = sandbox.ok(&manifest(&agents_line(r#"["codex"]"#)), &context);
    let Content::Tree(tree) = &find(&desired, ItemKind::Skill, "review", "codex").content else {
        panic!("a skill is a tree");
    };
    assert!(tree[Path::new("run.sh")].executable);
    assert!(!tree[Path::new("SKILL.md")].executable);
    assert_eq!(tree[Path::new("style.md")].bytes, b"shared text\n");
    assert!(!tree[Path::new("style.md")].executable);
}

#[test]
fn opencode_and_muse_use_dot_config_on_macos_and_windows_and_muse_has_no_windows_paths() {
    let sandbox = sandbox();
    let list = agents_line(r#"["opencode", "muse", "claude"]"#);

    let macos = sandbox.isolated(Os::Macos);
    let (desired, _) = sandbox.ok(&manifest(&list), &macos);
    let opencode = find(&desired, ItemKind::Skill, "review", "opencode");
    assert_eq!(
        sandbox.relative(path_of(opencode)),
        ".config/opencode/skills/review"
    );
    let muse = find(&desired, ItemKind::Mcp, "files", "muse");
    assert_eq!(
        sandbox.relative(entry_of(muse).0),
        ".config/muse/settings.json"
    );

    let windows = sandbox.isolated(Os::Windows);
    let (desired, unsupported) = sandbox.ok(&manifest(&list), &windows);
    let opencode = find(&desired, ItemKind::Instructions, "global", "opencode");
    assert_eq!(
        sandbox.relative(path_of(opencode)),
        ".config/opencode/AGENTS.md"
    );
    let claude = find(&desired, ItemKind::Mcp, "files", "claude");
    assert_eq!(sandbox.relative(entry_of(claude).0), ".claude.json");
    for (kind, id) in [
        (ItemKind::Skill, "review"),
        (ItemKind::Instructions, "global"),
        (ItemKind::Mcp, "files"),
        (ItemKind::Mcp, "github"),
    ] {
        let action = refused(&unsupported, kind, id, "muse");
        assert_eq!(action.target, Target::None, "muse {kind:?} on windows");
    }
    assert!(reason(refused(&unsupported, ItemKind::Skill, "review", "muse")).contains("Windows"));
}

#[test]
fn agent_dir_overrides_apply_outside_isolation_and_are_ignored_inside_it() {
    let sandbox = sandbox();
    let base = sandbox.home.parent().unwrap().to_path_buf();
    let vars = [
        ("CLAUDE_CONFIG_DIR", base.join("claude-pool")),
        ("CODEX_HOME", base.join("codex-home")),
        ("GROK_HOME", base.join("grok-home")),
        ("PI_CODING_AGENT_DIR", base.join("pi-dir")),
        ("GEMINI_HOME", base.join("gemini-home")),
        ("XDG_CONFIG_HOME", base.join("xdg")),
    ];
    let text = manifest(&agents_line(ALL));

    let ambient = sandbox.ambient(Os::Linux, &vars);
    let (desired, _) = sandbox.ok(&text, &ambient);
    let at = |kind, id, agent| {
        let one = find(&desired, kind, id, agent);
        let path = match &one.target {
            Target::Path { path } => path.clone(),
            Target::Entry { file, .. } => file.clone(),
            other => panic!("{other:?}"),
        };
        path.strip_prefix(&base)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    assert_eq!(
        at(ItemKind::Skill, "review", "claude"),
        "claude-pool/skills/review"
    );
    assert_eq!(
        at(ItemKind::Instructions, "global", "claude"),
        "claude-pool/CLAUDE.md"
    );
    assert_eq!(
        at(ItemKind::Mcp, "files", "claude"),
        "claude-pool/.claude.json"
    );
    assert_eq!(
        at(ItemKind::Skill, "review", "codex"),
        "home/.agents/skills/review"
    );
    assert_eq!(
        at(ItemKind::Instructions, "global", "codex"),
        "codex-home/AGENTS.md"
    );
    assert_eq!(
        at(ItemKind::Mcp, "files", "codex"),
        "codex-home/config.toml"
    );
    assert_eq!(at(ItemKind::Mcp, "files", "grok"), "grok-home/config.toml");
    assert_eq!(at(ItemKind::Skill, "review", "pi"), "pi-dir/skills/review");
    assert_eq!(
        at(ItemKind::Instructions, "global", "antigravity"),
        "gemini-home/AGENTS.md"
    );
    assert_eq!(
        at(ItemKind::Mcp, "files", "antigravity"),
        "gemini-home/config/mcp_config.json"
    );
    assert_eq!(
        at(ItemKind::Skill, "review", "opencode"),
        "xdg/opencode/skills/review"
    );
    assert_eq!(at(ItemKind::Mcp, "files", "muse"), "xdg/muse/settings.json");
    assert_eq!(
        at(ItemKind::Skill, "review", "cursor"),
        "home/.cursor/skills/review"
    );

    let mut isolating = vars.to_vec();
    isolating.push(("LOADOUT_HOME", sandbox.home.clone()));
    let isolated = sandbox.ambient(Os::Linux, &isolating);
    let (desired, _) = sandbox.ok(&text, &isolated);
    let claude = find(&desired, ItemKind::Mcp, "files", "claude");
    assert_eq!(sandbox.relative(entry_of(claude).0), ".claude.json");
    let opencode = find(&desired, ItemKind::Skill, "review", "opencode");
    assert_eq!(
        sandbox.relative(path_of(opencode)),
        ".config/opencode/skills/review"
    );
}

#[test]
fn only_the_effective_agents_of_each_item_get_it() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = format!(
        "[loadout]\nversion = 1\n\n{}\n[skills.review]\nsource = \"skills/review\"\nagents = [\"codex\", \"cursor\"]\n\n[mcp.files]\ncommand = \"npx\"\nagents = [\"pi\"]\n",
        agents_line(r#"["claude", "codex", "pi"]"#)
    );
    let (desired, unsupported) = sandbox.ok(&text, &context);
    let got: Vec<(ItemKind, String)> = desired
        .iter()
        .map(|one| (one.item.kind, one.item.agent.clone().unwrap()))
        .collect();
    assert_eq!(
        got,
        [
            (ItemKind::Skill, "codex".to_owned()),
            (ItemKind::Mcp, "pi".to_owned())
        ]
    );
    assert!(unsupported.is_empty());
}

#[test]
fn opencode_writes_the_jsonc_file_the_user_already_keeps() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&agents_line(r#"["opencode"]"#));

    let (desired, _) = sandbox.ok(&text, &context);
    let (file, format, ..) = entry_of(find(&desired, ItemKind::Mcp, "files", "opencode"));
    assert_eq!(
        (sandbox.relative(file).as_str(), format),
        (".config/opencode/opencode.json", McpFormat::Json)
    );

    sandbox.home_file(".config/opencode/opencode.jsonc", "{\n  // mine\n}\n");
    let (desired, _) = sandbox.ok(&text, &context);
    let (file, format, ..) = entry_of(find(&desired, ItemKind::Mcp, "files", "opencode"));
    assert_eq!(
        (sandbox.relative(file).as_str(), format),
        (".config/opencode/opencode.jsonc", McpFormat::Jsonc)
    );
}

const DEVIN: &str = r#"
[agents.custom.devin]
evidence = "https://docs.example/devin"
t3_acp_agent = "devin"
skills = { linux = "~/.devin/skills" }
instructions = { linux = "~/.devin/AGENTS.md" }

[agents.custom.devin.mcp]
linux = "~/.devin/mcp.jsonc"
format = "jsonc"
map = "/servers/mcp"
shape = "opencode-json"
env_syntax = "{env:VAR}"

[agents.custom.copilot]
evidence = "https://docs.example/copilot"
skills = { macos = "~/.copilot/skills" }

[agents.custom.copilot.mcp]
linux = "~/.copilot/mcp.toml"
format = "toml"
map = "/tools/mcp"
shape = "grok-toml"
env_syntax = "none"
"#;

#[test]
fn declared_custom_agents_get_exactly_their_declared_paths_shapes_and_syntax() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&format!(
        "{}{DEVIN}",
        agents_line(r#"["devin", "copilot"]"#)
    ));
    let (desired, unsupported) = sandbox.ok(&text, &context);

    let skill = find(&desired, ItemKind::Skill, "review", "devin");
    assert_eq!(sandbox.relative(path_of(skill)), ".devin/skills/review");
    let instructions = find(&desired, ItemKind::Instructions, "global", "devin");
    assert_eq!(sandbox.relative(path_of(instructions)), ".devin/AGENTS.md");
    let github = find(&desired, ItemKind::Mcp, "github", "devin");
    let (file, format, pointer, key, shape) = entry_of(github);
    assert_eq!(sandbox.relative(file), ".devin/mcp.jsonc");
    assert_eq!(
        (format, pointer, key, shape),
        (
            McpFormat::Jsonc,
            "/servers/mcp",
            "github",
            McpShape::OpencodeJson
        )
    );
    assert_eq!(
        entry_json(github)["headers"]["Authorization"],
        "Bearer {env:GITHUB_TOKEN}"
    );

    let files = find(&desired, ItemKind::Mcp, "files", "copilot");
    assert_eq!(entry_of(files).4, McpShape::GrokToml);
    let github = refused(&unsupported, ItemKind::Mcp, "github", "copilot");
    assert!(matches!(
        github.target,
        Target::Entry {
            shape: McpShape::GrokToml,
            ..
        }
    ));
    assert!(reason(github).contains("no documented env syntax"));

    let skill = refused(&unsupported, ItemKind::Skill, "review", "copilot");
    assert_eq!(skill.target, Target::None);
    assert!(
        reason(skill).contains("no skills path for linux"),
        "{}",
        reason(skill)
    );
    let instructions = refused(&unsupported, ItemKind::Instructions, "global", "copilot");
    assert_eq!(instructions.target, Target::None);
    assert!(reason(instructions).contains("no instructions table"));

    let macos = sandbox.isolated(Os::Macos);
    let (_, unsupported) = sandbox.ok(&text, &macos);
    let devin = refused(&unsupported, ItemKind::Mcp, "files", "devin");
    assert_eq!(devin.target, Target::None);
    assert!(reason(devin).contains("no mcp path for macos"));
}

#[test]
fn a_destination_inside_the_active_checkout_is_unsupported_and_its_source_is_never_read() {
    let sandbox = sandbox();
    let text = manifest(&format!(
        "{}\n[agents.custom.devin]\nevidence = \"x\"\nskills = {{ linux = \"~/checkout/skills\" }}\n",
        agents_line(r#"["devin", "codex"]"#)
    ));
    let checkout = sandbox.home.join("checkout");
    let moved = sandbox.relocated(checkout);
    let context = moved.isolated(Os::Linux);
    let (desired, unsupported) = moved.ok(&text, &context);
    let devin = refused(&unsupported, ItemKind::Skill, "review", "devin");
    assert!(matches!(devin.target, Target::Path { .. }));
    assert!(
        reason(devin).contains("loadout's own state"),
        "{}",
        reason(devin)
    );
    find(&desired, ItemKind::Skill, "review", "codex");
}

#[cfg(unix)]
#[test]
fn a_skills_folder_aliased_into_an_auth_store_is_unsupported() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    fs::create_dir_all(sandbox.home.join(".ssh")).unwrap();
    fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    symlink(
        sandbox.home.join(".ssh"),
        sandbox.home.join(".cursor/skills"),
    )
    .unwrap();
    let context = sandbox.isolated(Os::Linux);
    let (desired, unsupported) = sandbox.ok(&manifest(&agents_line(r#"["cursor"]"#)), &context);
    let cursor = refused(&unsupported, ItemKind::Skill, "review", "cursor");
    assert!(reason(cursor).contains("auth"), "{}", reason(cursor));
    assert!(matches!(cursor.target, Target::Path { .. }));
    assert!(desired.iter().all(|one| one.item.kind != ItemKind::Skill));
}

#[test]
fn a_user_file_inside_an_agent_destination_is_a_collision() {
    let sandbox = sandbox();
    sandbox.source("files/skill.md", "x\n");
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&format!(
        "{}\n[files.notes]\nsource = \"files/skill.md\"\ntarget = \"~/.claude/skills/review/SKILL.md\"\n",
        agents_line(r#"["claude"]"#)
    ));
    let errors = sandbox.invalid(&text, &context);
    assert!(errors.contains("skills.review for claude"), "{errors}");
    assert!(errors.contains("files.notes"), "{errors}");
}

#[test]
fn two_agents_writing_one_skill_destination_is_a_collision() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&format!(
        "{}\n[agents.custom.devin]\nevidence = \"x\"\nskills = {{ linux = \"~/.agents/skills\" }}\n",
        agents_line(r#"["codex", "devin"]"#)
    ));
    let errors = sandbox.invalid(&text, &context);
    assert!(
        errors.contains("skills.review for devin") && errors.contains("skills.review for codex"),
        "{errors}"
    );
    assert!(errors.contains("writes the same path as"), "{errors}");
}

#[test]
fn destination_case_folds_on_macos_and_windows_but_not_linux() {
    let sandbox = sandbox();
    let text = manifest(&format!(
        "{}\n[agents.custom.devin]\nevidence = \"x\"\nskills = {{ linux = \"~/.Agents/skills\", macos = \"~/.Agents/skills\" }}\n",
        agents_line(r#"["codex", "devin"]"#)
    ));
    sandbox.ok(&text, &sandbox.isolated(Os::Linux));
    let errors = sandbox.invalid(&text, &sandbox.isolated(Os::Macos));
    assert!(errors.contains("skills.review for devin"), "{errors}");
}

#[cfg(unix)]
#[test]
fn a_linked_folder_cannot_hide_a_second_writer() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    fs::create_dir_all(sandbox.home.join(".agents/skills")).unwrap();
    fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    symlink(
        sandbox.home.join(".agents/skills"),
        sandbox.home.join(".cursor/skills"),
    )
    .unwrap();
    let context = sandbox.isolated(Os::Linux);
    let errors = sandbox.invalid(&manifest(&agents_line(r#"["codex", "cursor"]"#)), &context);
    assert!(errors.contains("skills.review for cursor"), "{errors}");
    assert!(errors.contains("skills.review for codex"), "{errors}");
}

#[test]
fn mcp_keys_collide_per_file_and_pi_folds_dash_and_underscore() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let pair = "[mcp.my-docs]\ncommand = \"a\"\n\n[mcp.my_docs]\ncommand = \"b\"\n";
    let base = "[loadout]\nversion = 1\n";

    sandbox.ok(
        &format!("{base}{}{pair}", agents_line(r#"["claude"]"#)),
        &context,
    );
    let errors = sandbox.invalid(
        &format!("{base}{}{pair}", agents_line(r#"["pi"]"#)),
        &context,
    );
    assert!(
        errors.contains("mcp.my_docs for pi") && errors.contains("mcp.my-docs for pi"),
        "{errors}"
    );
    assert!(errors.contains("sets the same MCP key as"), "{errors}");

    let shared = format!(
        "{base}{}[mcp.docs]\ncommand = \"a\"\n\n[agents.custom.devin]\nevidence = \"x\"\n\n[agents.custom.devin.mcp]\nlinux = \"~/.cursor/mcp.json\"\nformat = \"json\"\nmap = \"/mcpServers\"\nshape = \"cursor-json\"\nenv_syntax = \"${{env:VAR}}\"\n",
        agents_line(r#"["cursor", "devin"]"#)
    );
    let errors = sandbox.invalid(&shared, &context);
    assert!(
        errors.contains("mcp.docs for devin") && errors.contains("mcp.docs for cursor"),
        "{errors}"
    );
}

#[test]
fn materialize_reads_nothing_into_the_home_and_writes_nothing() {
    let sandbox = sandbox();
    sandbox.home_file(
        ".claude.json",
        "{\"oauthAccount\":{\"token\":\"sentinel\"}}",
    );
    sandbox.home_file(".codex/auth.json", "sentinel");
    sandbox.home_file(".config/opencode/opencode.jsonc", "{}");
    let before = sandbox.snapshot();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&agents_line(ALL));
    let first = sandbox.ok(&text, &context);
    let second = sandbox.ok(&text, &context);
    assert_eq!(first, second, "materialize is deterministic");
    assert_eq!(sandbox.snapshot(), before, "materialize never writes");
    let rendered = format!("{:?}", first.1);
    assert!(!rendered.contains("sentinel"));
}

#[test]
fn a_skill_changed_after_validation_is_refused_before_anything_is_planned() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&agents_line(r#"["claude"]"#));
    fs::write(sandbox.loadout.join("loadout.toml"), &text).unwrap();
    let (loadout, _) = Loadout::load(&sandbox.loadout).unwrap();
    let facts = Facts {
        name: "box".to_owned(),
        os: Os::Linux,
        distro: None,
        arch: Arch::X86_64,
        home: sandbox.home.clone(),
    };
    let (resolved, _) = resolve::resolve(&loadout, &facts).unwrap();

    fs::create_dir_all(sandbox.loadout.join("skills/review/.git")).unwrap();
    let Err(Error::Invalid(errors)) = agents::materialize(&resolved, &context, &sandbox.loadout)
    else {
        panic!("a skill that gained Git metadata must be refused");
    };
    let errors = format!("{errors:?}");
    assert!(errors.contains("Git metadata"), "{errors}");
    fs::remove_dir(sandbox.loadout.join("skills/review/.git")).unwrap();

    fs::remove_file(sandbox.loadout.join("skills/review/SKILL.md")).unwrap();
    let Err(Error::Invalid(errors)) = agents::materialize(&resolved, &context, &sandbox.loadout)
    else {
        panic!("a skill without SKILL.md must be refused");
    };
    assert!(format!("{errors:?}").contains("SKILL.md"));
}

#[cfg(unix)]
#[test]
fn a_skill_link_that_leaves_the_loadout_or_names_a_folder_is_refused() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&agents_line(r#"["claude"]"#));
    fs::write(sandbox.loadout.join("loadout.toml"), &text).unwrap();
    let (loadout, _) = Loadout::load(&sandbox.loadout).unwrap();
    let facts = Facts {
        name: "box".to_owned(),
        os: Os::Linux,
        distro: None,
        arch: Arch::X86_64,
        home: sandbox.home.clone(),
    };
    let (resolved, _) = resolve::resolve(&loadout, &facts).unwrap();
    let secret = sandbox.home_file(".codex/auth.json", "sentinel");
    let link = sandbox.loadout.join("skills/review/token");

    symlink(&secret, &link).unwrap();
    let Err(Error::Invalid(errors)) = agents::materialize(&resolved, &context, &sandbox.loadout)
    else {
        panic!("a link out of the loadout must be refused");
    };
    let errors = format!("{errors:?}");
    assert!(errors.contains("resolves outside the loadout"), "{errors}");
    assert!(!errors.contains("sentinel"));
    fs::remove_file(&link).unwrap();

    symlink(sandbox.loadout.join("instructions"), &link).unwrap();
    let Err(Error::Invalid(errors)) = agents::materialize(&resolved, &context, &sandbox.loadout)
    else {
        panic!("a link to a folder must be refused");
    };
    assert!(format!("{errors:?}").contains("links to a folder"));
}

fn doctor(home: &Path, state: &Path) -> Json {
    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .args(["doctor", "--json"])
        .env("LOADOUT_HOME", home)
        .env("LOADOUT_STATE_DIR", state)
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor --json: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn check<'a>(report: &'a Json, name: &str) -> Option<&'a Json> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
}

#[test]
fn doctor_reports_every_adapter_and_t3_pickup_without_writing() {
    let sandbox = sandbox();
    let state = sandbox.home.parent().unwrap().join("state");
    let before = sandbox.snapshot();
    let report = doctor(&sandbox.home, &state);
    assert_eq!(sandbox.snapshot(), before, "doctor never writes");
    assert!(!state.exists());

    let overrides = check(&report, "agent overrides").unwrap();
    assert!(
        overrides["detail"]
            .as_str()
            .unwrap()
            .contains("ignored under LOADOUT_HOME")
    );
    for agent in [
        "claude",
        "codex",
        "cursor",
        "grok",
        "muse",
        "pi",
        "opencode",
        "antigravity",
    ] {
        let one = check(&report, &format!("agent {agent}")).unwrap_or_else(|| panic!("{agent}"));
        assert_eq!(one["status"], "info");
    }
    let antigravity = check(&report, "agent antigravity").unwrap()["detail"]
        .as_str()
        .unwrap();
    assert!(antigravity.contains("only part"), "{antigravity}");
    assert!(antigravity.contains("private GEMINI_HOME"), "{antigravity}");
    let cursor = check(&report, "agent cursor").unwrap()["detail"]
        .as_str()
        .unwrap();
    assert!(cursor.contains("instructions unsupported"), "{cursor}");
    assert!(cursor.contains("inferred, not proven"), "{cursor}");
    assert_eq!(
        check(&report, "skill duplicates").unwrap()["status"],
        "pass"
    );
    assert!(check(&report, "codex override").is_none());
}

#[test]
fn doctor_warns_about_double_loading_and_shadowing_files() {
    let sandbox = sandbox();
    let state = sandbox.home.parent().unwrap().join("state");
    sandbox.home_file(".claude/skills/review/SKILL.md", "x");
    sandbox.home_file(".agents/skills/review/SKILL.md", "x");
    sandbox.home_file(".codex/AGENTS.override.md", "x");
    sandbox.home_file(".grok/AGENTS.md", "x");
    sandbox.home_file(".claude/CLAUDE.md", "x");
    sandbox.home_file(".config/opencode/opencode.json", "{}");
    sandbox.home_file(".config/opencode/opencode.jsonc", "{}");
    let report = doctor(&sandbox.home, &state);

    let duplicates = check(&report, "skill duplicates").unwrap();
    assert_eq!(duplicates["status"], "warn");
    let detail = duplicates["detail"].as_str().unwrap().replace('\\', "/");
    let muse = if cfg!(windows) { None } else { Some("muse") };
    for agent in ["cursor", "opencode"].into_iter().chain(muse) {
        assert!(
            detail.contains(&format!("{agent} loads review from")),
            "{detail}"
        );
    }
    assert!(!detail.contains("claude loads"), "{detail}");
    assert_eq!(check(&report, "codex override").unwrap()["status"], "warn");
    assert_eq!(
        check(&report, "grok instructions").unwrap()["status"],
        "warn"
    );
    assert_eq!(check(&report, "opencode config").unwrap()["status"], "warn");
}

#[test]
fn a_checkout_kept_under_t3_worktrees_or_the_state_clone_is_read_normally() {
    let text = manifest(&agents_line(r#"["claude", "codex"]"#));
    let sandbox = sandbox();
    let worktree = sandbox.home.join(".t3").join("worktrees").join("loadout");
    let sandbox = sandbox.relocated(worktree);
    let context = sandbox.isolated(Os::Linux);
    let (desired, unsupported) = sandbox.ok(&text, &context);
    assert_eq!(desired.len(), 2 + 2 + 4, "{unsupported:#?}");

    let sandbox = self::sandbox();
    let context = sandbox.isolated(Os::Linux);
    let clone = context.state.join("loadout");
    let sandbox = sandbox.relocated(clone);
    let (desired, _) = sandbox.ok(&text, &context);
    find(&desired, ItemKind::Skill, "review", "codex");
}

#[test]
fn a_checkout_inside_a_credential_folder_is_refused() {
    let sandbox = sandbox();
    let inside = sandbox.home.join(".ssh").join("loadout");
    let sandbox = sandbox.relocated(inside);
    let context = sandbox.isolated(Os::Linux);
    let errors = sandbox.invalid(&manifest(&agents_line(r#"["claude"]"#)), &context);
    assert!(errors.contains("auth"), "{errors}");
}

#[cfg(unix)]
#[test]
fn a_home_checkout_cannot_copy_an_auth_store_or_private_settings_it_contains() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let base = sandbox.home.parent().unwrap().to_path_buf();
    let home = sandbox.home.clone();
    fs::remove_dir(&home).unwrap();
    let sandbox = sandbox.relocated(home.clone());
    let context = sandbox.ambient(Os::Linux, &[("CLAUDE_CONFIG_DIR", base.join("claude"))]);
    let text = manifest(&agents_line(r#"["claude"]"#));

    let auth = sandbox.home_file(".codex/auth.json", "AUTH-SENTINEL");
    let link = home.join("skills").join("review").join("token");
    symlink(&auth, &link).unwrap();
    let errors = sandbox.invalid(&text, &context);
    assert!(errors.contains("auth"), "{errors}");
    assert!(!errors.contains("AUTH-SENTINEL"), "{errors}");
    fs::remove_file(&link).unwrap();

    let settings = sandbox.home_file(".t3/settings.json", "PROVIDER-SENTINEL");
    symlink(&settings, &link).unwrap();
    let errors = sandbox.invalid(&text, &context);
    assert!(errors.contains("private"), "{errors}");
    assert!(!errors.contains("PROVIDER-SENTINEL"), "{errors}");
    fs::remove_file(&link).unwrap();

    let (desired, _) = sandbox.ok(&text, &context);
    let skill = find(&desired, ItemKind::Skill, "review", "claude");
    assert!(path_of(skill).starts_with(base.join("claude")));
}

#[cfg(unix)]
#[test]
fn git_metadata_named_by_a_gitfile_or_commondir_stays_refused_after_validation() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    sandbox.source(".git", "gitdir: meta/repo\n");
    sandbox.source(
        "meta/repo/config",
        "[remote] url = https://token@example.com\n",
    );
    sandbox.source("meta/repo/commondir", "../common\n");
    sandbox.source("meta/common/HEAD", "ref: refs/heads/main\n");
    sandbox.source("meta/notes.md", "plain\n");
    let context = sandbox.isolated(Os::Linux);
    let text = manifest(&agents_line(r#"["claude"]"#));
    fs::write(sandbox.loadout.join("loadout.toml"), &text).unwrap();
    let (loadout, _) = Loadout::load(&sandbox.loadout)
        .unwrap_or_else(|diagnostics| panic!("fixture must validate: {diagnostics:?}"));
    let facts = Facts {
        name: "box".to_owned(),
        os: Os::Linux,
        distro: None,
        arch: Arch::X86_64,
        home: sandbox.home.clone(),
    };
    let (resolved, _) = resolve::resolve(&loadout, &facts).unwrap();
    let link = sandbox.loadout.join("skills/review/extra");

    for target in [
        "meta/repo/config",
        "meta/repo/commondir",
        "meta/common/HEAD",
    ] {
        symlink(sandbox.loadout.join(target), &link).unwrap();
        let Err(Error::Invalid(errors)) =
            agents::materialize(&resolved, &context, &sandbox.loadout)
        else {
            panic!("a link to {target} must be refused");
        };
        let errors = format!("{errors:?}");
        assert!(errors.contains("Git metadata"), "{target}: {errors}");
        assert!(!errors.contains("token@"), "{errors}");
        fs::remove_file(&link).unwrap();
    }

    symlink(sandbox.loadout.join("meta/notes.md"), &link).unwrap();
    let (desired, _) = agents::materialize(&resolved, &context, &sandbox.loadout).unwrap();
    let Content::Tree(tree) = &find(&desired, ItemKind::Skill, "review", "claude").content else {
        panic!("a skill is a tree");
    };
    assert_eq!(tree[Path::new("extra")].bytes, b"plain\n");
}

#[cfg(unix)]
#[test]
fn git_pointers_aliasing_protected_files_are_refused_before_they_are_read() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let base = sandbox.home.parent().unwrap().to_path_buf();
    let home = sandbox.home.clone();
    fs::remove_dir(&home).unwrap();
    let sandbox = sandbox.relocated(home.clone());
    let context = sandbox.ambient(Os::Linux, &[("CLAUDE_CONFIG_DIR", base.join("claude"))]);
    let resolved = sandbox
        .checked(&manifest(&agents_line(r#"["claude"]"#)), &context)
        .unwrap();
    fs::create_dir_all(home.join("meta/repo")).unwrap();
    fs::create_dir_all(home.join("meta/common")).unwrap();

    let auth = sandbox.home_file(".codex/auth.json", "gitdir: meta/repo\nAUTH-SENTINEL\n");
    symlink(&auth, home.join(".git")).unwrap();
    let errors = refusal(&resolved, &context, &home);
    assert!(
        errors.contains("Git pointer .git") && errors.contains("auth"),
        "{errors}"
    );
    assert!(!errors.contains("SENTINEL"), "{errors}");
    fs::remove_file(home.join(".git")).unwrap();

    sandbox.home_file(".git", "gitdir: meta/repo\n");
    let settings = sandbox.home_file(".t3/settings.json", "../common\nPROVIDER-SENTINEL\n");
    let commondir = home.join("meta/repo/commondir");
    symlink(&settings, &commondir).unwrap();
    let errors = refusal(&resolved, &context, &home);
    assert!(
        errors.contains("Git pointer meta/repo/commondir") && errors.contains("private"),
        "{errors}"
    );
    assert!(!errors.contains("SENTINEL"), "{errors}");
    fs::remove_file(&commondir).unwrap();

    sandbox.home_file("meta/repo/commondir", "../common\n");
    let (desired, _) = agents::materialize(&resolved, &context, &home).unwrap();
    find(&desired, ItemKind::Skill, "review", "claude");
}

#[cfg(unix)]
#[test]
fn a_git_pointer_into_an_override_auth_store_under_a_private_checkout_is_refused() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let checkout = sandbox.home.join(".t3").join("worktrees").join("loadout");
    let sandbox = sandbox.relocated(checkout.clone());
    let context = sandbox.ambient(Os::Linux, &[("CODEX_HOME", checkout.join("codex"))]);
    let resolved = sandbox
        .checked(&manifest(&agents_line(r#"["claude"]"#)), &context)
        .unwrap();
    fs::create_dir_all(checkout.join("meta/repo")).unwrap();
    let auth = sandbox.home_file(
        ".t3/worktrees/loadout/codex/auth.json",
        "gitdir: meta/repo\nAUTH-SENTINEL\n",
    );
    symlink(&auth, checkout.join(".git")).unwrap();
    let errors = refusal(&resolved, &context, &checkout);
    assert!(
        errors.contains("Git pointer .git") && errors.contains("auth"),
        "{errors}"
    );
    assert!(!errors.contains("SENTINEL"), "{errors}");
    fs::remove_file(checkout.join(".git")).unwrap();

    sandbox.home_file(".t3/worktrees/loadout/.git", "gitdir: meta/repo\n");
    let (desired, _) = agents::materialize(&resolved, &context, &checkout).unwrap();
    find(&desired, ItemKind::Skill, "review", "claude");
}

/// Two custom agents that merge MCP servers into one file: `[path, format, map, shape]` each.
fn shared_mcp(devin: [&str; 4], jules: [&str; 4], servers: &str) -> String {
    let agent = |id: &str, [path, format, map, shape]: [&str; 4]| {
        format!(
            "[agents.custom.{id}]\nevidence = \"x\"\n\n[agents.custom.{id}.mcp]\nlinux = \"{path}\"\nformat = \"{format}\"\nmap = \"{map}\"\nshape = \"{shape}\"\nenv_syntax = \"none\"\n\n"
        )
    };
    format!(
        "[loadout]\nversion = 1\n\n[agents]\nenabled = [\"devin\", \"jules\"]\n\n{}{}{servers}",
        agent("devin", devin),
        agent("jules", jules)
    )
}

const ONE_SERVER: &str = "[mcp.docs]\ncommand = \"docs\"\n";
const TWO_SERVERS: &str = "[mcp.docs]\ncommand = \"docs\"\nagents = [\"devin\"]\n\n[mcp.lint]\ncommand = \"lint\"\nagents = [\"jules\"]\n";

fn entries(desired: &[Desired]) -> Vec<(String, McpFormat, String, String)> {
    desired
        .iter()
        .filter_map(|one| match &one.target {
            Target::Entry {
                file,
                format,
                pointer,
                key,
                ..
            } => Some((
                file.file_name().unwrap().to_string_lossy().into_owned(),
                *format,
                pointer.clone(),
                key.clone(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn custom_agents_may_merge_one_literal_file_in_one_format() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    let maps = shared_mcp(
        ["~/shared/mcp.json", "json", "/one", "cursor-json"],
        ["~/shared/mcp.json", "json", "/two", "claude-json"],
        ONE_SERVER,
    );
    let resolved = sandbox.checked(&maps, &context).unwrap();
    let (desired, unsupported) =
        agents::materialize(&resolved, &context, &sandbox.loadout).unwrap();
    assert!(unsupported.is_empty(), "{unsupported:#?}");
    assert_eq!(
        entries(&desired),
        [
            (
                "mcp.json".to_owned(),
                McpFormat::Json,
                "/one".to_owned(),
                "docs".to_owned()
            ),
            (
                "mcp.json".to_owned(),
                McpFormat::Json,
                "/two".to_owned(),
                "docs".to_owned()
            ),
        ]
    );

    let keys = shared_mcp(
        ["~/shared/mcp.json", "json", "/mcpServers", "cursor-json"],
        ["~/shared/mcp.json", "json", "/mcpServers", "cursor-json"],
        TWO_SERVERS,
    );
    let resolved = sandbox.checked(&keys, &context).unwrap();
    let (desired, _) = agents::materialize(&resolved, &context, &sandbox.loadout).unwrap();
    let keys: Vec<String> = entries(&desired).into_iter().map(|entry| entry.3).collect();
    assert_eq!(keys, ["docs", "lint"]);

    let same = shared_mcp(
        ["~/shared/mcp.json", "json", "/mcpServers", "cursor-json"],
        ["~/shared/mcp.json", "json", "/mcpServers", "cursor-json"],
        ONE_SERVER,
    );
    let resolved = sandbox.checked(&same, &context).unwrap();
    let errors = refusal(&resolved, &context, &sandbox.loadout);
    assert!(errors.contains("sets the same MCP key as"), "{errors}");
}

#[test]
fn one_literal_file_declared_in_two_formats_is_refused_by_resolve() {
    let sandbox = sandbox();
    let context = sandbox.isolated(Os::Linux);
    for (format, shape) in [("toml", "grok-toml"), ("jsonc", "opencode-json")] {
        let text = shared_mcp(
            ["~/shared/mcp", "json", "/one", "cursor-json"],
            ["~/shared/mcp", format, "/two", shape],
            ONE_SERVER,
        );
        let errors = sandbox.checked(&text, &context).unwrap_err();
        assert!(
            errors.contains("one file has one format"),
            "{format}: {errors}"
        );
        assert!(
            errors.contains("as json") && errors.contains(&format!("as {format}")),
            "{errors}"
        );
    }
}

#[cfg(unix)]
#[test]
fn aliases_of_one_file_must_agree_on_its_format() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    fs::create_dir_all(sandbox.home.join("shared")).unwrap();
    symlink(sandbox.home.join("shared"), sandbox.home.join("alias")).unwrap();
    let context = sandbox.isolated(Os::Linux);
    for (format, shape) in [("toml", "grok-toml"), ("jsonc", "opencode-json")] {
        let text = shared_mcp(
            ["~/shared/mcp", "json", "/one", "cursor-json"],
            ["~/alias/mcp", format, "/two", shape],
            ONE_SERVER,
        );
        let resolved = sandbox.checked(&text, &context).unwrap();
        let errors = refusal(&resolved, &context, &sandbox.loadout);
        assert!(
            errors.contains("mcp.docs for jules") && errors.contains("mcp.docs for devin"),
            "{errors}"
        );
        assert!(
            errors.contains(&format!("merges into one MCP file as {format}, not json")),
            "{errors}"
        );
    }

    let text = shared_mcp(
        ["~/shared/mcp.json", "json", "/one", "cursor-json"],
        ["~/alias/mcp.json", "json", "/two", "claude-json"],
        ONE_SERVER,
    );
    let resolved = sandbox.checked(&text, &context).unwrap();
    let (desired, _) = agents::materialize(&resolved, &context, &sandbox.loadout).unwrap();
    let maps: Vec<String> = entries(&desired).into_iter().map(|entry| entry.2).collect();
    assert_eq!(maps, ["/one", "/two"]);
}

#[cfg(unix)]
fn validate(home: &Path, checkout: &Path) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .arg("validate")
        .arg(checkout)
        .env("LOADOUT_HOME", home)
        .env("LOADOUT_STATE_DIR", home.parent().unwrap().join("state"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).replace('\\', "/");
    (output.status.code().unwrap(), text)
}

#[cfg(unix)]
#[test]
fn validate_refuses_protected_git_pointers_and_manifests_before_reading_them() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let home = sandbox.home.clone();
    fs::remove_dir(&home).unwrap();
    let sandbox = sandbox.relocated(home.clone());
    fs::write(
        home.join("loadout.toml"),
        manifest(&agents_line(r#"["claude"]"#)),
    )
    .unwrap();
    fs::create_dir_all(home.join("meta/repo")).unwrap();
    fs::create_dir_all(home.join("meta/common")).unwrap();
    let (code, text) = validate(&home, &home);
    assert_eq!(code, 0, "{text}");

    let auth = sandbox.home_file(".codex/auth.json", "gitdir: meta/repo\nAUTH-SENTINEL = [\n");
    symlink(&auth, home.join(".git")).unwrap();
    let (code, text) = validate(&home, &home);
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("Git pointer .git") && text.contains("auth"),
        "{text}"
    );
    assert!(!text.contains("SENTINEL"), "{text}");
    fs::remove_file(home.join(".git")).unwrap();

    sandbox.home_file(".git", "gitdir: meta/repo\n");
    let settings = sandbox.home_file(".t3/settings.json", "../common\nPROVIDER-SENTINEL\n");
    let commondir = home.join("meta/repo/commondir");
    symlink(&settings, &commondir).unwrap();
    let (code, text) = validate(&home, &home);
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("Git pointer meta/repo/commondir") && text.contains("private"),
        "{text}"
    );
    assert!(!text.contains("SENTINEL"), "{text}");
    fs::remove_file(&commondir).unwrap();

    sandbox.home_file("meta/repo/commondir", "../common\n");
    let (code, text) = validate(&home, &home);
    assert_eq!(
        code, 0,
        "an ordinary gitfile and commondir validate: {text}"
    );

    fs::create_dir_all(home.join("os")).unwrap();
    symlink(&auth, home.join("os/linux.toml")).unwrap();
    let (code, text) = validate(&home, &home);
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("os/linux.toml") && text.contains("auth"),
        "{text}"
    );
    assert!(!text.contains("SENTINEL"), "{text}");
}

#[cfg(unix)]
#[test]
fn the_guarded_load_refuses_protected_pointers_and_keeps_legitimate_checkouts() {
    use std::os::unix::fs::symlink;

    let sandbox = sandbox();
    let checkout = sandbox.home.join(".t3").join("worktrees").join("loadout");
    let sandbox = sandbox.relocated(checkout.clone());
    let context = sandbox.ambient(Os::Linux, &[("CODEX_HOME", checkout.join("codex"))]);
    fs::write(
        checkout.join("loadout.toml"),
        manifest(&agents_line(r#"["claude"]"#)),
    )
    .unwrap();
    fs::create_dir_all(checkout.join("meta/repo")).unwrap();
    sandbox.home_file(".t3/worktrees/loadout/.git", "gitdir: meta/repo\n");
    let resolved = sandbox
        .checked(&manifest(&agents_line(r#"["claude"]"#)), &context)
        .unwrap();
    let (desired, _) = agents::materialize(&resolved, &context, &checkout).unwrap();
    find(&desired, ItemKind::Skill, "review", "claude");

    fs::remove_file(checkout.join(".git")).unwrap();
    let auth = sandbox.home_file(
        ".t3/worktrees/loadout/codex/auth.json",
        "gitdir: meta/repo\nAUTH-SENTINEL\n",
    );
    symlink(&auth, checkout.join(".git")).unwrap();
    let Err(errors) = Loadout::load_guarded(&checkout, &context) else {
        panic!("a .git aliasing an override auth store must be refused");
    };
    let errors = lines(&errors);
    assert!(
        errors.contains("Git pointer .git") && errors.contains("auth"),
        "{errors}"
    );
    assert!(!errors.contains("SENTINEL"), "{errors}");
}
