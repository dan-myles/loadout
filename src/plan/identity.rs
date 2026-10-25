use std::io;
use std::path::{Path, PathBuf};

use super::Target;
use super::volume::{self, Case};
use crate::context::Context;
use crate::manifest::McpShape;
use crate::platform::Os;
use crate::reserved::{self, Part};

/// Asks the nearest existing folder at or above a path how it treats case.
pub(super) type Probe<'a> = &'a dyn Fn(&Path) -> (PathBuf, Case);

/// The probe a platform provides; see `case_probe`.
pub(super) type ProbeFn = fn(&Path) -> (PathBuf, Case);

/// Where a destination lies: its path as written, and with every existing link resolved
/// (`reserved::normalize`). Two destinations are compared in every pair of forms, so a stable
/// link such as `~/alias -> ~/real` makes `~/alias/tool` and `~/real/tool` one place. Names stay
/// lossless and fold case only on Windows, the same rules state uses for its target roots. A
/// hard link, a bind mount, or a link changed after the plan is not covered.
#[derive(Debug, Clone)]
pub(super) struct Place {
    paths: Vec<PathBuf>,
    forms: Vec<Vec<Part>>,
}

/// Why a place couldn't be established.
#[derive(Debug)]
pub(super) enum Unresolved {
    /// The path itself is malformed, which a produced target never is.
    Internal(String),
    /// The file system couldn't say where the path leads.
    Unknown(String),
}

pub(super) fn place(context: &Context, path: &Path) -> Result<Place, Unresolved> {
    let fold_case = context.os == Os::Windows;
    let real = match reserved::normalize(path) {
        Ok(real) => real,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            return Err(Unresolved::Internal(format!("{}: {error}", path.display())));
        }
        Err(error) => {
            return Err(Unresolved::Unknown(format!(
                "loadout can't tell where {} leads ({error})",
                path.display()
            )));
        }
    };
    let forms = vec![
        reserved::identity(path, fold_case),
        reserved::identity(&real, fold_case),
    ];
    Ok(Place {
        paths: vec![path.to_path_buf(), real],
        forms,
    })
}

/// The file a concrete target writes: the path itself, or an MCP entry's document.
pub(super) fn file(target: &Target) -> Option<&Path> {
    match target {
        Target::Path { path } | Target::Entry { file: path, .. } => Some(path),
        Target::Program { .. } | Target::None => None,
    }
}

/// How two destinations relate, whatever their roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Disjoint,
    /// They are one destination, or one sits inside the other. `case` names the folder whose
    /// proven case-insensitivity made two spellings one, when that is how they met.
    Conflict {
        case: Option<PathBuf>,
    },
    /// They might be one destination: the folder that decides couldn't be shown to tell the
    /// two spellings apart or not.
    Unknown {
        dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Same,
    Nested,
}

fn relation(one: &Place, other: &Place) -> Option<Relation> {
    let mut nested = false;
    for a in &one.forms {
        for b in &other.forms {
            if a == b {
                return Some(Relation::Same);
            }
            if (a.len() > b.len() && a.starts_with(b)) || (b.len() > a.len() && b.starts_with(a)) {
                nested = true;
            }
        }
    }
    nested.then_some(Relation::Nested)
}

/// Whether two targets that share a file, or sit one inside the other, still stay apart. Only
/// two MCP entries in one document can: the same format, and neither key's address the other's
/// or inside it.
fn apart(one: &Target, other: &Target, relation: Relation) -> bool {
    match (one, other, relation) {
        (
            Target::Entry {
                format: one_format,
                pointer: one_pointer,
                key: one_key,
                shape: one_shape,
                ..
            },
            Target::Entry {
                format: other_format,
                pointer: other_pointer,
                key: other_key,
                shape: other_shape,
                ..
            },
            Relation::Same,
        ) => {
            one_format == other_format
                && !nested(
                    &address(one_pointer, one_key, *one_shape),
                    &address(other_pointer, other_key, *other_shape),
                )
        }
        _ => false,
    }
}

/// An MCP key's address in its document, by the same rule U5's collision check uses
/// (`agents/collide.rs`): the map pointer split on `/` without its leading empty part, each
/// part decoded `~1` to `/` and then `~0` to `~`, then the key, with `_` read as `-` for Pi,
/// which treats the two as one character. This is a private copy until a shared home below both
/// callers exists; the two must stay identical.
fn address(pointer: &str, key: &str, shape: McpShape) -> Vec<String> {
    let key = if shape == McpShape::PiJson {
        key.replace('_', "-")
    } else {
        key.to_owned()
    };
    pointer
        .split('/')
        .skip(1)
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .chain([key])
        .collect()
}

/// Where an MCP key is actually stored in its document: the map pointer's decoded parts and
/// the key exactly as written. Unlike `address`, nothing is folded, so a Cursor `my_docs` and a
/// Pi `my_docs` are one stored key even though Pi reads it as `my-docs`. Ownership migrations
/// compare this; collisions between writers compare `address`.
pub(super) fn storage(target: &Target) -> Option<Vec<String>> {
    let Target::Entry { pointer, key, .. } = target else {
        return None;
    };
    Some(
        pointer
            .split('/')
            .skip(1)
            .map(|part| part.replace("~1", "/").replace("~0", "~"))
            .chain([key.clone()])
            .collect(),
    )
}

/// Whether two places are one document: the same file as written or resolved, or two
/// spellings that the deciding folder proves, or might prove, to be one.
pub(super) fn document(one: &Place, other: &Place, probe: Option<Probe<'_>>) -> Verdict {
    let file = Target::Path {
        path: PathBuf::new(),
    };
    relate((&file, one), (&file, other), probe)
}

fn nested(one: &[String], other: &[String]) -> bool {
    one.starts_with(other) || other.starts_with(one)
}

/// How two concrete destinations relate. Their places are compared exactly first (written and
/// resolved, lossless). Where two spellings differ only by case, the folder that holds both
/// spellings is asked, read-only, how it treats case (`probe`, given on Linux and macOS; see
/// `case_probe`). A proven insensitive folder makes them one, a sensitive one keeps them apart,
/// and an unproven one leaves them possibly one. Two MCP entries that would stay apart even in
/// one document never conflict.
pub(super) fn relate(
    one: (&Target, &Place),
    other: (&Target, &Place),
    probe: Option<Probe<'_>>,
) -> Verdict {
    if let Some(relation) = relation(one.1, other.1) {
        return if apart(one.0, other.0, relation) {
            Verdict::Disjoint
        } else {
            Verdict::Conflict { case: None }
        };
    }
    let Some(probe) = probe else {
        return Verdict::Disjoint;
    };
    let mut unknown = None;
    for a in &one.1.paths {
        for b in &other.1.paths {
            let Some(pair) = volume::case_pair(a, b) else {
                continue;
            };
            let relation = if pair.same {
                Relation::Same
            } else {
                Relation::Nested
            };
            if apart(one.0, other.0, relation) {
                continue;
            }
            let mut verdicts: Vec<(PathBuf, Case)> = Vec::new();
            for prefix in &pair.divergences {
                verdicts.push(probe(prefix.as_path()));
            }
            if verdicts.iter().any(|(_, case)| *case == Case::Sensitive) {
                continue;
            }
            if let Some((dir, _)) = verdicts.iter().find(|(_, case)| *case == Case::Unknown) {
                unknown.get_or_insert_with(|| dir.clone());
                continue;
            }
            return Verdict::Conflict {
                case: verdicts.last().map(|(dir, _)| dir.clone()),
            };
        }
    }
    match unknown {
        Some(dir) => Verdict::Unknown { dir },
        None => Verdict::Disjoint,
    }
}

/// The case probe to use on this machine. Linux and macOS volumes can each be either way, folder
/// by folder on Linux, so the actual folder decides there. On Windows, `Place` already folds
/// ASCII case, as state does, and no read-only probe can prove more.
pub(super) fn case_probe(context: &Context) -> Option<ProbeFn> {
    match context.os {
        Os::Linux | Os::Macos => Some(volume::probe),
        Os::Windows => None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use super::*;
    use crate::manifest::McpFormat;

    fn context(home: &Path) -> Context {
        let home = home.as_os_str().to_owned();
        Context::resolve(
            Os::current().unwrap(),
            move |name| (name == "LOADOUT_HOME").then(|| home.clone()),
            None,
        )
        .unwrap()
    }

    fn at(path: PathBuf) -> Target {
        Target::Path { path }
    }

    fn entry(file: PathBuf, pointer: &str, key: &str) -> Target {
        mcp(file, McpFormat::Json, pointer, key, McpShape::ClaudeJson)
    }

    fn mcp(file: PathBuf, format: McpFormat, pointer: &str, key: &str, shape: McpShape) -> Target {
        Target::Entry {
            file,
            format,
            pointer: pointer.to_owned(),
            key: key.to_owned(),
            shape,
        }
    }

    fn verdict(
        context: &Context,
        one: &Target,
        other: &Target,
        probe: Option<Probe<'_>>,
    ) -> Verdict {
        let one_place = place(context, file(one).unwrap()).unwrap();
        let other_place = place(context, file(other).unwrap()).unwrap();
        relate((one, &one_place), (other, &other_place), probe)
    }

    fn exact(context: &Context, one: &Target, other: &Target) -> Verdict {
        verdict(context, one, other, None)
    }

    const CONFLICT: Verdict = Verdict::Conflict { case: None };

    #[test]
    fn the_address_rule_matches_u5_for_escapes_nesting_pi_names_and_disjoint_keys() {
        assert_eq!(
            address("/mcp~1servers/a~0b", "docs", McpShape::ClaudeJson),
            ["mcp/servers", "a~b", "docs"]
        );
        assert_eq!(address("/a~01", "k", McpShape::CursorJson), ["a~1", "k"]);
        assert_eq!(
            address("/mcpServers", "my_docs", McpShape::PiJson),
            ["mcpServers", "my-docs"]
        );
        assert_eq!(
            address("/mcpServers", "my_docs", McpShape::CursorJson),
            ["mcpServers", "my_docs"]
        );
        assert!(nested(
            &address("/servers", "group", McpShape::ClaudeJson),
            &address("/servers/group", "env", McpShape::ClaudeJson)
        ));
        assert!(nested(
            &address("/servers/group", "env", McpShape::ClaudeJson),
            &address("/servers", "group", McpShape::ClaudeJson)
        ));
        assert!(!nested(
            &address("/mcpServers", "docs", McpShape::ClaudeJson),
            &address("/mcpServers", "search", McpShape::ClaudeJson)
        ));
        assert_eq!(address("/a~1b", "k", McpShape::ClaudeJson), ["a/b", "k"]);
        assert!(!nested(
            &address("/a~1b", "k", McpShape::ClaudeJson),
            &address("/a/b", "k", McpShape::ClaudeJson)
        ));
    }

    #[test]
    fn entries_in_one_document_conflict_on_nested_addresses_pi_names_or_formats_and_not_otherwise()
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let context = context(&home);
        let doc = || home.join("custom.json");
        assert_eq!(
            exact(
                &context,
                &entry(doc(), "/servers", "group"),
                &entry(doc(), "/servers/group", "env")
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &entry(doc(), "/servers/group", "env"),
                &entry(doc(), "/servers", "group")
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &entry(doc(), "/mcpServers", "docs"),
                &entry(doc(), "/mcpServers", "search")
            ),
            Verdict::Disjoint
        );
        assert_eq!(
            exact(
                &context,
                &mcp(
                    doc(),
                    McpFormat::Json,
                    "/mcpServers",
                    "my_docs",
                    McpShape::PiJson
                ),
                &mcp(
                    doc(),
                    McpFormat::Json,
                    "/mcpServers",
                    "my-docs",
                    McpShape::PiJson
                )
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &mcp(
                    doc(),
                    McpFormat::Json,
                    "/mcpServers",
                    "docs",
                    McpShape::CursorJson
                ),
                &mcp(
                    doc(),
                    McpFormat::Jsonc,
                    "/other",
                    "search",
                    McpShape::CursorJson
                )
            ),
            CONFLICT,
            "one file has one format"
        );
        assert_eq!(
            exact(
                &context,
                &entry(doc(), "/mcp~1servers", "docs"),
                &entry(doc(), "/mcp", "servers")
            ),
            Verdict::Disjoint,
            "an escaped slash is part of one name"
        );
        assert_eq!(
            exact(&context, &at(doc()), &entry(doc(), "/mcpServers", "docs")),
            CONFLICT
        );
    }

    #[test]
    fn written_paths_conflict_when_equal_or_nested_and_siblings_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let context = context(&home);
        assert_eq!(
            exact(
                &context,
                &at(home.join(".tool")),
                &at(home.join(".tool/settings"))
            ),
            CONFLICT
        );
        assert_eq!(
            exact(&context, &at(home.join(".tool")), &at(home.join(".tools"))),
            Verdict::Disjoint
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_link_makes_two_spellings_one_place_but_keeps_distinct_files_and_keys_apart() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        fs::create_dir(home.join("real")).unwrap();
        std::os::unix::fs::symlink("real", home.join("alias")).unwrap();
        let context = context(&home);
        assert_eq!(
            exact(
                &context,
                &at(home.join("alias/tool")),
                &at(home.join("real/tool"))
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &at(home.join("alias")),
                &at(home.join("real/tool"))
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &at(home.join("alias/a")),
                &at(home.join("real/b"))
            ),
            Verdict::Disjoint
        );
        assert_eq!(
            exact(
                &context,
                &entry(home.join("alias/mcp.json"), "/servers", "group"),
                &entry(home.join("real/mcp.json"), "/servers/group", "env")
            ),
            CONFLICT
        );
        assert_eq!(
            exact(
                &context,
                &entry(home.join("alias/mcp.json"), "/mcpServers", "docs"),
                &entry(home.join("real/mcp.json"), "/mcpServers", "other")
            ),
            Verdict::Disjoint
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_and_a_case_spelling_meet_at_the_deciding_folder() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        fs::create_dir(home.join("real")).unwrap();
        std::os::unix::fs::symlink("real", home.join("alias")).unwrap();
        let context = context(&home);
        let asked = std::cell::RefCell::new(Vec::new());
        let as_case = |case: Case| {
            let asked = &asked;
            move |prefix: &Path| {
                asked.borrow_mut().push(prefix.to_path_buf());
                (prefix.to_path_buf(), case)
            }
        };
        let foo = at(home.join("alias/Foo"));
        let bar = at(home.join("real/foo"));
        let insensitive = as_case(Case::Insensitive);
        assert_eq!(
            verdict(&context, &foo, &bar, Some(&insensitive)),
            Verdict::Conflict {
                case: Some(home.join("real"))
            }
        );
        assert!(
            asked.borrow().contains(&home.join("real")),
            "the real folder decides"
        );
        let sensitive = as_case(Case::Sensitive);
        assert_eq!(
            verdict(&context, &foo, &bar, Some(&sensitive)),
            Verdict::Disjoint
        );
        let unknown = as_case(Case::Unknown);
        assert_eq!(
            verdict(&context, &foo, &bar, Some(&unknown)),
            Verdict::Unknown {
                dir: home.join("real")
            }
        );
        let keys = (
            entry(home.join("alias/Mcp.json"), "/mcpServers", "docs"),
            entry(home.join("real/mcp.json"), "/mcpServers", "other"),
        );
        assert_eq!(
            verdict(&context, &keys.0, &keys.1, Some(&insensitive)),
            Verdict::Disjoint,
            "compatible keys in one document stay apart, case or not"
        );
        assert_eq!(
            verdict(
                &context,
                &at(home.join("alias/a")),
                &at(home.join("real/b")),
                Some(&insensitive)
            ),
            Verdict::Disjoint
        );
    }

    /// On Windows a place itself folds ASCII case, so two spellings that differ only by case are
    /// one destination before any folder is asked, whatever a probe would say.
    #[cfg(windows)]
    #[test]
    fn windows_spellings_that_differ_only_by_case_are_one_destination_without_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let context = context(&home);
        let one = at(home.join("X/foo"));
        let other = at(home.join("x/Foo"));
        let sensitive = |prefix: &Path| (prefix.to_path_buf(), Case::Sensitive);
        for probe in [None, Some(&sensitive as Probe<'_>)] {
            assert_eq!(
                verdict(&context, &one, &other, probe),
                Verdict::Conflict { case: None }
            );
        }
    }

    /// Linux and macOS: the folders that hold the differing spellings decide.
    #[cfg(not(windows))]
    #[test]
    fn mixed_deciding_folders_need_every_difference_proven_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let context = context(&home);
        let one = at(home.join("X/foo"));
        let other = at(home.join("x/Foo"));
        let mixed = |prefix: &Path| {
            let case = if prefix == home {
                Case::Insensitive
            } else {
                Case::Unknown
            };
            (prefix.to_path_buf(), case)
        };
        assert!(matches!(
            verdict(&context, &one, &other, Some(&mixed)),
            Verdict::Unknown { .. }
        ));
        let split = |prefix: &Path| {
            let case = if prefix == home {
                Case::Insensitive
            } else {
                Case::Sensitive
            };
            (prefix.to_path_buf(), case)
        };
        assert_eq!(
            verdict(&context, &one, &other, Some(&split)),
            Verdict::Disjoint
        );
        let all = |prefix: &Path| (prefix.to_path_buf(), Case::Insensitive);
        assert!(matches!(
            verdict(&context, &one, &other, Some(&all)),
            Verdict::Conflict { case: Some(_) }
        ));
    }

    #[test]
    fn a_relative_path_is_an_internal_error_not_an_unknown_place() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        assert!(matches!(
            place(&context, Path::new("relative/tool")),
            Err(Unresolved::Internal(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_loop_is_an_unknown_place() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink("b", home.join("a")).unwrap();
        std::os::unix::fs::symlink("a", home.join("b")).unwrap();
        let context = context(&home);
        assert!(matches!(
            place(&context, &home.join("a/tool")),
            Err(Unresolved::Unknown(_))
        ));
    }
}
