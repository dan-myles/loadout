use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

/// How one directory treats names that differ only by case, as read from the file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    Insensitive,
    Sensitive,
    Unknown,
}

/// Two paths that differ as written but are equal, or one inside the other, when case is
/// folded. `divergences` holds the prefix up to each component where their spellings differ:
/// the folder that holds both spellings there.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CasePair {
    pub(super) same: bool,
    pub(super) divergences: Vec<PathBuf>,
}

pub(super) fn case_pair(a: &Path, b: &Path) -> Option<CasePair> {
    let parts = |path: &Path| -> Vec<OsString> {
        path.components()
            .map(|part| part.as_os_str().to_os_string())
            .collect()
    };
    let (ours, theirs) = (parts(a), parts(b));
    let shared = ours.len().min(theirs.len());
    let fold = |part: &OsString| part.to_string_lossy().to_lowercase();
    if ours == theirs || (0..shared).any(|index| fold(&ours[index]) != fold(&theirs[index])) {
        return None;
    }
    let divergences: Vec<PathBuf> = (0..shared)
        .filter(|index| ours[*index] != theirs[*index])
        .map(|index| a.components().take(index).collect::<PathBuf>())
        .collect();
    (!divergences.is_empty()).then_some(CasePair {
        same: ours.len() == theirs.len(),
        divergences,
    })
}

/// The nearest existing folder at or above `prefix`, and how it treats case. Read-only: it
/// lists the folder and stats one child under two spellings, and never creates anything.
pub(super) fn probe(prefix: &Path) -> (PathBuf, Case) {
    let mut dir = prefix.to_path_buf();
    loop {
        match fs::symlink_metadata(&dir) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !dir.pop() {
                    return (prefix.to_path_buf(), Case::Unknown);
                }
            }
            Err(_) => return (dir, Case::Unknown),
        }
    }
    let case = case_of(&dir);
    (dir, case)
}

/// A child whose name has an ASCII letter, statted under its own name and with every ASCII
/// letter's case swapped. The same file under both proves the folder ignores case; no file
/// under the swapped name, or a different one, proves it doesn't. A folder with no such child
/// proves nothing, and neither does its parent, since one folder can be casefolded under a
/// parent that isn't.
fn case_of(dir: &Path) -> Case {
    let Ok(entries) = fs::read_dir(dir) else {
        return Case::Unknown;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return Case::Unknown;
        };
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            continue;
        };
        if !text.bytes().any(|byte| byte.is_ascii_alphabetic()) {
            continue;
        }
        let swapped: String = text
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        let Ok(original) = fs::symlink_metadata(dir.join(&name)) else {
            return Case::Unknown;
        };
        return match fs::symlink_metadata(dir.join(swapped)) {
            Ok(other) => {
                #[cfg(unix)]
                let same = Some(same_file(&original, &other));
                // No stable file identity here: two spellings can't be proven one or two.
                #[cfg(not(unix))]
                let same = {
                    let _ = (&original, &other);
                    None
                };
                match same {
                    Some(true) => Case::Insensitive,
                    Some(false) => Case::Sensitive,
                    None => Case::Unknown,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Case::Sensitive,
            Err(_) => Case::Unknown,
        };
    }
    Case::Unknown
}

/// Whether two entries are one file.
#[cfg(unix)]
fn same_file(one: &Metadata, other: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    one.dev() == other.dev() && one.ino() == other.ino()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paths_that_differ_by_case_alone_are_pairs_with_their_deciding_folders() {
        assert_eq!(case_pair(Path::new("/h/.a"), Path::new("/h/.b")), None);
        assert_eq!(case_pair(Path::new("/h/.a"), Path::new("/h/.a")), None);
        assert_eq!(
            case_pair(Path::new("/h/alias/Foo"), Path::new("/h/real/foo")),
            None
        );
        assert_eq!(
            case_pair(Path::new("/h/x/Foo"), Path::new("/h/x/foo")),
            Some(CasePair {
                same: true,
                divergences: vec![PathBuf::from("/h/x")]
            })
        );
        assert_eq!(
            case_pair(Path::new("/h/X/foo/a"), Path::new("/h/x/Foo")),
            Some(CasePair {
                same: false,
                divergences: vec![PathBuf::from("/h"), PathBuf::from("/h/X")]
            })
        );
    }

    #[test]
    fn an_empty_folder_proves_nothing_and_its_parent_is_never_asked() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        fs::write(dir.path().join("Sibling"), "x").unwrap();
        assert_eq!(case_of(&empty), Case::Unknown);
        assert_eq!(probe(&empty.join("Foo")), (empty.clone(), Case::Unknown));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_ordinary_linux_folder_is_proven_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Notes"), "x").unwrap();
        assert_eq!(case_of(dir.path()), Case::Sensitive);
    }
}
