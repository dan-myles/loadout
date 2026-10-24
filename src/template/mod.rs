mod builtins;
mod strict;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Once;

use minijinja::machinery::{self, CodeGenerator};
use minijinja::syntax::SyntaxConfig;
use minijinja::{AutoEscape, Environment, UndefinedBehavior, Value};

use crate::context::Context;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{self, Guard, Loadout, Variable};

pub const SUFFIX: &str = ".tmpl";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{}: {problem}", .file.display(), Line(*.line))]
pub struct Error {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub problem: Problem,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("variable `{0}` is not defined for this machine")]
    Missing(String),
    #[error("{0}")]
    Render(String),
    #[error("the home path {0:?} is not UTF-8, so machine.home has no text value")]
    Home(PathBuf),
}

struct Line(Option<usize>);

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(line) => write!(f, ":{line}"),
            None => Ok(()),
        }
    }
}

pub fn check(file: &Path, source: &str) -> Result<(), Error> {
    let name = file.display().to_string();
    machinery::parse(source, &name, syntax_config())
        .map(drop)
        .map_err(|error| syntax(file, &error))
}

pub fn render(
    file: &Path,
    source: &str,
    variables: &BTreeMap<String, Variable>,
    facts: &Facts,
) -> Result<String, Error> {
    let context = context(variables, facts).map_err(|problem| Error {
        file: file.to_path_buf(),
        line: None,
        problem,
    })?;
    let name = file.display().to_string();
    let parsed =
        machinery::parse(source, &name, syntax_config()).map_err(|error| syntax(file, &error))?;
    let mut generator = CodeGenerator::new(&name, source);
    generator.compile_stmt(&strict::rewrite(source, &parsed));
    let (instructions, blocks) = generator.finish();
    let environment = environment();
    let mut output = String::new();
    let failure = |line, problem| Error {
        file: file.to_path_buf(),
        line,
        problem,
    };
    // A builtin that panics must still end as this file's template error, never as a crash or a
    // panic report on stderr. Known panics are fixed at their builtin; this catches the rest.
    quiet_panics();
    RENDERING.set(true);
    let evaluated = panic::catch_unwind(AssertUnwindSafe(|| {
        machinery::eval(
            &environment,
            &instructions,
            context,
            &blocks,
            &mut machinery::make_string_output(&mut output),
            AutoEscape::None,
        )
        .map(drop)
    }));
    RENDERING.set(false);
    match evaluated {
        Ok(Ok(())) => Ok(output),
        Ok(Err(error)) => {
            let (line, problem) = runtime(&error);
            Err(failure(line, problem))
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown failure".to_owned());
            Err(failure(
                None,
                Problem::Render(format!("the template engine failed: {message}")),
            ))
        }
    }
}

/// Syntax-checks every `.tmpl` source a user file names, in any layer. Context-free: each source is
/// confined to the checkout, but the host's reserved-destination policy is not applied, so a
/// template aliasing an auth store would be opened. For synthetic and test use only; validate on a
/// real host calls [`check_all_guarded`].
pub fn check_all(loadout: &Loadout) -> Diagnostics {
    check_all_with(loadout, &|_, _| Ok(()))
}

/// [`check_all`] for a real host: each source also passes `manifest::source_guard` with `context`,
/// as reached and as its canonical file, before it is opened. Call it only on a loadout that
/// `Loadout::load_guarded` accepted with the same `context`.
pub fn check_all_guarded(loadout: &Loadout, context: &Context) -> Diagnostics {
    check_all_with(loadout, &manifest::source_guard(context, &loadout.root))
}

fn check_all_with(loadout: &Loadout, guard: &Guard<'_>) -> Diagnostics {
    let sources: BTreeSet<&str> = loadout
        .layers
        .iter()
        .flat_map(|file| file.layer.files.values())
        .filter_map(|item| item.source.as_deref())
        .filter(|source| source.ends_with(SUFFIX))
        .collect();
    let mut diagnostics = Diagnostics::default();
    for source in sources {
        let file = Path::new(source);
        let problem = match read(&loadout.root, file, guard) {
            Ok(text) => check(file, &text).err().map(|error| match error.line {
                Some(line) => format!("line {line}: {}", error.problem),
                None => error.problem.to_string(),
            }),
            Err(message) => Some(message),
        };
        if let Some(message) = problem {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                file: Some(file.to_path_buf()),
                key: None,
                message,
            });
        }
    }
    diagnostics
}

fn read(root: &Path, file: &Path, guard: &Guard<'_>) -> Result<String, String> {
    let reached = root.join(file);
    let real = reached
        .canonicalize()
        .map_err(|error| format!("cannot read: {error}"))?;
    if !real.starts_with(root) {
        return Err("resolves outside the loadout".to_owned());
    }
    if !fs::metadata(&real).is_ok_and(|metadata| metadata.is_file()) {
        return Err("must be a regular file".to_owned());
    }
    guard(&reached, &real)?;
    let bytes = fs::read(&real).map_err(|error| format!("cannot read: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "a template must be UTF-8 text".to_owned())
}

fn syntax_config() -> SyntaxConfig {
    SyntaxConfig::builder()
        .keep_trailing_newline(true)
        .build()
        .expect("the default delimiters are a valid syntax")
}

fn environment<'source>() -> Environment<'source> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment.set_auto_escape_callback(|_| AutoEscape::None);
    environment.add_filter(strict::GUARD, strict::guard);
    builtins::install(&mut environment);
    // Debug mode is what makes an undefined error name its variable; its debug() global would
    // print the whole context, so it goes.
    environment.set_debug(true);
    environment.remove_global("debug");
    environment.set_formatter(|output, state, value| {
        if value.is_none() {
            return Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                "cannot print none; machine.distro is none on a machine without a distro, so \
                 give a fallback such as `machine.distro or \"unknown\"`",
            ));
        }
        minijinja::escape_formatter(output, state, value)
    });
    environment
}

fn context(variables: &BTreeMap<String, Variable>, facts: &Facts) -> Result<Value, Problem> {
    let home = facts
        .home
        .to_str()
        .ok_or_else(|| Problem::Home(facts.home.clone()))?;
    let machine = Value::from(BTreeMap::from([
        ("name", Value::from(facts.name.as_str())),
        ("os", Value::from(facts.os.name())),
        ("distro", Value::from(facts.distro.as_deref())),
        ("arch", Value::from(facts.arch.name())),
        ("home", Value::from(home)),
    ]));
    let mut context: BTreeMap<&str, Value> = variables
        .iter()
        .map(|(name, variable)| {
            let value = match variable {
                Variable::Boolean(value) => Value::from(*value),
                Variable::Integer(value) => Value::from(*value),
                Variable::Text(value) => Value::from(value.as_str()),
            };
            (name.as_str(), value)
        })
        .collect();
    context.insert("machine", machine);
    Ok(Value::from(context))
}

fn syntax(file: &Path, error: &minijinja::Error) -> Error {
    let detail = error.detail().unwrap_or("invalid template").to_owned();
    let message = match detail.strip_prefix("unknown statement ") {
        Some("include" | "import" | "from" | "extends" | "block") => format!(
            "{detail}; a template renders on its own and cannot include, import, or extend files"
        ),
        _ => detail,
    };
    Error {
        file: file.to_path_buf(),
        line: error.line(),
        problem: Problem::Syntax(message),
    }
}

fn runtime(error: &minijinja::Error) -> (Option<usize>, Problem) {
    if let Some(undefined) = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<strict::Undefined>())
    {
        let problem = if undefined.lookup {
            Problem::Missing(undefined.expression.clone())
        } else {
            Problem::Render(error.detail().unwrap_or_default().to_owned())
        };
        return (undefined.line.or(error.line()), problem);
    }
    let detail = error.detail().unwrap_or_default();
    let problem = match undefined_name(detail) {
        Some(name) => Problem::Missing(name.to_owned()),
        None if detail.is_empty() => Problem::Render(error.kind().to_string()),
        None => Problem::Render(detail.to_owned()),
    };
    (error.line(), problem)
}

thread_local! {
    static RENDERING: Cell<bool> = const { Cell::new(false) };
}

fn quiet_panics() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !RENDERING.get() {
                previous(info);
            }
        }));
    });
}

// minijinja names an undefined value only inside its detail text, as `name` is undefined.
fn undefined_name(detail: &str) -> Option<&str> {
    let end = detail.find("` is undefined")?;
    let start = detail[..end].rfind('`')? + 1;
    Some(&detail[start..end])
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const ROOT: &str = "[loadout]\nversion = 1\n\n\
        [files.a]\nsource = \"files/a.tmpl\"\ntarget = \"~/a\"\n\n\
        [files.b]\nsource = \"files/b.tmpl\"\ntarget = \"~/b\"\n\n\
        [files.again]\nsource = \"files/a.tmpl\"\ntarget = \"~/again\"\n\n\
        [files.plain]\nsource = \"files/plain\"\ntarget = \"~/plain\"\n";

    fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    fn loadout(files: &[(&str, &str)]) -> (tempfile::TempDir, Loadout) {
        let dir = write(files);
        let (loadout, _) = Loadout::load(dir.path()).unwrap();
        (dir, loadout)
    }

    #[test]
    fn the_guard_sees_each_template_source_once_before_it_is_read() {
        let (_dir, loadout) = loadout(&[
            ("loadout.toml", ROOT),
            ("files/a.tmpl", "{{ x }}\n"),
            ("files/b.tmpl", "{% if %}\n"),
            ("files/plain", "{% if %}\n"),
        ]);
        let seen = RefCell::new(Vec::new());
        let guarded = check_all_with(&loadout, &|reached, real| {
            seen.borrow_mut()
                .push((reached.to_path_buf(), real.to_path_buf()));
            Ok(())
        });
        let a = loadout.root.join("files/a.tmpl");
        let b = loadout.root.join("files/b.tmpl");
        assert_eq!(seen.into_inner(), [(a.clone(), a), (b.clone(), b)]);
        assert_eq!(guarded, check_all(&loadout));
        assert_eq!(guarded.count(Severity::Error), 1, "{guarded:?}");
    }

    #[test]
    fn a_refused_template_is_never_read() {
        let (_dir, loadout) = loadout(&[
            ("loadout.toml", ROOT),
            ("files/a.tmpl", "{% if %}\n"),
            ("files/b.tmpl", "{{ x }}\n"),
            ("files/plain", "plain\n"),
        ]);
        let diagnostics = check_all_with(&loadout, &|reached, _| {
            if reached.ends_with("a.tmpl") {
                Err("files/a.tmpl refused".to_owned())
            } else {
                Ok(())
            }
        });
        let refused: Vec<&Diagnostic> = diagnostics.iter().collect();
        assert_eq!(refused.len(), 1, "{diagnostics:?}");
        assert_eq!(refused[0].file.as_deref(), Some(Path::new("files/a.tmpl")));
        assert_eq!(refused[0].message, "files/a.tmpl refused");
    }

    #[cfg(unix)]
    #[test]
    fn the_guard_sees_an_alias_as_reached_and_as_its_canonical_file() {
        let dir = write(&[
            ("loadout.toml", ROOT),
            ("files/real.tmpl", "{{ x }}\n"),
            ("files/b.tmpl", "{{ x }}\n"),
            ("files/plain", "plain\n"),
        ]);
        std::os::unix::fs::symlink("real.tmpl", dir.path().join("files/a.tmpl")).unwrap();
        let (loadout, _) = Loadout::load(dir.path()).unwrap();
        let seen = RefCell::new(Vec::new());
        check_all_with(&loadout, &|reached, real| {
            seen.borrow_mut()
                .push((reached.to_path_buf(), real.to_path_buf()));
            Ok(())
        });
        let alias = loadout.root.join("files/a.tmpl");
        let real = loadout.root.join("files/real.tmpl");
        assert_eq!(seen.into_inner()[0], (alias, real));
    }
}
