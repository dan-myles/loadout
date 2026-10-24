use std::collections::BTreeMap;

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstObjectProp, CstRootNode};
use serde_json::{Map, Number, Value as Json};

use super::{Error, Servers, pointer};
use crate::manifest::McpFormat;

fn options(format: McpFormat) -> ParseOptions {
    let comments = format == McpFormat::Jsonc;
    ParseOptions {
        allow_comments: comments,
        allow_trailing_commas: comments,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

fn root(text: &str, format: McpFormat) -> Result<CstRootNode, Error> {
    let options = options(format);
    let failed = |error: jsonc_parser::errors::ParseError| Error::Parse {
        format,
        message: error.to_string(),
    };
    // The CST keeps comments as tokens even when they are disallowed, so strict JSON is checked
    // by the plain parser first.
    if format == McpFormat::Json {
        jsonc_parser::parse_to_value(text, &options).map_err(failed)?;
    }
    CstRootNode::parse(text, &options).map_err(failed)
}

fn kind(node: &CstNode) -> &'static str {
    if node.as_object().is_some() {
        "an object"
    } else if node.as_array().is_some() {
        "an array"
    } else if node.as_string_lit().is_some() {
        "a string"
    } else if node.as_number_lit().is_some() {
        "a number"
    } else if node.as_boolean_lit().is_some() {
        "a boolean"
    } else if node.as_null_keyword().is_some() {
        "null"
    } else {
        "an unsupported value"
    }
}

// jsonc-parser's own conversion turns a number serde_json can't hold (such as 1e400) into a
// string, which would hash like that string, so values are converted here and refuse instead.
fn value(node: &CstNode, at: &str) -> Result<Json, Error> {
    let refuse = |message: String| Error::Value {
        pointer: at.to_owned(),
        message,
    };
    if let Some(object) = node.as_object() {
        object_value(&object, at)
    } else if let Some(array) = node.as_array() {
        array
            .elements()
            .iter()
            .enumerate()
            .map(|(index, element)| value(element, &format!("{at}/{index}")))
            .collect::<Result<_, _>>()
            .map(Json::Array)
    } else if let Some(text) = node.as_string_lit() {
        text.decoded_value()
            .map(Json::String)
            .map_err(|error| refuse(format!("unreadable string ({error:?})")))
    } else if node.as_number_lit().is_some() {
        let raw = node.to_string();
        let integer = !raw.contains(['.', 'e', 'E']);
        raw.parse::<Number>()
            .ok()
            .filter(|number| !integer || number.is_i64() || number.is_u64())
            .map(Json::Number)
            .ok_or_else(|| refuse(format!("the number {raw} has no exact JSON value")))
    } else if let Some(boolean) = node.as_boolean_lit() {
        Ok(Json::Bool(boolean.value()))
    } else if node.as_null_keyword().is_some() {
        Ok(Json::Null)
    } else {
        Err(refuse(format!("{node} is not a JSON value")))
    }
}

fn object_value(object: &CstObject, at: &str) -> Result<Json, Error> {
    let mut out = Map::new();
    for prop in object.properties() {
        let name = prop.decoded_name().ok_or_else(|| Error::Value {
            pointer: at.to_owned(),
            message: "unreadable property name".to_owned(),
        })?;
        let child = pointer::child(at, &name);
        let node = prop.value().ok_or_else(|| Error::Value {
            pointer: child.clone(),
            message: "missing value".to_owned(),
        })?;
        let converted = value(&node, &child)?;
        if out.insert(name.clone(), converted).is_some() {
            return Err(Error::Duplicate {
                pointer: at.to_owned(),
                key: name,
            });
        }
    }
    Ok(Json::Object(out))
}

fn property(object: &CstObject, name: &str, at: &str) -> Result<Option<CstObjectProp>, Error> {
    let mut found = object
        .properties()
        .into_iter()
        .filter(|prop| prop.decoded_name().as_deref() == Some(name));
    let first = found.next();
    if found.next().is_some() {
        return Err(Error::Duplicate {
            pointer: at.to_owned(),
            key: name.to_owned(),
        });
    }
    Ok(first)
}

fn object_at(prop: &CstObjectProp, at: &str) -> Result<CstObject, Error> {
    prop.object_value().ok_or_else(|| Error::NotAnObject {
        pointer: at.to_owned(),
        found: prop.value().as_ref().map_or("nothing", kind),
    })
}

fn root_object(root: &CstRootNode, create: bool) -> Result<Option<CstObject>, Error> {
    match root.value() {
        None if create => Ok(Some(root.object_value_or_set())),
        None => Ok(None),
        Some(node) => node
            .as_object()
            .map(Some)
            .ok_or_else(|| Error::NotAnObject {
                pointer: String::new(),
                found: kind(&node),
            }),
    }
}

fn map(root: &CstRootNode, parts: &[String], create: bool) -> Result<Option<CstObject>, Error> {
    let Some(mut object) = root_object(root, create)? else {
        return Ok(None);
    };
    for (depth, part) in parts.iter().enumerate() {
        let parent = pointer::prefix(&parts[..depth]);
        let at = pointer::prefix(&parts[..=depth]);
        object = match property(&object, part, &parent)? {
            Some(prop) => object_at(&prop, &at)?,
            None if create => object
                .append(part, CstInputValue::Object(Vec::new()))
                .object_value_or_set(),
            None => return Ok(None),
        };
    }
    Ok(Some(object))
}

pub(super) fn parse(text: &str, format: McpFormat, parts: &[String]) -> Result<Servers, Error> {
    let root = root(text, format)?;
    let Some(object) = map(&root, parts, false)? else {
        return Ok(BTreeMap::new());
    };
    match object_value(&object, &pointer::prefix(parts))? {
        Json::Object(servers) => Ok(servers.into_iter().collect()),
        _ => unreachable!("object_value returns an object"),
    }
}

pub(super) struct Change<'a> {
    pub(super) set: &'a Servers,
    pub(super) remove: &'a [String],
    pub(super) schema_version: bool,
}

pub(super) fn edit(
    text: &str,
    format: McpFormat,
    parts: &[String],
    change: &Change<'_>,
) -> Result<String, Error> {
    let root = root(text, format)?;
    let fresh = root.value().is_none();
    let create = !change.set.is_empty();
    let at = pointer::prefix(parts);
    if change.schema_version {
        schema_version(&root, create)?;
    }
    if let Some(object) = map(&root, parts, create)? {
        for name in change.remove {
            if let Some(prop) = property(&object, name, &at)? {
                prop.remove();
            }
        }
        for (name, value) in change.set {
            let value = input(value);
            match property(&object, name, &at)? {
                Some(prop) => prop.set_value(value),
                None => {
                    object.append(name, value);
                }
            }
        }
    }
    let mut out = root.to_string();
    if fresh && create && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

const SCHEMA_VERSION: &str = "schema_version";

pub(super) fn check_schema_version(text: &str, format: McpFormat) -> Result<(), Error> {
    schema_version(&root(text, format)?, false)
}

fn schema_version(root: &CstRootNode, insert: bool) -> Result<(), Error> {
    let Some(object) = root_object(root, insert)? else {
        return Ok(());
    };
    match property(&object, SCHEMA_VERSION, "")? {
        None if insert => {
            object.insert(0, SCHEMA_VERSION, CstInputValue::Number("1".to_owned()));
            Ok(())
        }
        None => Ok(()),
        Some(prop) => {
            let found = prop
                .value()
                .map(|node| value(&node, "/schema_version"))
                .transpose()?;
            if found.as_ref().and_then(Json::as_u64) == Some(1) {
                Ok(())
            } else {
                Err(Error::Schema {
                    found: found.map_or_else(|| "missing".to_owned(), |value| value.to_string()),
                })
            }
        }
    }
}

fn input(value: &Json) -> CstInputValue {
    match value {
        Json::Null => CstInputValue::Null,
        Json::Bool(value) => CstInputValue::Bool(*value),
        Json::Number(number) => CstInputValue::Number(number.to_string()),
        Json::String(text) => CstInputValue::String(text.clone()),
        Json::Array(values) => CstInputValue::Array(values.iter().map(input).collect()),
        Json::Object(object) => CstInputValue::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), input(value)))
                .collect(),
        ),
    }
}
