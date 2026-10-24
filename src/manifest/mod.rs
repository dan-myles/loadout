mod check;
mod schema;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::platform::Os;
use crate::reserved;

pub use crate::id::is_id;
pub use check::{git_metadata, git_metadata_guarded};
pub use schema::*;

pub const ROOT_FILE: &str = "loadout.toml";
const OS_DIR: &str = "os";
const TAGS_DIR: &str = "tags";
const MACHINES_DIR: &str = "machines";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "layer", content = "name", rename_all = "lowercase")]
pub enum Scope {
    Global,
    Os(Os),
    Tag(String),
    Machine(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerFile {
    pub scope: Scope,
    pub file: PathBuf,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loadout {
    pub root: PathBuf,
    pub machines: BTreeMap<String, MachineEntry>,
    pub layers: Vec<LayerFile>,
}

/// Sees each checkout file a reader is about to open (a manifest, a Git pointer, or a template
/// source), as reached and as its canonical file, and refuses it with a message instead.
pub type Guard<'a> = dyn Fn(&Path, &Path) -> Result<(), String> + 'a;

/// The host's guard for checkout content: `reserved::check_source_file` with `context`, on the file
/// as reached and as its canonical file. `checkout` is the canonical checkout root. Every reader of
/// checkout content on a real host (manifests, Git pointers, template sources) runs it before the
/// open.
pub fn source_guard<'a>(
    context: &'a Context,
    checkout: &'a Path,
) -> impl Fn(&Path, &Path) -> Result<(), String> + 'a {
    move |reached: &Path, real: &Path| match reserved::check_source_file(
        context, checkout, reached, real,
    ) {
        Some(conflict) => {
            let shown = reached.strip_prefix(checkout).unwrap_or(reached);
            Err(format!("{} {}", shown.display(), conflict.message()))
        }
        None => Ok(()),
    }
}

impl Loadout {
    /// Context-free: manifests and Git metadata are confined to the checkout, but the host's
    /// reserved-destination policy is not applied, so a checkout file aliasing an auth store
    /// would be opened. For synthetic and test use only; a command on a real host loads with
    /// [`Loadout::load_guarded`].
    pub fn load(root: &Path) -> Result<(Loadout, Diagnostics), Diagnostics> {
        load_with(root, &|_, _| Ok(()))
    }

    /// For a real host (validate, plan): every manifest and Git pointer file passes
    /// `reserved::check_source_file` with `context` before it is opened. `context` decides only
    /// what may be read; resolution stays machine-independent.
    pub fn load_guarded(
        root: &Path,
        context: &Context,
    ) -> Result<(Loadout, Diagnostics), Diagnostics> {
        let checkout = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        load_with(root, &source_guard(context, &checkout))
    }

    pub fn layer(&self, scope: &Scope) -> Option<&Layer> {
        self.layers
            .iter()
            .find(|file| &file.scope == scope)
            .map(|file| &file.layer)
    }

    pub fn global(&self) -> &Layer {
        &self.layers[0].layer
    }

    pub fn item_count(&self) -> usize {
        self.layers
            .iter()
            .map(|file| {
                let layer = &file.layer;
                layer.files.len()
                    + layer.skills.len()
                    + layer.instructions.len()
                    + layer.mcp.len()
                    + layer.programs.len()
            })
            .sum()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootFile {
    loadout: Option<Header>,
    #[serde(default)]
    machines: BTreeMap<String, MachineEntry>,
    #[serde(default)]
    variables: BTreeMap<String, Variable>,
    update: Option<UpdatePatch>,
    agents: Option<AgentsPatch>,
    #[serde(default)]
    files: BTreeMap<String, FilePatch>,
    #[serde(default)]
    skills: BTreeMap<String, SkillPatch>,
    #[serde(default)]
    instructions: BTreeMap<String, InstructionsPatch>,
    #[serde(default)]
    mcp: BTreeMap<String, McpPatch>,
    #[serde(default)]
    programs: BTreeMap<String, ProgramPatch>,
}

fn read(root: &Path, guard: &Guard<'_>, diagnostics: &mut Diagnostics) -> Option<Loadout> {
    let error = |file: Option<PathBuf>, message: String| Diagnostic {
        severity: Severity::Error,
        file,
        key: None,
        message,
    };
    let real_root = match root.canonicalize() {
        Ok(real) if real.is_dir() => real,
        _ => {
            diagnostics.push(error(
                None,
                format!("{} is not a directory", root.display()),
            ));
            return None;
        }
    };
    let root_file = PathBuf::from(ROOT_FILE);
    let parsed: RootFile = parse(&real_root, &root_file, guard, diagnostics)?;
    match &parsed.loadout {
        None => diagnostics.push(error(
            Some(root_file.clone()),
            format!(
                "missing [loadout] table; start the file with [loadout] and version = {VERSION}"
            ),
        )),
        Some(header) if header.version != VERSION => diagnostics.push(error(
            Some(root_file.clone()),
            format!(
                "loadout.version is {}; this loadout CLI reads version {VERSION}",
                header.version
            ),
        )),
        Some(_) => {}
    }

    let mut layers = vec![LayerFile {
        scope: Scope::Global,
        file: root_file,
        layer: Layer {
            variables: parsed.variables,
            update: parsed.update,
            agents: parsed.agents,
            files: parsed.files,
            skills: parsed.skills,
            instructions: parsed.instructions,
            mcp: parsed.mcp,
            programs: parsed.programs,
        },
    }];

    for (name, file) in layer_files(&real_root, OS_DIR, diagnostics) {
        let Some(os) = Os::ALL.into_iter().find(|os| os.name() == name) else {
            diagnostics.push(error(
                Some(file),
                format!("unknown OS layer {name:?}; OS layers are macos.toml, linux.toml, and windows.toml"),
            ));
            continue;
        };
        if let Some(layer) = parse(&real_root, &file, guard, diagnostics) {
            layers.push(LayerFile {
                scope: Scope::Os(os),
                file,
                layer,
            });
        }
    }
    for (dir, scope) in [
        (TAGS_DIR, Scope::Tag as fn(String) -> Scope),
        (MACHINES_DIR, Scope::Machine),
    ] {
        for (name, file) in layer_files(&real_root, dir, diagnostics) {
            if !is_id(&name) {
                diagnostics.push(error(
                    Some(file),
                    format!("layer name {name:?} must be lowercase letters, digits, - or _"),
                ));
                continue;
            }
            if let Some(layer) = parse(&real_root, &file, guard, diagnostics) {
                layers.push(LayerFile {
                    scope: scope(name),
                    file,
                    layer,
                });
            }
        }
    }

    Some(Loadout {
        root: real_root,
        machines: parsed.machines,
        layers,
    })
}

fn load_with(root: &Path, guard: &Guard<'_>) -> Result<(Loadout, Diagnostics), Diagnostics> {
    let mut diagnostics = Diagnostics::default();
    let loadout = read(root, guard, &mut diagnostics);
    if let Some(loadout) = &loadout {
        diagnostics.extend(check::check(loadout, guard));
    }
    match loadout {
        Some(loadout) if !diagnostics.has_errors() => Ok((loadout, diagnostics)),
        _ => Err(diagnostics),
    }
}

const IGNORED: [&str; 2] = [".DS_Store", ".gitkeep"];

fn layer_files(root: &Path, dir: &str, diagnostics: &mut Diagnostics) -> Vec<(String, PathBuf)> {
    let mut problem = |file: PathBuf, message: String| {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            file: Some(file),
            key: None,
            message,
        });
    };
    let path = root.join(dir);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            problem(PathBuf::from(dir), format!("cannot read: {error}"));
            return Vec::new();
        }
        Ok(_) => {}
    }
    match path.canonicalize() {
        Ok(real) if !real.starts_with(root) => {
            problem(
                PathBuf::from(dir),
                "resolves outside the loadout".to_owned(),
            );
            return Vec::new();
        }
        Ok(real) if !real.is_dir() => {
            problem(
                PathBuf::from(dir),
                "must be a folder of <name>.toml layer files".to_owned(),
            );
            return Vec::new();
        }
        Ok(_) => {}
        Err(error) => {
            problem(PathBuf::from(dir), format!("cannot read: {error}"));
            return Vec::new();
        }
    }
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            problem(PathBuf::from(dir), format!("cannot read: {error}"));
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                problem(PathBuf::from(dir), format!("cannot read an entry: {error}"));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let relative = Path::new(dir).join(&name);
        match name.strip_suffix(".toml") {
            Some(stem) if entry.path().is_file() => files.push((stem.to_owned(), relative)),
            _ => problem(
                relative,
                format!("{dir}/ holds only <name>.toml layer files"),
            ),
        }
    }
    files.sort();
    files
}

fn parse<T: for<'de> Deserialize<'de>>(
    root: &Path,
    file: &Path,
    guard: &Guard<'_>,
    diagnostics: &mut Diagnostics,
) -> Option<T> {
    let mut problem = |message: String| {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            file: Some(file.to_path_buf()),
            key: None,
            message,
        });
    };
    let real = match root.join(file).canonicalize() {
        Ok(real) if real.starts_with(root) => real,
        Ok(_) => {
            problem("resolves outside the loadout".to_owned());
            return None;
        }
        Err(error) => {
            problem(format!("cannot read: {error}"));
            return None;
        }
    };
    if !fs::metadata(&real).is_ok_and(|metadata| metadata.is_file()) {
        problem("must be a regular file".to_owned());
        return None;
    }
    if let Err(message) = guard(&root.join(file), &real) {
        problem(message);
        return None;
    }
    let text = match fs::read_to_string(real) {
        Ok(text) => text,
        Err(error) => {
            problem(format!("cannot read: {error}"));
            return None;
        }
    };
    match toml::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            problem(error.to_string().trim_end().to_owned());
            None
        }
    }
}
