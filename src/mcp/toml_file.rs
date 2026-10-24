use std::collections::BTreeMap;

use serde_json::{Map, Number, Value as Json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use super::{Error, Servers, pointer};
use crate::manifest::McpFormat;

fn document(text: &str) -> Result<DocumentMut, Error> {
    text.parse()
        .map_err(|error: toml_edit::TomlError| Error::Parse {
            format: McpFormat::Toml,
            message: error.to_string(),
        })
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::None => "nothing",
        Item::Table(_) => "a table",
        Item::ArrayOfTables(_) => "an array of tables",
        Item::Value(value) => match value {
            Value::String(_) => "a string",
            Value::Integer(_) => "an integer",
            Value::Float(_) => "a float",
            Value::Boolean(_) => "a boolean",
            Value::Datetime(_) => "a datetime",
            Value::Array(_) => "an array",
            Value::InlineTable(_) => "a table",
        },
    }
}

fn map<'a>(
    doc: &'a mut DocumentMut,
    parts: &[String],
    create: bool,
) -> Result<Option<(&'a mut dyn TableLike, bool)>, Error> {
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    let mut inline = false;
    for (depth, part) in parts.iter().enumerate() {
        let at = pointer::prefix(&parts[..=depth]);
        if table.get(part).is_none() {
            if !create {
                return Ok(None);
            }
            let item = if inline {
                Item::Value(Value::InlineTable(InlineTable::new()))
            } else {
                let mut child = Table::new();
                child.set_implicit(true);
                Item::Table(child)
            };
            table.insert(part, item);
        }
        let item = table.get_mut(part).expect("present or just inserted");
        let found = kind(item);
        inline = item.is_inline_table();
        table = item
            .as_table_like_mut()
            .ok_or(Error::NotAnObject { pointer: at, found })?;
    }
    Ok(Some((table, inline)))
}

pub(super) fn parse(text: &str, parts: &[String]) -> Result<Servers, Error> {
    let mut doc = document(text)?;
    let Some((table, _)) = map(&mut doc, parts, false)? else {
        return Ok(BTreeMap::new());
    };
    let at = pointer::prefix(parts);
    table
        .iter()
        .map(|(name, item)| Ok((name.to_owned(), item_json(item, &format!("{at}/{name}"))?)))
        .collect()
}

pub(super) fn edit(
    text: &str,
    parts: &[String],
    set: &Servers,
    remove: &[String],
) -> Result<String, Error> {
    let crlf = text.contains("\r\n");
    if crlf && text.replace("\r\n", "").contains('\n') {
        return Err(Error::MixedNewlines);
    }
    let mut doc = document(text)?;
    let at = pointer::prefix(parts);
    let entries = set
        .iter()
        .map(|(name, value)| Ok((name, entry(value, &format!("{at}/{name}"))?)))
        .collect::<Result<Vec<_>, Error>>()?;
    if let Some((table, inline)) = map(&mut doc, parts, !set.is_empty())? {
        for name in remove {
            table.remove(name);
        }
        for (name, mut new) in entries {
            let keep_inline = match table.get(name) {
                Some(Item::Table(old)) => {
                    new.decor_mut().clone_from(old.decor());
                    new.set_position(old.position());
                    false
                }
                Some(Item::Value(_)) => true,
                _ => inline,
            };
            let item = if keep_inline {
                Item::Value(Value::InlineTable(new.into_inline_table()))
            } else {
                Item::Table(new)
            };
            table.insert(name, item);
        }
    }
    // toml_edit writes every line ending as LF and always ends with one, so the file's own line
    // endings and missing final newline are put back.
    let mut out = doc.to_string();
    if crlf {
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let newline = if crlf { "\r\n" } else { "\n" };
    if !text.is_empty()
        && !text.ends_with('\n')
        && let Some(kept) = out.strip_suffix(newline).map(str::len)
    {
        out.truncate(kept);
    }
    Ok(out)
}

fn entry(value: &Json, at: &str) -> Result<Table, Error> {
    let Json::Object(object) = value else {
        return Err(Error::Value {
            pointer: at.to_owned(),
            message: "a TOML server entry must be an object".to_owned(),
        });
    };
    let mut table = Table::new();
    for (key, value) in object {
        table.insert(key, Item::Value(toml_value(value, &format!("{at}/{key}"))?));
    }
    Ok(table)
}

fn toml_value(value: &Json, at: &str) -> Result<Value, Error> {
    Ok(match value {
        Json::Null => {
            return Err(Error::Value {
                pointer: at.to_owned(),
                message: "TOML has no null".to_owned(),
            });
        }
        Json::Bool(value) => Value::from(*value),
        Json::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::from(integer)
            } else if let Some(float) = number.as_f64().filter(|_| !number.is_u64()) {
                Value::from(float)
            } else {
                return Err(Error::Value {
                    pointer: at.to_owned(),
                    message: format!("{number} does not fit a TOML integer"),
                });
            }
        }
        Json::String(text) => Value::from(text.as_str()),
        Json::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| toml_value(value, &format!("{at}/{index}")))
                .collect::<Result<Array, Error>>()?,
        ),
        Json::Object(object) => {
            let mut table = InlineTable::new();
            for (key, value) in object {
                table.insert(key, toml_value(value, &format!("{at}/{key}"))?);
            }
            Value::InlineTable(table)
        }
    })
}

fn item_json(item: &Item, at: &str) -> Result<Json, Error> {
    match item {
        Item::None => Ok(Json::Null),
        Item::Value(value) => value_json(value, at),
        Item::Table(table) => table_json(table, at),
        Item::ArrayOfTables(tables) => tables
            .iter()
            .enumerate()
            .map(|(index, table)| table_json(table, &format!("{at}/{index}")))
            .collect(),
    }
}

fn table_json(table: &dyn TableLike, at: &str) -> Result<Json, Error> {
    table
        .iter()
        .map(|(key, item)| Ok((key.to_owned(), item_json(item, &format!("{at}/{key}"))?)))
        .collect::<Result<Map<_, _>, Error>>()
        .map(Json::Object)
}

fn value_json(value: &Value, at: &str) -> Result<Json, Error> {
    Ok(match value {
        Value::String(text) => Json::String(text.value().clone()),
        Value::Integer(integer) => Json::from(*integer.value()),
        Value::Float(float) => Number::from_f64(*float.value())
            .map(Json::Number)
            .ok_or_else(|| Error::Value {
                pointer: at.to_owned(),
                message: format!("{} has no JSON form", float.value()),
            })?,
        Value::Boolean(boolean) => Json::Bool(*boolean.value()),
        Value::Datetime(datetime) => {
            return Err(Error::Value {
                pointer: at.to_owned(),
                message: format!("the datetime {} has no JSON form", datetime.value()),
            });
        }
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| value_json(value, &format!("{at}/{index}")))
            .collect::<Result<_, _>>()?,
        Value::InlineTable(table) => table_json(table, at)?,
    })
}
