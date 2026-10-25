//! The install record, `loadout.install.toml`, next to the installed binary.
//!
//! The install scripts (`scripts/release/install.sh`, `install.ps1`) and self-update write it; only
//! self-update and doctor read it. The format is the frozen one in `docs/releasing.md`: UTF-8 with
//! no byte order mark, `\n` line endings and a final newline, no control characters, one header
//! comment, `schema = 1` or `schema = 2`, and one basic string per known key whose only escapes
//! are `\\` and `\"`. Schema 2 is the Windows two-binary layout and adds the background launcher.
//! It is parsed as data with this exact grammar, never through a general TOML reader, so a record
//! another tool rewrote, or one with any extra syntax, is invalid and the binary stays unmanaged.

use std::fmt::Write as _;

pub(super) const FILE: &str = "loadout.install.toml";
const HEADER: &str = "# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.";
const SCHEMA_1_KEYS: [&str; 8] = [
    "installer",
    "repository",
    "source",
    "target",
    "version",
    "archive_sha256",
    "binary",
    "binary_sha256",
];
const SCHEMA_2_EXTRA: [&str; 2] = ["background", "background_sha256"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub(super) installer: String,
    pub(super) repository: String,
    pub(super) source: String,
    pub(super) target: String,
    pub(super) version: String,
    pub(super) archive_sha256: String,
    pub(super) binary: String,
    pub(super) binary_sha256: String,
    /// Schema 2 only: the background launcher's path and hash.
    pub(super) background: Option<(String, String)>,
}

impl Record {
    pub(super) fn schema(&self) -> u8 {
        if self.background.is_some() { 2 } else { 1 }
    }
}

/// Parses a record. The error says what is wrong, for doctor and for the self-update refusal.
pub(super) fn parse(bytes: &[u8]) -> Result<Record, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "it is not valid UTF-8".to_owned())?;
    if text.starts_with('\u{feff}') {
        return Err("it starts with a byte order mark".to_owned());
    }
    let Some(body) = text.strip_suffix('\n') else {
        return Err("it does not end with a newline".to_owned());
    };
    let mut schema = None;
    let mut values: Vec<(&str, String)> = Vec::new();
    for line in body.split('\n') {
        if line.chars().any(char::is_control) {
            return Err("it contains a control character".to_owned());
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(number) = line.strip_prefix("schema = ") {
            if schema.is_some() {
                return Err("schema appears twice".to_owned());
            }
            schema = Some(match number {
                "1" => 1,
                "2" => 2,
                other => return Err(format!("schema {other:?} is not 1 or 2")),
            });
            continue;
        }
        let (key, value) = line
            .split_once(" = ")
            .ok_or_else(|| format!("line {line:?} is not part of the record format"))?;
        if !SCHEMA_1_KEYS.contains(&key) && !SCHEMA_2_EXTRA.contains(&key) {
            return Err(format!("unknown key {key:?}"));
        }
        if values.iter().any(|(seen, _)| *seen == key) {
            return Err(format!("{key} appears twice"));
        }
        values.push((
            key,
            unquote(value).ok_or_else(|| format!("{key} is not a basic string"))?,
        ));
    }
    let schema = schema.ok_or_else(|| "schema is missing".to_owned())?;
    let mut take = |key: &str| {
        values
            .iter()
            .position(|(seen, _)| *seen == key)
            .map(|index| values.swap_remove(index).1)
            .ok_or_else(|| format!("{key} is missing"))
    };
    let record = Record {
        installer: take("installer")?,
        repository: take("repository")?,
        source: take("source")?,
        target: take("target")?,
        version: take("version")?,
        archive_sha256: take("archive_sha256")?,
        binary: take("binary")?,
        binary_sha256: take("binary_sha256")?,
        background: match schema {
            1 => None,
            _ => Some((take("background")?, take("background_sha256")?)),
        },
    };
    if let Some((key, _)) = values.first() {
        return Err(format!("{key} does not belong in a schema {schema} record"));
    }
    Ok(record)
}

/// The exact bytes of a record, in the order the install scripts write it. Refuses a value with
/// a control character, so the result always parses back to the same record.
pub(super) fn render(record: &Record) -> Result<String, String> {
    let mut text = String::new();
    let _ = writeln!(text, "{HEADER}");
    let _ = writeln!(text, "schema = {}", record.schema());
    let mut field = |key: &str, value: &str| -> Result<(), String> {
        if value.chars().any(char::is_control) {
            return Err(format!(
                "{key} contains a control character, which the record can't hold"
            ));
        }
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(text, "{key} = \"{escaped}\"");
        Ok(())
    };
    field("installer", &record.installer)?;
    field("repository", &record.repository)?;
    field("source", &record.source)?;
    field("target", &record.target)?;
    field("version", &record.version)?;
    field("archive_sha256", &record.archive_sha256)?;
    field("binary", &record.binary)?;
    field("binary_sha256", &record.binary_sha256)?;
    if let Some((background, sha256)) = &record.background {
        field("background", background)?;
        field("background_sha256", sha256)?;
    }
    Ok(text)
}

fn unquote(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                escaped @ ('\\' | '"') => out.push(escaped),
                _ => return None,
            },
            '"' => return None,
            other => out.push(other),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(background: bool) -> Record {
        Record {
            installer: "install.ps1".to_owned(),
            repository: "dan-myles/loadout".to_owned(),
            source: "https://github.com/dan-myles/loadout/releases/latest/download/loadout-x86_64-pc-windows-msvc.zip"
                .to_owned(),
            target: "x86_64-pc-windows-msvc".to_owned(),
            version: "1.2.3".to_owned(),
            archive_sha256: "a".repeat(64),
            binary: r#"C:\Users\me "q"\AppData\Local\Programs\loadout\loadout.exe"#.to_owned(),
            binary_sha256: "b".repeat(64),
            background: background.then(|| {
                (
                    r"C:\Users\me\AppData\Local\Programs\loadout\loadout-background.exe".to_owned(),
                    "c".repeat(64),
                )
            }),
        }
    }

    #[test]
    fn both_schemas_round_trip_exactly() {
        for background in [false, true] {
            let record = sample(background);
            let text = render(&record).unwrap();
            assert!(text.starts_with(&format!("{HEADER}\nschema = {}\n", record.schema())));
            assert_eq!(parse(text.as_bytes()).unwrap(), record);
        }
    }

    #[test]
    fn install_sh_output_parses() {
        let text = format!(
            "{HEADER}\nschema = 1\ninstaller = \"install.sh\"\nrepository = \"dan-myles/loadout\"\n\
             source = \"http://127.0.0.1:8000/loadout-x86_64-unknown-linux-musl.tar.gz\"\n\
             target = \"x86_64-unknown-linux-musl\"\nversion = \"1.2.3\"\narchive_sha256 = \"{a}\"\n\
             binary = \"/home/me/.local/bin/loadout\"\nbinary_sha256 = \"{b}\"\n",
            a = "a".repeat(64),
            b = "b".repeat(64),
        );
        let record = parse(text.as_bytes()).unwrap();
        assert_eq!(record.schema(), 1);
        assert_eq!(record.binary, "/home/me/.local/bin/loadout");
    }

    #[test]
    fn anything_outside_the_grammar_is_invalid() {
        let good = render(&sample(false)).unwrap();
        let good2 = render(&sample(true)).unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "byte order mark",
                [b"\xef\xbb\xbf".as_slice(), good.as_bytes()].concat(),
            ),
            (
                "no final newline",
                good.trim_end_matches('\n').as_bytes().to_vec(),
            ),
            ("crlf", good.replace('\n', "\r\n").into_bytes()),
            ("invalid utf-8", [good.as_bytes(), b"# \xff\n"].concat()),
            (
                "nul",
                good.replacen("Do not edit.", "Do not\0edit.", 1)
                    .into_bytes(),
            ),
            (
                "tab",
                good.replacen("Do not edit.", "Do not\tedit.", 1)
                    .into_bytes(),
            ),
            (
                "quoted schema",
                good.replace("schema = 1", "schema = \"1\"").into_bytes(),
            ),
            (
                "schema 3",
                good.replace("schema = 1", "schema = 3").into_bytes(),
            ),
            ("no schema", good.replace("schema = 1\n", "").into_bytes()),
            ("unknown key", format!("{good}extra = \"x\"\n").into_bytes()),
            (
                "duplicate key",
                format!("{good}version = \"9\"\n").into_bytes(),
            ),
            (
                "missing key",
                good.replace("version = \"1.2.3\"\n", "").into_bytes(),
            ),
            (
                "bare value",
                good.replace("version = \"1.2.3\"", "version = 1.2.3")
                    .into_bytes(),
            ),
            (
                "other escape",
                good.replace("version = \"1.2.3\"", "version = \"1\\n2\"")
                    .into_bytes(),
            ),
            (
                "unescaped quote",
                good.replace("version = \"1.2.3\"", "version = \"1\"2\"")
                    .into_bytes(),
            ),
            (
                "schema 1 with launcher",
                good2.replace("schema = 2", "schema = 1").into_bytes(),
            ),
            (
                "schema 2 without launcher",
                good.replace("schema = 1", "schema = 2").into_bytes(),
            ),
            (
                "expression",
                good.replace("version = \"1.2.3\"", "version = \"$(rm x)\"\n$(rm x)")
                    .into_bytes(),
            ),
        ];
        for (name, bytes) in cases {
            assert!(parse(&bytes).is_err(), "{name} must be invalid");
        }
    }

    #[test]
    fn render_refuses_control_characters() {
        let mut record = sample(false);
        record.binary = "/home/bad\nname/loadout".to_owned();
        assert!(render(&record).is_err());
    }
}
