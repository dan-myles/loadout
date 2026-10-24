use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const FILE: &[u8] = b"loadout:file:v1\n";
const TREE: &[u8] = b"loadout:tree:v1\n";
const ENTRY: &[u8] = b"loadout:mcp-entry:v1\n";

pub(super) struct Blob<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) executable: bool,
}

fn mode_byte(executable: bool) -> u8 {
    if executable && !cfg!(windows) {
        b'x'
    } else {
        b'-'
    }
}

fn length(bytes: &[u8]) -> [u8; 8] {
    u64::try_from(bytes.len())
        .expect("a length fits in u64")
        .to_be_bytes()
}

fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a String never fails");
        out
    })
}

pub(super) fn file(blob: &Blob<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FILE);
    hasher.update([mode_byte(blob.executable), 0]);
    hasher.update(length(blob.bytes));
    hasher.update(blob.bytes);
    hex(&hasher.finalize())
}

/// `files` is keyed by the `/`-separated relative path; `String`'s order is byte order.
pub(super) fn tree<'a>(files: impl IntoIterator<Item = (&'a str, Blob<'a>)>) -> String {
    let sorted: BTreeMap<&str, Blob<'_>> = files.into_iter().collect();
    let mut hasher = Sha256::new();
    hasher.update(TREE);
    for (path, blob) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0, mode_byte(blob.executable), 0]);
        hasher.update(length(blob.bytes));
        hasher.update(blob.bytes);
    }
    hex(&hasher.finalize())
}

pub(super) fn entry(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ENTRY);
    hasher.update(canonical_json(value));
    hex(&hasher.finalize())
}

/// The sort is explicit because `serde_json`'s `preserve_order` feature, if another unit enables
/// it, makes `Map` keep insertion order.
pub(super) fn canonical_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&sorted(value)).expect("a JSON value always serializes")
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&object[key])))
                    .collect::<Map<String, Value>>(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        scalar => scalar.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sha(parts: &[&[u8]]) -> String {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        hex(&hasher.finalize())
    }

    #[test]
    fn file_hash_follows_the_documented_byte_layout() {
        let blob = Blob {
            bytes: b"hi\n",
            executable: false,
        };
        let expected = sha(&[b"loadout:file:v1\n", b"-\0", &3u64.to_be_bytes(), b"hi\n"]);
        assert_eq!(file(&blob), expected);
    }

    #[test]
    fn tree_hash_follows_the_documented_byte_layout_in_byte_order() {
        let files = [
            (
                "b",
                Blob {
                    bytes: b"2",
                    executable: false,
                },
            ),
            (
                "a/x",
                Blob {
                    bytes: b"1",
                    executable: true,
                },
            ),
        ];
        let x = if cfg!(windows) { b"-" } else { b"x" };
        let expected = sha(&[
            b"loadout:tree:v1\n",
            b"a/x\0",
            x,
            b"\0",
            &1u64.to_be_bytes(),
            b"1",
            b"b\0-\0",
            &1u64.to_be_bytes(),
            b"2",
        ]);
        assert_eq!(tree(files), expected);
    }

    #[test]
    fn entry_hash_sorts_keys_recursively_and_ignores_formatting() {
        let a = json!({"b": [{"z": 1, "y": "2"}], "a": null});
        let b: Value =
            serde_json::from_str(r#"{ "a" : null , "b" : [ { "y":"2","z":1 } ] }"#).unwrap();
        assert_eq!(
            canonical_json(&a),
            br#"{"a":null,"b":[{"y":"2","z":1}]}"#.to_vec()
        );
        assert_eq!(entry(&a), entry(&b));
        assert_eq!(
            entry(&a),
            sha(&[
                b"loadout:mcp-entry:v1\n",
                br#"{"a":null,"b":[{"y":"2","z":1}]}"#
            ])
        );
    }

    #[test]
    fn kinds_never_collide() {
        let empty = Blob {
            bytes: b"",
            executable: false,
        };
        let digests = [file(&empty), tree([]), entry(&json!(""))];
        assert_ne!(digests[0], digests[1]);
        assert_ne!(digests[1], digests[2]);
        assert_ne!(digests[0], digests[2]);
    }
}
