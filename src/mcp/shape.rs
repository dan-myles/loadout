use std::collections::BTreeMap;

use serde_json::{Map, Value as Json, json};

use super::Unsupported;
use crate::manifest::{EnvRef, EnvSyntax, McpShape, Value};
use crate::resolve::{McpServer, Transport};

struct Fields {
    kind: Option<(&'static str, &'static str, &'static str)>,
    command_list: bool,
    env: &'static str,
    url: &'static str,
    headers: &'static str,
    omit_empty: bool,
}

fn fields(shape: McpShape) -> Fields {
    let plain = Fields {
        kind: None,
        command_list: false,
        env: "env",
        url: "url",
        headers: "headers",
        omit_empty: false,
    };
    match shape {
        McpShape::ClaudeJson => Fields {
            kind: Some(("type", "stdio", "http")),
            ..plain
        },
        McpShape::CursorJson | McpShape::PiJson => plain,
        McpShape::MuseJson => Fields {
            kind: Some(("transport", "stdio", "streamable_http")),
            ..plain
        },
        McpShape::AntigravityJson => Fields {
            url: "serverUrl",
            ..plain
        },
        McpShape::OpencodeJson => Fields {
            kind: Some(("type", "local", "remote")),
            command_list: true,
            env: "environment",
            ..plain
        },
        McpShape::CodexToml => Fields {
            headers: "http_headers",
            omit_empty: true,
            ..plain
        },
        McpShape::GrokToml => Fields {
            omit_empty: true,
            ..plain
        },
    }
}

pub(super) fn entry(
    server: &McpServer,
    shape: McpShape,
    syntax: EnvSyntax,
) -> Result<Json, Unsupported> {
    if syntax == EnvSyntax::CodexEnvVars && shape != McpShape::CodexToml {
        return Err(Unsupported::CodexSyntax { shape });
    }
    let fields = fields(shape);
    let mut out = Map::new();
    let put = |key: &str, value: Json, out: &mut Map<String, Json>| {
        let empty = value.as_object().is_some_and(Map::is_empty)
            || value.as_array().is_some_and(Vec::is_empty);
        if !(fields.omit_empty && empty) {
            out.insert(key.to_owned(), value);
        }
    };
    match &server.transport {
        Transport::Stdio { command, args, env } => {
            if let Some((key, stdio, _)) = fields.kind {
                out.insert(key.to_owned(), json!(stdio));
            }
            if fields.command_list {
                let list = std::iter::once(command).chain(args).collect::<Vec<_>>();
                out.insert("command".to_owned(), json!(list));
            } else {
                out.insert("command".to_owned(), json!(command));
                out.insert("args".to_owned(), json!(args));
            }
            if syntax == EnvSyntax::CodexEnvVars {
                let (literal, passed) = codex_env(env)?;
                put(fields.env, Json::Object(literal), &mut out);
                put("env_vars", json!(passed), &mut out);
            } else {
                let env = references("env", env, syntax)?;
                put(fields.env, Json::Object(env), &mut out);
            }
        }
        Transport::Http { url, headers } => {
            if let Some((key, _, http)) = fields.kind {
                out.insert(key.to_owned(), json!(http));
            }
            out.insert(fields.url.to_owned(), json!(url));
            if syntax == EnvSyntax::CodexEnvVars {
                let (literal, bearer) = codex_headers(headers)?;
                if let Some(var) = bearer {
                    out.insert("bearer_token_env_var".to_owned(), json!(var));
                }
                put(fields.headers, Json::Object(literal), &mut out);
            } else {
                let headers = references("headers", headers, syntax)?;
                put(fields.headers, Json::Object(headers), &mut out);
            }
        }
    }
    Ok(Json::Object(out))
}

fn references(
    table: &str,
    values: &BTreeMap<String, Value>,
    syntax: EnvSyntax,
) -> Result<Map<String, Json>, Unsupported> {
    values
        .iter()
        .map(|(name, value)| {
            let text = match value {
                Value::Text(text) => text.clone(),
                Value::Env(EnvRef { env, prefix }) => {
                    let reference = match syntax {
                        EnvSyntax::DollarBrace => format!("${{{env}}}"),
                        EnvSyntax::DollarBraceEnv => format!("${{env:{env}}}"),
                        EnvSyntax::BraceEnv => format!("{{env:{env}}}"),
                        EnvSyntax::None | EnvSyntax::CodexEnvVars => {
                            return Err(Unsupported::NoEnvSyntax {
                                field: format!("{table}.{name}"),
                            });
                        }
                    };
                    format!("{}{reference}", prefix.as_deref().unwrap_or_default())
                }
            };
            Ok((name.clone(), Json::String(text)))
        })
        .collect()
}

fn codex_env(
    env: &BTreeMap<String, Value>,
) -> Result<(Map<String, Json>, Vec<String>), Unsupported> {
    let mut literal = Map::new();
    let mut passed = Vec::new();
    for (name, value) in env {
        match value {
            Value::Text(text) => {
                literal.insert(name.clone(), json!(text));
            }
            Value::Env(EnvRef { env, prefix: None }) if env == name => passed.push(env.clone()),
            Value::Env(reference) => {
                return Err(Unsupported::CodexEnv {
                    name: name.clone(),
                    var: reference.env.clone(),
                    prefix: reference.prefix.clone(),
                });
            }
        }
    }
    Ok((literal, passed))
}

const BEARER: &str = "Bearer ";

fn codex_headers(
    headers: &BTreeMap<String, Value>,
) -> Result<(Map<String, Json>, Option<String>), Unsupported> {
    let mut literal = Map::new();
    let mut bearer = None;
    for (name, value) in headers {
        match value {
            Value::Text(text) => {
                literal.insert(name.clone(), json!(text));
            }
            Value::Env(EnvRef {
                env,
                prefix: Some(prefix),
            }) if name.eq_ignore_ascii_case("authorization") && prefix == BEARER => {
                bearer = Some(env.clone());
            }
            Value::Env(reference) => {
                return Err(Unsupported::CodexHeader {
                    name: name.clone(),
                    var: reference.env.clone(),
                });
            }
        }
    }
    Ok((literal, bearer))
}
