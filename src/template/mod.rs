use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use minijinja::syntax::SyntaxConfig;
use minijinja::{AutoEscape, Environment, UndefinedBehavior, Value};

use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{Loadout, Variable};

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
    environment()
        .template_from_named_str(&name, source)
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
    let environment = environment();
    let template = environment
        .template_from_named_str(&name, source)
        .map_err(|error| syntax(file, &error))?;
    template.render(context).map_err(|error| Error {
        file: file.to_path_buf(),
        line: error.line(),
        problem: runtime(&error),
    })
}

/// Syntax-checks every `.tmpl` source a user file names, in any layer. Call it only on a loadout
/// that `Loadout::load` accepted: that is what confines each source before its content is read.
pub fn check_all(loadout: &Loadout) -> Diagnostics {
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
        let problem = match read(&loadout.root, file) {
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

fn read(root: &Path, file: &Path) -> Result<String, String> {
    let real = root
        .join(file)
        .canonicalize()
        .map_err(|error| format!("cannot read: {error}"))?;
    if !real.starts_with(root) {
        return Err("resolves outside the loadout".to_owned());
    }
    if !fs::metadata(&real).is_ok_and(|metadata| metadata.is_file()) {
        return Err("must be a regular file".to_owned());
    }
    let bytes = fs::read(&real).map_err(|error| format!("cannot read: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "a template must be UTF-8 text".to_owned())
}

fn environment<'source>() -> Environment<'source> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment.set_auto_escape_callback(|_| AutoEscape::None);
    environment.set_syntax(
        SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .build()
            .expect("the default delimiters are a valid syntax"),
    );
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

fn runtime(error: &minijinja::Error) -> Problem {
    let detail = error.detail().unwrap_or_default();
    match undefined_name(detail) {
        Some(name) => Problem::Missing(name.to_owned()),
        None if detail.is_empty() => Problem::Render(error.kind().to_string()),
        None => Problem::Render(detail.to_owned()),
    }
}

// minijinja names an undefined value only inside its detail text, as `name` is undefined.
fn undefined_name(detail: &str) -> Option<&str> {
    let end = detail.find("` is undefined")?;
    let start = detail[..end].rfind('`')? + 1;
    Some(&detail[start..end])
}
