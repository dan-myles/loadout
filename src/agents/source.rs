use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::manifest;
use crate::plan::FileContent;
use crate::reserved;

const SKILL_FILE: &str = "SKILL.md";
const GIT_DIR: &str = ".git";

/// Reads skill and instructions sources from the active checkout, copied as they are and never
/// rendered. Validate confined them already; this runs again because the checkout can change
/// between validate and plan.
pub(super) struct Sources<'a> {
    context: &'a Context,
    root: PathBuf,
    git: Vec<PathBuf>,
}

impl<'a> Sources<'a> {
    pub(super) fn new(context: &'a Context, loadout: &Path) -> Result<Self, String> {
        let root = loadout.canonicalize().map_err(|error| {
            format!("cannot resolve the loadout {}: {error}", loadout.display())
        })?;
        let guard = |reached: &Path, real: &Path| match policy(context, &root, reached, real) {
            Some(message) => Err(format!("Git pointer {message}")),
            None => Ok(()),
        };
        let git = manifest::git_metadata_guarded(&root, &guard)?;
        Ok(Self { context, root, git })
    }

    pub(super) fn file(&self, source: &Path) -> Result<FileContent, String> {
        let real = self.confine(source)?;
        let mut content = self.read(source, &real)?;
        content.executable = false;
        Ok(content)
    }

    pub(super) fn tree(&self, source: &Path) -> Result<BTreeMap<PathBuf, FileContent>, String> {
        let real = self.confine(source)?;
        if !real.is_dir() {
            return Err(format!("{} is not a skill folder", self.shown(source)));
        }
        let mut tree = BTreeMap::new();
        self.walk(&real, Path::new(""), &mut tree)?;
        if !tree.contains_key(Path::new(SKILL_FILE)) {
            return Err(format!("{} has no {SKILL_FILE}", self.shown(source)));
        }
        Ok(tree)
    }

    fn walk(
        &self,
        dir: &Path,
        relative: &Path,
        tree: &mut BTreeMap<PathBuf, FileContent>,
    ) -> Result<(), String> {
        let cannot = |error: io::Error| format!("cannot read {}: {error}", self.shown(dir));
        for entry in fs::read_dir(dir).map_err(cannot)? {
            let entry = entry.map_err(cannot)?;
            let path = entry.path();
            let name = entry.file_name();
            if name.eq_ignore_ascii_case(GIT_DIR) || self.in_git(&path) {
                return Err(format!(
                    "{} is Git metadata, not skill content",
                    self.shown(&path)
                ));
            }
            let kind = fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot read {}: {error}", self.shown(&path)))?
                .file_type();
            let key = relative.join(&name);
            if kind.is_dir() {
                self.walk(&path, &key, tree)?;
            } else if kind.is_symlink() {
                if fs::read_link(&path).is_ok_and(|literal| names_git(&literal)) {
                    return Err(format!(
                        "{} links into Git metadata (.git)",
                        self.shown(&path)
                    ));
                }
                let real = self.confine(&path)?;
                if real.is_dir() {
                    return Err(format!(
                        "{} links to a folder; skills may link only to files inside the loadout",
                        self.shown(&path)
                    ));
                }
                tree.insert(key, self.read(&path, &real)?);
            } else if kind.is_file() {
                tree.insert(key, self.read(&path, &path)?);
            } else {
                return Err(format!(
                    "{} is not a regular file or folder",
                    self.shown(&path)
                ));
            }
        }
        Ok(())
    }

    /// The real path of a source, which must stay inside the loadout and outside its Git
    /// metadata.
    fn confine(&self, path: &Path) -> Result<PathBuf, String> {
        if path.strip_prefix(&self.root).is_ok_and(names_git) {
            return Err(format!(
                "{} is Git metadata (.git), not loadout content",
                self.shown(path)
            ));
        }
        let real = path
            .canonicalize()
            .map_err(|_| format!("{} does not exist or is a broken link", self.shown(path)))?;
        if !real.starts_with(&self.root) {
            return Err(format!("{} resolves outside the loadout", self.shown(path)));
        }
        if self.in_git(&real) {
            return Err(format!(
                "{} resolves into Git metadata (.git)",
                self.shown(path)
            ));
        }
        Ok(real)
    }

    /// A checkout can hold an auth store or another reserved file, such as a checkout at the
    /// home folder, so both the path as reached and the file it lands on pass the source policy
    /// before anything is opened.
    fn read(&self, shown: &Path, real: &Path) -> Result<FileContent, String> {
        if let Some(message) = policy(self.context, &self.root, shown, real) {
            return Err(message);
        }
        let not_a_file = || format!("{} is not a regular file", self.shown(shown));
        if !fs::metadata(real).is_ok_and(|meta| meta.is_file()) {
            return Err(not_a_file());
        }
        let cannot = |error: io::Error| format!("cannot read {}: {error}", self.shown(shown));
        let mut file = File::open(real).map_err(cannot)?;
        let metadata = file.metadata().map_err(cannot)?;
        if !metadata.is_file() {
            return Err(not_a_file());
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(cannot)?;
        Ok(FileContent {
            bytes,
            executable: executable(&metadata),
        })
    }

    fn in_git(&self, real: &Path) -> bool {
        real.strip_prefix(&self.root).is_ok_and(names_git)
            || self.git.iter().any(|metadata| real.starts_with(metadata))
    }

    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

fn policy(context: &Context, root: &Path, reached: &Path, real: &Path) -> Option<String> {
    let conflict = reserved::check_source_file(context, root, reached, real)?;
    let shown = reached.strip_prefix(root).unwrap_or(reached);
    Some(format!("{} {}", shown.display(), conflict.message()))
}

fn names_git(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case(GIT_DIR))
}

#[cfg(unix)]
fn executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &Metadata) -> bool {
    false
}
