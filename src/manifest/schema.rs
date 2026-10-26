use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::machine::Facts;
use crate::platform::{Arch, Os};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineEntry {
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    #[serde(default)]
    pub variables: BTreeMap<String, Variable>,
    pub update: Option<UpdatePatch>,
    pub agents: Option<AgentsPatch>,
    #[serde(default)]
    pub files: BTreeMap<String, FilePatch>,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillPatch>,
    #[serde(default)]
    pub instructions: BTreeMap<String, InstructionsPatch>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpPatch>,
    #[serde(default)]
    pub programs: BTreeMap<String, ProgramPatch>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Variable {
    Boolean(bool),
    Integer(i64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: PartialEq> OneOrMany<T> {
    pub fn contains(&self, value: &T) -> bool {
        match self {
            OneOrMany::One(one) => one == value,
            OneOrMany::Many(many) => many.contains(value),
        }
    }

    pub fn values(&self) -> &[T] {
        match self {
            OneOrMany::One(one) => std::slice::from_ref(one),
            OneOrMany::Many(many) => many,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum List<T> {
    Assign(Vec<T>),
    Edit(ListEdit<T>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListEdit<T> {
    #[serde(default = "Vec::new")]
    pub add: Vec<T>,
    #[serde(default = "Vec::new")]
    pub remove: Vec<T>,
}

impl<T> List<T> {
    pub fn values(&self) -> impl Iterator<Item = &T> {
        match self {
            List::Assign(values) => values.iter().chain([].iter()),
            List::Edit(edit) => edit.add.iter().chain(edit.remove.iter()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub os: Option<OneOrMany<Os>>,
    pub distro: Option<OneOrMany<String>>,
    pub arch: Option<OneOrMany<Arch>>,
    pub machine: Option<OneOrMany<String>>,
}

impl Condition {
    pub fn matches(&self, facts: &Facts) -> bool {
        self.os.as_ref().is_none_or(|os| os.contains(&facts.os))
            && self
                .arch
                .as_ref()
                .is_none_or(|arch| arch.contains(&facts.arch))
            && self
                .machine
                .as_ref()
                .is_none_or(|machine| machine.contains(&facts.name))
            && self.distro.as_ref().is_none_or(|distro| {
                facts
                    .distro
                    .as_ref()
                    .is_some_and(|actual| distro.contains(actual))
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePatch {
    pub auto: Option<bool>,
    pub interval_minutes: Option<u32>,
    /// Legacy: once turned the CLI self-update after each update on or off. Accepted so
    /// existing loadouts stay valid, but it does nothing and validation warns about it.
    pub cli: Option<bool>,
}

pub const MIN_INTERVAL_MINUTES: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BuiltinAgent {
    Claude,
    Codex,
    Cursor,
    Grok,
    Muse,
    Pi,
    Opencode,
    Antigravity,
}

impl BuiltinAgent {
    pub const ALL: [BuiltinAgent; 8] = [
        BuiltinAgent::Claude,
        BuiltinAgent::Codex,
        BuiltinAgent::Cursor,
        BuiltinAgent::Grok,
        BuiltinAgent::Muse,
        BuiltinAgent::Pi,
        BuiltinAgent::Opencode,
        BuiltinAgent::Antigravity,
    ];

    pub fn id(self) -> &'static str {
        match self {
            BuiltinAgent::Claude => "claude",
            BuiltinAgent::Codex => "codex",
            BuiltinAgent::Cursor => "cursor",
            BuiltinAgent::Grok => "grok",
            BuiltinAgent::Muse => "muse",
            BuiltinAgent::Pi => "pi",
            BuiltinAgent::Opencode => "opencode",
            BuiltinAgent::Antigravity => "antigravity",
        }
    }

    pub fn t3_driver(self) -> &'static str {
        match self {
            BuiltinAgent::Claude => "claudeAgent",
            other => other.id(),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.id() == id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsPatch {
    pub enabled: Option<List<String>>,
    #[serde(default)]
    pub custom: BTreeMap<String, CustomAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAgent {
    pub evidence: String,
    pub t3_acp_agent: Option<String>,
    pub skills: Option<PerOs>,
    pub instructions: Option<PerOs>,
    pub mcp: Option<CustomMcp>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PerOs {
    pub macos: Option<String>,
    pub linux: Option<String>,
    pub windows: Option<String>,
}

impl PerOs {
    pub fn get(&self, os: Os) -> Option<&str> {
        match os {
            Os::Macos => self.macos.as_deref(),
            Os::Linux => self.linux.as_deref(),
            Os::Windows => self.windows.as_deref(),
        }
    }

    pub fn paths(&self) -> impl Iterator<Item = (Os, &str)> {
        Os::ALL
            .into_iter()
            .filter_map(|os| self.get(os).map(|path| (os, path)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomMcp {
    pub macos: Option<String>,
    pub linux: Option<String>,
    pub windows: Option<String>,
    pub format: McpFormat,
    pub map: String,
    pub shape: McpShape,
    pub env_syntax: EnvSyntax,
}

impl CustomMcp {
    pub fn paths(&self) -> PerOs {
        PerOs {
            macos: self.macos.clone(),
            linux: self.linux.clone(),
            windows: self.windows.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpFormat {
    Json,
    Jsonc,
    Toml,
}

impl McpFormat {
    pub fn id(self) -> &'static str {
        match self {
            McpFormat::Json => "json",
            McpFormat::Jsonc => "jsonc",
            McpFormat::Toml => "toml",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpShape {
    ClaudeJson,
    CodexToml,
    CursorJson,
    GrokToml,
    OpencodeJson,
    PiJson,
    MuseJson,
    AntigravityJson,
}

impl McpShape {
    pub fn id(self) -> &'static str {
        match self {
            McpShape::ClaudeJson => "claude-json",
            McpShape::CodexToml => "codex-toml",
            McpShape::CursorJson => "cursor-json",
            McpShape::GrokToml => "grok-toml",
            McpShape::OpencodeJson => "opencode-json",
            McpShape::PiJson => "pi-json",
            McpShape::MuseJson => "muse-json",
            McpShape::AntigravityJson => "antigravity-json",
        }
    }

    pub fn is_toml(self) -> bool {
        matches!(self, McpShape::CodexToml | McpShape::GrokToml)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum EnvSyntax {
    #[serde(rename = "${VAR}")]
    DollarBrace,
    #[serde(rename = "${env:VAR}")]
    DollarBraceEnv,
    #[serde(rename = "{env:VAR}")]
    BraceEnv,
    #[serde(rename = "codex-env-vars")]
    CodexEnvVars,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilePatch {
    pub source: Option<String>,
    pub target: Option<String>,
    pub when: Option<Condition>,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPatch {
    pub source: Option<String>,
    pub agents: Option<List<String>>,
    pub when: Option<Condition>,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionsPatch {
    pub source: Option<String>,
    pub agents: Option<List<String>>,
    pub when: Option<Condition>,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    Env(EnvRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvRef {
    pub env: String,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpPatch {
    pub command: Option<String>,
    pub args: Option<List<String>>,
    pub env: Option<BTreeMap<String, Value>>,
    pub url: Option<String>,
    pub headers: Option<BTreeMap<String, Value>>,
    pub agents: Option<List<String>>,
    pub when: Option<Condition>,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Brew,
    Cask,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Flatpak,
    Snap,
    Winget,
    Scoop,
    Choco,
    Npm,
    Pnpm,
    Bun,
    Cargo,
    Uv,
    Pipx,
    Go,
}

impl Manager {
    pub const ALL: [Manager; 18] = [
        Manager::Brew,
        Manager::Cask,
        Manager::Apt,
        Manager::Dnf,
        Manager::Pacman,
        Manager::Zypper,
        Manager::Flatpak,
        Manager::Snap,
        Manager::Winget,
        Manager::Scoop,
        Manager::Choco,
        Manager::Npm,
        Manager::Pnpm,
        Manager::Bun,
        Manager::Cargo,
        Manager::Uv,
        Manager::Pipx,
        Manager::Go,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::Cask => "cask",
            Manager::Apt => "apt",
            Manager::Dnf => "dnf",
            Manager::Pacman => "pacman",
            Manager::Zypper => "zypper",
            Manager::Flatpak => "flatpak",
            Manager::Snap => "snap",
            Manager::Winget => "winget",
            Manager::Scoop => "scoop",
            Manager::Choco => "choco",
            Manager::Npm => "npm",
            Manager::Pnpm => "pnpm",
            Manager::Bun => "bun",
            Manager::Cargo => "cargo",
            Manager::Uv => "uv",
            Manager::Pipx => "pipx",
            Manager::Go => "go",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Manager::Brew | Manager::Cask => "brew",
            Manager::Apt => "apt-get",
            other => other.id(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramPatch {
    pub brew: Option<String>,
    pub cask: Option<String>,
    pub apt: Option<String>,
    pub dnf: Option<String>,
    pub pacman: Option<String>,
    pub zypper: Option<String>,
    pub flatpak: Option<String>,
    pub snap: Option<String>,
    pub winget: Option<String>,
    pub scoop: Option<String>,
    pub choco: Option<String>,
    pub npm: Option<String>,
    pub pnpm: Option<String>,
    pub bun: Option<String>,
    pub cargo: Option<String>,
    pub uv: Option<String>,
    pub pipx: Option<String>,
    pub go: Option<String>,
    pub when: Option<Condition>,
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub replace: bool,
}

impl ProgramPatch {
    pub fn package(&self, manager: Manager) -> Option<&str> {
        match manager {
            Manager::Brew => self.brew.as_deref(),
            Manager::Cask => self.cask.as_deref(),
            Manager::Apt => self.apt.as_deref(),
            Manager::Dnf => self.dnf.as_deref(),
            Manager::Pacman => self.pacman.as_deref(),
            Manager::Zypper => self.zypper.as_deref(),
            Manager::Flatpak => self.flatpak.as_deref(),
            Manager::Snap => self.snap.as_deref(),
            Manager::Winget => self.winget.as_deref(),
            Manager::Scoop => self.scoop.as_deref(),
            Manager::Choco => self.choco.as_deref(),
            Manager::Npm => self.npm.as_deref(),
            Manager::Pnpm => self.pnpm.as_deref(),
            Manager::Bun => self.bun.as_deref(),
            Manager::Cargo => self.cargo.as_deref(),
            Manager::Uv => self.uv.as_deref(),
            Manager::Pipx => self.pipx.as_deref(),
            Manager::Go => self.go.as_deref(),
        }
    }

    pub fn packages(&self) -> impl Iterator<Item = (Manager, &str)> {
        Manager::ALL
            .into_iter()
            .filter_map(|manager| self.package(manager).map(|package| (manager, package)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(os: Os, distro: Option<&str>) -> Facts {
        Facts {
            name: "box".to_owned(),
            os,
            distro: distro.map(str::to_owned),
            arch: Arch::X86_64,
            home: "/h".into(),
        }
    }

    #[test]
    fn every_manager_is_a_program_field() {
        for manager in Manager::ALL {
            let text = format!("{} = \"pkg\"", manager.id());
            let patch: ProgramPatch = toml::from_str(&text).unwrap();
            assert_eq!(patch.packages().collect::<Vec<_>>(), [(manager, "pkg")]);
        }
    }

    #[test]
    fn every_builtin_agent_round_trips_its_id() {
        for agent in BuiltinAgent::ALL {
            assert_eq!(BuiltinAgent::from_id(agent.id()), Some(agent));
        }
        assert_eq!(BuiltinAgent::Claude.t3_driver(), "claudeAgent");
    }

    #[test]
    fn list_takes_assignment_or_edit_and_rejects_other_keys() {
        let assign: McpPatch = toml::from_str("args = [\"a\"]").unwrap();
        assert_eq!(assign.args, Some(List::Assign(vec!["a".to_owned()])));
        let edit: McpPatch = toml::from_str("args = { add = [\"b\"] }").unwrap();
        assert_eq!(
            edit.args,
            Some(List::Edit(ListEdit {
                add: vec!["b".to_owned()],
                remove: vec![]
            }))
        );
        assert!(toml::from_str::<McpPatch>("args = { append = [\"b\"] }").is_err());
    }

    #[test]
    fn value_is_text_or_env_reference() {
        let patch: McpPatch =
            toml::from_str("env = { A = \"x\", B = { env = \"TOKEN\", prefix = \"Bearer \" } }")
                .unwrap();
        let env = patch.env.unwrap();
        assert_eq!(env["A"], Value::Text("x".to_owned()));
        assert_eq!(
            env["B"],
            Value::Env(EnvRef {
                env: "TOKEN".to_owned(),
                prefix: Some("Bearer ".to_owned())
            })
        );
        assert!(toml::from_str::<McpPatch>("env = { B = { secret = \"x\" } }").is_err());
    }

    #[test]
    fn condition_requires_every_key_to_match() {
        let condition: Condition =
            toml::from_str("os = [\"linux\", \"macos\"]\ndistro = \"ubuntu\"").unwrap();
        assert!(condition.matches(&facts(Os::Linux, Some("ubuntu"))));
        assert!(!condition.matches(&facts(Os::Linux, Some("fedora"))));
        assert!(!condition.matches(&facts(Os::Macos, None)));
        assert!(!condition.matches(&facts(Os::Windows, None)));
        assert!(Condition::default().matches(&facts(Os::Windows, None)));
    }

    #[test]
    fn env_syntax_names_are_the_literal_patterns() {
        let mcp: CustomMcp = toml::from_str(
            "linux = \"~/.x/mcp.json\"\nformat = \"jsonc\"\nmap = \"/mcp\"\nshape = \"opencode-json\"\nenv_syntax = \"{env:VAR}\"",
        )
        .unwrap();
        assert_eq!(mcp.env_syntax, EnvSyntax::BraceEnv);
        assert_eq!(mcp.shape, McpShape::OpencodeJson);
    }
}
