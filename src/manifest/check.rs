use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::schema::{
    AgentsPatch, BuiltinAgent, Condition, CustomAgent, EnvSyntax, List, MIN_INTERVAL_MINUTES,
    McpFormat, McpPatch, OneOrMany, PerOs, Value,
};
use super::{LayerFile, Loadout, Scope, is_id};
use crate::context::{split_base, windows_problem};
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::platform::Os;

const TEMPLATE_SUFFIX: &str = ".tmpl";
const SKILL_FILE: &str = "SKILL.md";

pub(super) fn check(loadout: &Loadout) -> Diagnostics {
    let custom: BTreeSet<String> = loadout
        .layers
        .iter()
        .filter_map(|file| file.layer.agents.as_ref())
        .flat_map(|agents| agents.custom.keys().cloned())
        .collect();
    let mut checker = Checker {
        root: loadout
            .root
            .canonicalize()
            .unwrap_or_else(|_| loadout.root.clone()),
        custom,
        diagnostics: Diagnostics::default(),
        file: None,
    };
    checker.machines(loadout);
    for file in &loadout.layers {
        checker.layer(file);
    }
    checker.diagnostics
}

struct Checker {
    root: PathBuf,
    custom: BTreeSet<String>,
    diagnostics: Diagnostics,
    file: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Never,
    Possible,
    Certain,
}

fn windows_reach(scope: &Scope, when: Option<&Condition>) -> Reach {
    let os = when.and_then(|condition| condition.os.as_ref());
    match (scope, os) {
        (Scope::Os(Os::Windows), _) => Reach::Certain,
        (Scope::Os(_), _) => Reach::Never,
        (_, Some(os)) if !os.contains(&Os::Windows) => Reach::Never,
        (_, Some(os)) if os.values().iter().all(|os| *os == Os::Windows) => Reach::Certain,
        _ => Reach::Possible,
    }
}

#[derive(Clone, Copy)]
enum Source {
    File,
    Skill,
}

impl Checker {
    fn report(&mut self, severity: Severity, key: impl Into<String>, message: impl Into<String>) {
        let key = key.into();
        self.diagnostics.push(Diagnostic {
            severity,
            file: self.file.clone(),
            key: (!key.is_empty()).then_some(key),
            message: message.into(),
        });
    }

    fn error(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.report(Severity::Error, key, message);
    }

    fn warn(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.report(Severity::Warning, key, message);
    }

    fn machines(&mut self, loadout: &Loadout) {
        self.file = Some(PathBuf::from(super::ROOT_FILE));
        let mut used = BTreeSet::new();
        for (name, entry) in &loadout.machines {
            let key = format!("machines.{name}");
            if !is_id(name) {
                self.error(&key, "machine names are lowercase letters, digits, - or _");
            }
            let mut seen = BTreeSet::new();
            for tag in &entry.tags {
                if !seen.insert(tag.as_str()) {
                    self.error(
                        format!("{key}.tags"),
                        format!("tag {tag:?} is listed twice"),
                    );
                } else if !loadout
                    .root
                    .join("tags")
                    .join(format!("{tag}.toml"))
                    .is_file()
                {
                    self.error(
                        format!("{key}.tags"),
                        format!("tag {tag:?} has no layer file tags/{tag}.toml"),
                    );
                }
                used.insert(tag.as_str());
            }
        }

        for file in &loadout.layers {
            self.file = Some(file.file.clone());
            match &file.scope {
                Scope::Tag(tag) if !used.contains(tag.as_str()) => {
                    self.warn("", format!("no machine in [machines] lists tag {tag:?}"));
                }
                Scope::Machine(name) if !loadout.machines.contains_key(name) => self.error(
                    "",
                    format!("machine {name:?} has a layer file but no [machines.{name}] entry in loadout.toml"),
                ),
                _ => {}
            }
        }
    }

    fn layer(&mut self, file: &LayerFile) {
        self.file = Some(file.file.clone());
        let global = file.scope == Scope::Global;
        let layer = &file.layer;

        for name in layer.variables.keys() {
            if !is_variable(name) {
                self.error(
                    format!("variables.{name}"),
                    "variable names are letters, digits, and _, not starting with a digit",
                );
            } else if name == "machine" {
                self.error(
                    "variables.machine",
                    "`machine` is reserved for machine facts in templates",
                );
            }
        }

        if let Some(interval) = layer
            .update
            .as_ref()
            .and_then(|update| update.interval_minutes)
            && interval < MIN_INTERVAL_MINUTES
        {
            self.error(
                "update.interval_minutes",
                format!("must be at least {MIN_INTERVAL_MINUTES}"),
            );
        }

        if let Some(agents) = &layer.agents {
            self.agents(agents, global);
        }

        let mut targets = BTreeMap::new();
        for (id, item) in &layer.files {
            let key = format!("files.{id}");
            let fields = [
                item.source.is_some(),
                item.target.is_some(),
                item.when.is_some(),
            ];
            if !self.item(&key, id, global, item.remove, item.replace, &fields) {
                continue;
            }
            if global || item.replace {
                self.require(&key, "source", item.source.is_some());
                self.require(&key, "target", item.target.is_some());
            }
            if let Some(source) = &item.source {
                self.source(&format!("{key}.source"), source, Source::File);
            }
            if let Some(target) = &item.target {
                let reach = windows_reach(&file.scope, item.when.as_ref());
                self.target(&format!("{key}.target"), target, reach);
                if let Some(other) = targets.insert(target.clone(), id.clone()) {
                    self.error(
                        format!("{key}.target"),
                        format!("files.{other} already writes {target}"),
                    );
                }
            }
            self.condition(&key, item.when.as_ref());
        }

        for (id, item) in &layer.skills {
            let key = format!("skills.{id}");
            let fields = [
                item.source.is_some(),
                item.agents.is_some(),
                item.when.is_some(),
            ];
            if !self.item(&key, id, global, item.remove, item.replace, &fields) {
                continue;
            }
            if global || item.replace {
                self.require(&key, "source", item.source.is_some());
            }
            if let Some(source) = &item.source {
                self.source(&format!("{key}.source"), source, Source::Skill);
            }
            self.agent_list(&format!("{key}.agents"), item.agents.as_ref(), global);
            self.condition(&key, item.when.as_ref());
        }

        for (id, item) in &layer.instructions {
            let key = format!("instructions.{id}");
            let fields = [
                item.source.is_some(),
                item.agents.is_some(),
                item.when.is_some(),
            ];
            if !self.item(&key, id, global, item.remove, item.replace, &fields) {
                continue;
            }
            if global || item.replace {
                self.require(&key, "source", item.source.is_some());
            }
            if let Some(source) = &item.source {
                self.source(&format!("{key}.source"), source, Source::File);
                if source.ends_with(TEMPLATE_SUFFIX) {
                    self.warn(
                        format!("{key}.source"),
                        "only user files are rendered; this .tmpl file is copied as is",
                    );
                }
            }
            self.agent_list(&format!("{key}.agents"), item.agents.as_ref(), global);
            self.condition(&key, item.when.as_ref());
        }

        for (id, item) in &layer.mcp {
            let key = format!("mcp.{id}");
            let fields = [
                item.command.is_some(),
                item.args.is_some(),
                item.env.is_some(),
                item.url.is_some(),
                item.headers.is_some(),
                item.agents.is_some(),
                item.when.is_some(),
            ];
            if !self.item(&key, id, global, item.remove, item.replace, &fields) {
                continue;
            }
            self.mcp(&key, item, global);
        }

        for (id, item) in &layer.programs {
            let key = format!("programs.{id}");
            let packages: Vec<_> = item.packages().collect();
            let fields = [!packages.is_empty(), item.when.is_some()];
            if !self.item(&key, id, global, item.remove, item.replace, &fields) {
                continue;
            }
            if (global || item.replace) && packages.is_empty() {
                self.error(
                    &key,
                    "a program needs at least one manager, such as brew = \"<package>\"",
                );
            }
            for (manager, package) in packages {
                if package.trim().is_empty() {
                    self.error(format!("{key}.{}", manager.id()), "package id is empty");
                }
            }
            self.condition(&key, item.when.as_ref());
        }
    }

    fn item(
        &mut self,
        key: &str,
        id: &str,
        global: bool,
        remove: bool,
        replace: bool,
        fields: &[bool],
    ) -> bool {
        if !is_id(id) {
            self.error(key, "item ids are lowercase letters, digits, - or _");
        }
        if global && (remove || replace) {
            self.error(
                key,
                "the global layer has nothing to remove or replace; declare the item instead",
            );
            return false;
        }
        if remove && (replace || fields.iter().any(|set| *set)) {
            self.error(key, "remove = true stands alone, with no other fields");
            return false;
        }
        !remove
    }

    fn require(&mut self, key: &str, field: &str, present: bool) {
        if !present {
            self.error(key, format!("missing {field}"));
        }
    }

    fn source(&mut self, key: &str, source: &str, kind: Source) {
        let relative = Path::new(source);
        let clean = !source.is_empty()
            && !source.contains('\\')
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !clean {
            self.error(
                key,
                format!("{source:?} must be a relative path inside the loadout, using / and no .."),
            );
            return;
        }
        let path = self.root.join(relative);
        let Ok(real) = path.canonicalize() else {
            self.error(key, format!("{source} does not exist in the loadout"));
            return;
        };
        if !real.starts_with(&self.root) {
            self.error(key, format!("{source} resolves outside the loadout"));
            return;
        }
        match kind {
            Source::File if !real.is_file() => self.error(key, format!("{source} is not a file")),
            Source::Skill if !real.is_dir() => {
                self.error(key, format!("{source} is not a skill folder"));
            }
            Source::Skill => {
                self.skill_tree(key, &real);
                if !real.join(SKILL_FILE).is_file() {
                    self.error(key, format!("{source} has no {SKILL_FILE}"));
                }
            }
            Source::File => {}
        }
    }

    fn skill_tree(&mut self, key: &str, dir: &Path) {
        let root = self.root.clone();
        let shown = |path: &Path| {
            path.strip_prefix(&root)
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.error(key, format!("cannot read {}: {error}", shown(dir)));
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.error(key, format!("cannot read {}: {error}", shown(dir)));
                    continue;
                }
            };
            let path = entry.path();
            let kind = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.file_type(),
                Err(error) => {
                    self.error(key, format!("cannot read {}: {error}", shown(&path)));
                    continue;
                }
            };
            if kind.is_dir() {
                self.skill_tree(key, &path);
            } else if kind.is_symlink() {
                match path.canonicalize() {
                    Err(_) => self.error(key, format!("{} is a broken link", shown(&path))),
                    Ok(real) if !real.starts_with(&self.root) => self.error(
                        key,
                        format!("{} links outside the loadout", shown(&path)),
                    ),
                    Ok(real) if !real.is_file() => self.error(
                        key,
                        format!(
                            "{} links to a folder; skills may link only to files inside the loadout",
                            shown(&path)
                        ),
                    ),
                    Ok(_) => {}
                }
            } else if !kind.is_file() {
                self.error(
                    key,
                    format!("{} is not a regular file or folder", shown(&path)),
                );
            }
        }
    }

    fn target(&mut self, key: &str, target: &str, windows: Reach) {
        let Some((_, rest)) = split_base(target) else {
            self.error(
                key,
                format!("{target:?} must start with ~/ or {{config}}/ and may not contain .., NUL, or empty parts"),
            );
            return;
        };
        let Some(problem) = windows_problem(rest) else {
            return;
        };
        match windows {
            Reach::Certain => self.error(key, problem),
            Reach::Possible => self.warn(
                key,
                format!("{problem}; on Windows this target is refused, so override or remove it in os/windows.toml or add a when condition"),
            ),
            Reach::Never => {}
        }
    }

    fn condition(&mut self, key: &str, condition: Option<&Condition>) {
        let Some(condition) = condition else {
            return;
        };
        for machine in condition.machine.iter().flat_map(OneOrMany::values) {
            if !is_id(machine) {
                self.error(
                    format!("{key}.when.machine"),
                    format!("{machine:?} is not a machine name"),
                );
            }
        }
        for distro in condition.distro.iter().flat_map(OneOrMany::values) {
            if distro.is_empty()
                || distro
                    .chars()
                    .any(|c| c.is_ascii_uppercase() || c.is_whitespace())
            {
                self.error(
                    format!("{key}.when.distro"),
                    format!("{distro:?} must be a lowercase os-release ID, such as \"ubuntu\""),
                );
            }
        }
    }

    fn agent_list(&mut self, key: &str, list: Option<&List<String>>, global: bool) {
        let Some(list) = list else {
            return;
        };
        if global && matches!(list, List::Edit(_)) {
            self.error(
                key,
                "the global layer has no inherited list to edit; assign a list",
            );
        }
        for agent in list.values() {
            if BuiltinAgent::from_id(agent).is_none() && !self.custom.contains(agent) {
                self.error(
                    key,
                    format!("unknown agent {agent:?}; declare it under [agents.custom.{agent}]"),
                );
            }
        }
    }

    fn agents(&mut self, agents: &AgentsPatch, global: bool) {
        self.agent_list("agents.enabled", agents.enabled.as_ref(), global);
        for (id, agent) in &agents.custom {
            self.custom_agent(&format!("agents.custom.{id}"), id, agent);
        }
    }

    fn custom_agent(&mut self, key: &str, id: &str, agent: &CustomAgent) {
        if !is_id(id) {
            self.error(key, "agent ids are lowercase letters, digits, - or _");
        }
        if BuiltinAgent::from_id(id).is_some() {
            self.error(
                key,
                format!("{id} is a built-in agent; enable it in agents.enabled"),
            );
        }
        if agent.evidence.trim().is_empty() {
            self.error(
                format!("{key}.evidence"),
                "say where these paths come from (a docs URL or note)",
            );
        }
        if agent
            .t3_acp_agent
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            self.error(format!("{key}.t3_acp_agent"), "is empty");
        }
        if agent.skills.is_none() && agent.instructions.is_none() && agent.mcp.is_none() {
            self.error(key, "declare at least one of skills, instructions, or mcp");
        }
        if let Some(skills) = &agent.skills {
            self.per_os(&format!("{key}.skills"), skills);
        }
        if let Some(instructions) = &agent.instructions {
            self.per_os(&format!("{key}.instructions"), instructions);
        }
        if let Some(mcp) = &agent.mcp {
            let key = format!("{key}.mcp");
            self.per_os(&key, &mcp.paths());
            if mcp.shape.is_toml() != (mcp.format == McpFormat::Toml) {
                self.error(
                    format!("{key}.format"),
                    format!(
                        "shape {} needs format {}",
                        mcp.shape.id(),
                        if mcp.shape.is_toml() {
                            "toml"
                        } else {
                            "json or jsonc"
                        }
                    ),
                );
            }
            if !is_pointer(&mcp.map) {
                self.error(
                    format!("{key}.map"),
                    "must be a pointer such as \"/mcpServers\" or \"/mcp_servers\"",
                );
            }
            if mcp.env_syntax == EnvSyntax::CodexEnvVars
                && mcp.shape != super::schema::McpShape::CodexToml
            {
                self.error(
                    format!("{key}.env_syntax"),
                    "codex-env-vars works only with shape codex-toml",
                );
            }
        }
    }

    fn per_os(&mut self, key: &str, paths: &PerOs) {
        let mut any = false;
        for (os, path) in paths.paths() {
            any = true;
            let reach = if os == Os::Windows {
                Reach::Certain
            } else {
                Reach::Never
            };
            self.target(&format!("{key}.{os}"), path, reach);
        }
        if !any {
            self.error(key, "name a path for at least one of macos, linux, windows");
        }
    }

    fn mcp(&mut self, key: &str, item: &McpPatch, global: bool) {
        if item.command.is_some() && item.url.is_some() {
            self.error(
                key,
                "a server has either command (stdio) or url (remote), not both",
            );
        }
        if (global || item.replace) && item.command.is_none() && item.url.is_none() {
            self.error(key, "missing command (stdio server) or url (remote server)");
        }
        if item.url.is_some() && (item.args.is_some() || item.env.is_some()) {
            self.error(
                key,
                "args and env belong to stdio servers; a url server takes headers",
            );
        }
        if item.command.is_some() && item.headers.is_some() {
            self.error(
                key,
                "headers belong to url servers; a stdio server takes env",
            );
        }
        if let Some(command) = &item.command
            && command.trim().is_empty()
        {
            self.error(format!("{key}.command"), "is empty");
        }
        if let Some(url) = &item.url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            self.error(format!("{key}.url"), "must start with https:// or http://");
        }
        if global && matches!(item.args, Some(List::Edit(_))) {
            self.error(
                format!("{key}.args"),
                "the global layer has no inherited list to edit; assign a list",
            );
        }
        for (name, value) in item.env.iter().flatten() {
            if !is_variable(name) {
                self.error(
                    format!("{key}.env.{name}"),
                    "is not an environment variable name",
                );
            }
            self.value(&format!("{key}.env.{name}"), name, value);
        }
        for (name, value) in item.headers.iter().flatten() {
            self.value(&format!("{key}.headers.{name}"), name, value);
        }
        self.agent_list(&format!("{key}.agents"), item.agents.as_ref(), global);
        self.condition(key, item.when.as_ref());
    }

    fn value(&mut self, key: &str, name: &str, value: &Value) {
        match value {
            Value::Env(reference) if !is_variable(&reference.env) => {
                self.error(key, format!("{:?} is not an environment variable name", reference.env));
            }
            Value::Text(text) if looks_secret(name) && !text.is_empty() => self.warn(
                key,
                "looks like a credential written into the loadout; use { env = \"VARIABLE\" } instead",
            ),
            Value::Env(_) | Value::Text(_) => {}
        }
    }
}

fn is_variable(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_pointer(pointer: &str) -> bool {
    pointer
        .strip_prefix('/')
        .is_some_and(|rest| rest.split('/').all(|part| !part.is_empty()))
}

fn looks_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "api-key",
    ]
    .iter()
    .any(|word| name.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_need_a_leading_slash_and_no_empty_parts() {
        assert!(is_pointer("/mcpServers"));
        assert!(is_pointer("/amp/mcpServers"));
        assert!(!is_pointer("mcpServers"));
        assert!(!is_pointer("/"));
        assert!(!is_pointer("/a//b"));
    }

    #[test]
    fn credential_names_are_flagged() {
        assert!(looks_secret("Authorization"));
        assert!(looks_secret("GITHUB_TOKEN"));
        assert!(!looks_secret("LOG_LEVEL"));
    }

    #[test]
    fn source_walks_cannot_leave_the_loadout() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "x").unwrap();
        let mut checker = Checker {
            root: dir.path().canonicalize().unwrap(),
            custom: BTreeSet::new(),
            diagnostics: Diagnostics::default(),
            file: None,
        };
        checker.source("k", "a", Source::File);
        assert!(checker.diagnostics.is_empty());
        for bad in ["../a", "/etc/passwd", "", "a\\b", "./a"] {
            checker.source("k", bad, Source::File);
        }
        assert_eq!(checker.diagnostics.count(Severity::Error), 5);
    }
}
