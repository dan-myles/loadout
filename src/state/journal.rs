use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::disk::{self, io};
use super::{Error, Record};
use crate::plan::{Change, ItemRef, Observed, Target};

pub(super) const FILE: &str = "journal.json";
pub(super) const RUNS: &str = "runs";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Status {
    Running,
    Finished,
    RolledBack,
}

/// Where one journaled action got to. `Started` with no later step means the run stopped
/// between taking the backup and confirming the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Step {
    Pending,
    Started,
    Done,
    Released,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Backup {
    /// Relative to the run folder.
    pub(super) path: String,
    pub(super) kind: BackupKind,
    /// File modes, keyed by relative path ("" for a single file).
    #[serde(default)]
    pub(super) modes: BTreeMap<String, u32>,
    /// Folder modes for a tree, keyed by relative path ("" for its root).
    #[serde(default)]
    pub(super) dirs: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum BackupKind {
    Pending,
    File,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    pub(super) item: ItemRef,
    pub(super) target: Target,
    pub(super) change: Change,
    pub(super) prior: Observed,
    /// The ownership record before this run, restored by rollback.
    pub(super) record: Option<Record>,
    pub(super) backup: Option<Backup>,
    pub(super) step: Step,
    pub(super) after: Option<Observed>,
    /// Folders this action created, so rollback can remove them again when empty.
    #[serde(default)]
    pub(super) created: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Journal {
    pub(super) version: u32,
    pub(super) run: String,
    pub(super) started: String,
    /// The active loadout checkout, which rollback keeps reserved like the run did.
    pub(super) loadout: PathBuf,
    pub(super) status: Status,
    pub(super) entries: Vec<Entry>,
}

impl Journal {
    pub(super) fn new(run: String, started: String, loadout: PathBuf, entries: Vec<Entry>) -> Self {
        Self {
            version: VERSION,
            run,
            started,
            loadout,
            status: Status::Running,
            entries,
        }
    }

    pub(super) fn save(&self, dir: &Path) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| Error::Invalid {
            path: dir.join(FILE),
            message: error.to_string(),
        })?;
        disk::save(&dir.join(FILE), &bytes)
    }

    pub(super) fn load(dir: &Path, state: &Path) -> Result<Self, Error> {
        let path = dir.join(FILE);
        let bytes = disk::read_internal(&path, state)?.ok_or_else(|| Error::Invalid {
            path: path.clone(),
            message: "is missing".to_owned(),
        })?;
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|error| Error::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if journal.version != VERSION {
            return Err(Error::Invalid {
                path,
                message: format!("has version {}, expected {VERSION}", journal.version),
            });
        }
        Ok(journal)
    }
}

/// Run ids are UTC timestamps with nanoseconds, so their byte order is their time order.
pub(super) fn now() -> (String, String) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let days = i64::try_from(seconds / 86_400).expect("days since 1970 fit in i64");
    let (year, month, day) = civil(days);
    let rest = seconds % 86_400;
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let nanos = elapsed.subsec_nanos();
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}.{nanos:09}Z"),
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("day of month");
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("month");
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The run folders under `<state>/runs`, oldest first. Only names loadout could have written
/// count, so a stray file or an odd name is never taken for a run.
pub(super) fn run_ids(state: &Path) -> Result<Vec<String>, Error> {
    let runs = state.join(RUNS);
    let entries = match fs::read_dir(&runs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io("list", &runs)(error)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io("list", &runs))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_run_id(&name) && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            ids.push(name);
        }
    }
    ids.sort();
    Ok(ids)
}

pub(super) fn is_run_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 26
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 => *byte == b'T',
            15 => *byte == b'.',
            25 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_750), (2026, 10, 24));
    }

    #[test]
    fn run_ids_are_fixed_width_timestamps() {
        let (id, started) = now();
        assert!(is_run_id(&id), "{id}");
        assert_eq!(started.len(), 20, "{started}");
        assert!(!is_run_id("../../etc"));
        assert!(!is_run_id("20261024T035600.123456789Z/"));
    }
}
