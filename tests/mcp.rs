use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::context::Context;
use loadout::manifest::{EnvRef, EnvSyntax, McpFormat, McpShape, Value};
use loadout::mcp::{self, Error, Servers, Unsupported};
use loadout::platform::Os;
use loadout::resolve::{McpServer, Transport};
use serde_json::{Value as Json, json};

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn env(var: &str, prefix: Option<&str>) -> Value {
    Value::Env(EnvRef {
        env: var.to_owned(),
        prefix: prefix.map(str::to_owned),
    })
}

fn stdio(command: &str, args: &[&str], env: &[(&str, Value)]) -> McpServer {
    McpServer {
        transport: Transport::Stdio {
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        },
        agents: Vec::new(),
    }
}

fn http(url: &str, headers: &[(&str, Value)]) -> McpServer {
    McpServer {
        transport: Transport::Http {
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        },
        agents: Vec::new(),
    }
}

fn filesystem() -> McpServer {
    stdio(
        "npx",
        &["-y", "server-fs"],
        &[("LOG", text("warn")), ("TOKEN", env("TOKEN", None))],
    )
}

fn github() -> McpServer {
    http(
        "https://api.example.com/mcp/",
        &[("Authorization", env("GITHUB_TOKEN", Some("Bearer ")))],
    )
}

fn compact(value: &Json) -> String {
    serde_json::to_string(value).unwrap()
}

fn set(entries: &[(&str, Json)]) -> Servers {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn edit(
    before: &str,
    format: McpFormat,
    pointer: &str,
    shape: McpShape,
    servers: &Servers,
    remove: &[&str],
) -> String {
    let bytes = mcp::edit(
        before.as_bytes(),
        format,
        pointer,
        shape,
        servers,
        &names(remove),
    )
    .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn every_builtin_shape_writes_its_documented_stdio_fixture() {
    let cases = [
        (
            McpShape::ClaudeJson,
            EnvSyntax::DollarBrace,
            r#"{"type":"stdio","command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn","TOKEN":"${TOKEN}"}}"#,
        ),
        (
            McpShape::CodexToml,
            EnvSyntax::CodexEnvVars,
            r#"{"command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn"},"env_vars":["TOKEN"]}"#,
        ),
        (
            McpShape::CursorJson,
            EnvSyntax::DollarBraceEnv,
            r#"{"command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn","TOKEN":"${env:TOKEN}"}}"#,
        ),
        (
            McpShape::GrokToml,
            EnvSyntax::DollarBrace,
            r#"{"command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn","TOKEN":"${TOKEN}"}}"#,
        ),
        (
            McpShape::OpencodeJson,
            EnvSyntax::BraceEnv,
            r#"{"type":"local","command":["npx","-y","server-fs"],"environment":{"LOG":"warn","TOKEN":"{env:TOKEN}"}}"#,
        ),
        (
            McpShape::PiJson,
            EnvSyntax::DollarBrace,
            r#"{"command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn","TOKEN":"${TOKEN}"}}"#,
        ),
        (
            McpShape::MuseJson,
            EnvSyntax::DollarBrace,
            r#"{"transport":"stdio","command":"npx","args":["-y","server-fs"],"env":{"LOG":"warn","TOKEN":"${TOKEN}"}}"#,
        ),
    ];
    for (shape, syntax, expected) in cases {
        let entry = mcp::entry(&filesystem(), shape, syntax).unwrap();
        assert_eq!(compact(&entry), expected, "{}", shape.id());
    }
}

#[test]
fn every_builtin_shape_writes_its_documented_remote_fixture() {
    let cases = [
        (
            McpShape::ClaudeJson,
            EnvSyntax::DollarBrace,
            r#"{"type":"http","url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}"#,
        ),
        (
            McpShape::CodexToml,
            EnvSyntax::CodexEnvVars,
            r#"{"url":"https://api.example.com/mcp/","bearer_token_env_var":"GITHUB_TOKEN"}"#,
        ),
        (
            McpShape::CursorJson,
            EnvSyntax::DollarBraceEnv,
            r#"{"url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer ${env:GITHUB_TOKEN}"}}"#,
        ),
        (
            McpShape::GrokToml,
            EnvSyntax::DollarBrace,
            r#"{"url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}"#,
        ),
        (
            McpShape::OpencodeJson,
            EnvSyntax::BraceEnv,
            r#"{"type":"remote","url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer {env:GITHUB_TOKEN}"}}"#,
        ),
        (
            McpShape::PiJson,
            EnvSyntax::DollarBrace,
            r#"{"url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}"#,
        ),
        (
            McpShape::MuseJson,
            EnvSyntax::DollarBrace,
            r#"{"transport":"streamable_http","url":"https://api.example.com/mcp/","headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}"#,
        ),
    ];
    for (shape, syntax, expected) in cases {
        let entry = mcp::entry(&github(), shape, syntax).unwrap();
        assert_eq!(compact(&entry), expected, "{}", shape.id());
    }
}

#[test]
fn antigravity_takes_literal_values_and_server_url_but_no_env_reference() {
    let literal = stdio("uvx", &["tool"], &[("LOG", text("warn"))]);
    assert_eq!(
        compact(&mcp::entry(&literal, McpShape::AntigravityJson, EnvSyntax::None).unwrap()),
        r#"{"command":"uvx","args":["tool"],"env":{"LOG":"warn"}}"#
    );
    let remote = http("https://x.example/mcp", &[("X-Team", text("core"))]);
    assert_eq!(
        compact(&mcp::entry(&remote, McpShape::AntigravityJson, EnvSyntax::None).unwrap()),
        r#"{"serverUrl":"https://x.example/mcp","headers":{"X-Team":"core"}}"#
    );
    assert_eq!(
        mcp::entry(&filesystem(), McpShape::AntigravityJson, EnvSyntax::None),
        Err(Unsupported::NoEnvSyntax {
            field: "env.TOKEN".to_owned()
        })
    );
    assert_eq!(
        mcp::entry(&github(), McpShape::AntigravityJson, EnvSyntax::None),
        Err(Unsupported::NoEnvSyntax {
            field: "headers.Authorization".to_owned()
        })
    );
}

#[test]
fn a_custom_agent_combines_any_declared_shape_with_its_declared_syntax() {
    let entry = mcp::entry(&filesystem(), McpShape::CursorJson, EnvSyntax::BraceEnv).unwrap();
    assert_eq!(entry["env"]["TOKEN"], "{env:TOKEN}");
    let entry = mcp::entry(&github(), McpShape::CodexToml, EnvSyntax::DollarBrace).unwrap();
    assert_eq!(
        compact(&entry),
        r#"{"url":"https://api.example.com/mcp/","http_headers":{"Authorization":"Bearer ${GITHUB_TOKEN}"}}"#
    );
    assert!(matches!(
        mcp::entry(&filesystem(), McpShape::PiJson, EnvSyntax::None),
        Err(Unsupported::NoEnvSyntax { .. })
    ));
    assert_eq!(
        mcp::entry(&filesystem(), McpShape::CursorJson, EnvSyntax::CodexEnvVars),
        Err(Unsupported::CodexSyntax {
            shape: McpShape::CursorJson
        })
    );
}

#[test]
fn codex_refuses_env_and_headers_it_cannot_express() {
    let renamed = stdio("x", &[], &[("API_KEY", env("OTHER", None))]);
    let error = mcp::entry(&renamed, McpShape::CodexToml, EnvSyntax::CodexEnvVars).unwrap_err();
    assert_eq!(
        error,
        Unsupported::CodexEnv {
            name: "API_KEY".to_owned(),
            var: "OTHER".to_owned(),
            prefix: None
        }
    );
    assert!(error.to_string().contains("env_vars"));

    let prefixed = stdio("x", &[], &[("TOKEN", env("TOKEN", Some("Bearer ")))]);
    assert!(matches!(
        mcp::entry(&prefixed, McpShape::CodexToml, EnvSyntax::CodexEnvVars),
        Err(Unsupported::CodexEnv {
            prefix: Some(_),
            ..
        })
    ));

    for header in [
        ("X-Api-Key", env("KEY", None)),
        ("Authorization", env("TOKEN", None)),
        ("Authorization", env("TOKEN", Some("Token "))),
    ] {
        let remote = http("https://x.example/mcp", &[header]);
        assert!(matches!(
            mcp::entry(&remote, McpShape::CodexToml, EnvSyntax::CodexEnvVars),
            Err(Unsupported::CodexHeader { .. })
        ));
    }

    let accepted = http(
        "https://x.example/mcp",
        &[
            ("authorization", env("TOKEN", Some("Bearer "))),
            ("X-Team", text("core")),
        ],
    );
    assert_eq!(
        compact(&mcp::entry(&accepted, McpShape::CodexToml, EnvSyntax::CodexEnvVars).unwrap()),
        r#"{"url":"https://x.example/mcp","bearer_token_env_var":"TOKEN","http_headers":{"X-Team":"core"}}"#
    );
}

const CLAUDE: &str = r#"{
  "numStartups": 42,
  "oauthAccount": {
    "emailAddress": "someone@example.com",
    "organizationUuid": "00000000-0000-0000-0000-000000000000"
  },
  "projects": {
    "/home/u/src": {
      "allowedTools": [],
      "mcpServers": {}
    }
  },
  "mcpServers": {
    "user-tool": {
      "type": "stdio",
      "command": "mine",
      "args": [],
      "env": {}
    },
    "owned": {
      "type": "stdio",
      "command": "old",
      "args": [],
      "env": {}
    },
    "stale": {
      "type": "http",
      "url": "https://old.example/mcp",
      "headers": {}
    }
  },
  "tipsHistory": {"a": 1}
}
"#;

#[test]
fn claude_json_changes_only_owned_servers_and_keeps_account_state_byte_identical() {
    let servers = set(&[
        (
            "owned",
            mcp::entry(&filesystem(), McpShape::ClaudeJson, EnvSyntax::DollarBrace).unwrap(),
        ),
        (
            "github",
            mcp::entry(&github(), McpShape::ClaudeJson, EnvSyntax::DollarBrace).unwrap(),
        ),
    ]);
    let after = edit(
        CLAUDE,
        McpFormat::Json,
        "/mcpServers",
        McpShape::ClaudeJson,
        &servers,
        &["stale"],
    );
    let expected = r#"{
  "numStartups": 42,
  "oauthAccount": {
    "emailAddress": "someone@example.com",
    "organizationUuid": "00000000-0000-0000-0000-000000000000"
  },
  "projects": {
    "/home/u/src": {
      "allowedTools": [],
      "mcpServers": {}
    }
  },
  "mcpServers": {
    "user-tool": {
      "type": "stdio",
      "command": "mine",
      "args": [],
      "env": {}
    },
    "owned": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "server-fs"],
      "env": {
        "LOG": "warn",
        "TOKEN": "${TOKEN}"
      }
    },
    "github": {
      "type": "http",
      "url": "https://api.example.com/mcp/",
      "headers": {
        "Authorization": "Bearer ${GITHUB_TOKEN}"
      }
    }
  },
  "tipsHistory": {"a": 1}
}
"#;
    assert_eq!(after, expected);
    let back = mcp::parse(after.as_bytes(), McpFormat::Json, "/mcpServers").unwrap();
    assert_eq!(back["owned"], servers["owned"]);
    assert_eq!(back["github"], servers["github"]);
    assert!(!back.contains_key("stale"));
}

const OPENCODE: &str = r#"// OpenCode settings, edited by hand
{
  "$schema": "https://opencode.ai/config.json",
  /* theme first */
  "theme": "tokyonight",
  "mcp": {
    // my own server; loadout must not touch it
    "mine": { "type": "local", "command": ["mine"] },
    "owned": {
      "type": "local",
      "command": ["old"], // stale
    },
  },
  "keybinds": {
    "leader": "ctrl+x", // trailing comma below
  },
}
"#;

#[test]
fn opencode_jsonc_keeps_comments_and_trailing_commas() {
    let servers = set(&[
        (
            "owned",
            mcp::entry(&filesystem(), McpShape::OpencodeJson, EnvSyntax::BraceEnv).unwrap(),
        ),
        (
            "github",
            mcp::entry(&github(), McpShape::OpencodeJson, EnvSyntax::BraceEnv).unwrap(),
        ),
    ]);
    let after = edit(
        OPENCODE,
        McpFormat::Jsonc,
        "/mcp",
        McpShape::OpencodeJson,
        &servers,
        &[],
    );
    let expected = r#"// OpenCode settings, edited by hand
{
  "$schema": "https://opencode.ai/config.json",
  /* theme first */
  "theme": "tokyonight",
  "mcp": {
    // my own server; loadout must not touch it
    "mine": { "type": "local", "command": ["mine"] },
    "owned": {
      "type": "local",
      "command": ["npx", "-y", "server-fs"],
      "environment": {
        "LOG": "warn",
        "TOKEN": "{env:TOKEN}",
      },
    },
    "github": {
      "type": "remote",
      "url": "https://api.example.com/mcp/",
      "headers": {
        "Authorization": "Bearer {env:GITHUB_TOKEN}",
      },
    },
  },
  "keybinds": {
    "leader": "ctrl+x", // trailing comma below
  },
}
"#;
    assert_eq!(after, expected);

    let removed = edit(
        &after,
        McpFormat::Jsonc,
        "/mcp",
        McpShape::OpencodeJson,
        &Servers::new(),
        &["owned", "github"],
    );
    assert!(removed.contains("// my own server; loadout must not touch it"));
    assert!(removed.contains("\"mine\": { \"type\": \"local\", \"command\": [\"mine\"] }"));
    assert!(removed.contains("/* theme first */"));
    assert!(!removed.contains("owned") && !removed.contains("github"));
    assert_eq!(
        mcp::parse(removed.as_bytes(), McpFormat::Jsonc, "/mcp")
            .unwrap()
            .into_keys()
            .collect::<Vec<_>>(),
        ["mine"]
    );
}

#[test]
fn strict_json_rejects_comments_and_trailing_commas_that_jsonc_accepts() {
    for strict in [
        "{ // note\n \"mcpServers\": {} }",
        "{ \"mcpServers\": {}, }",
        "{ 'mcpServers': {} }",
        "{ mcpServers: {} }",
    ] {
        let error = mcp::parse(strict.as_bytes(), McpFormat::Json, "/mcpServers").unwrap_err();
        assert!(matches!(error, Error::Parse { .. }), "{strict}: {error}");
        let error = mcp::edit(
            strict.as_bytes(),
            McpFormat::Json,
            "/mcpServers",
            McpShape::AntigravityJson,
            &set(&[("a", json!({"command": "x"}))]),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, Error::Parse { .. }), "{strict}: {error}");
    }
    assert!(mcp::parse(b"{ // note\n \"mcp\": {}, }", McpFormat::Jsonc, "/mcp").is_ok());
}

const CODEX: &str = r#"# Codex config, hand-tuned
model = "gpt-5"
approval_policy = "on-request"

[projects."/home/u/src"]
trust_level = "trusted"

# servers I manage myself
[mcp_servers.mine]
command = "mine"   # keep this comment
args = []

# loadout owns this one
[mcp_servers.owned]
command = "old"

[mcp_servers.owned.env]
OLD = "1"

[mcp_servers.stale]
url = "https://old.example/mcp"

[tui]
notifications = true
"#;

#[test]
fn codex_toml_keeps_comments_key_order_projects_and_tui() {
    let servers = set(&[
        (
            "owned",
            mcp::entry(&filesystem(), McpShape::CodexToml, EnvSyntax::CodexEnvVars).unwrap(),
        ),
        (
            "github",
            mcp::entry(&github(), McpShape::CodexToml, EnvSyntax::CodexEnvVars).unwrap(),
        ),
    ]);
    let after = edit(
        CODEX,
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::CodexToml,
        &servers,
        &["stale"],
    );
    let expected = r#"# Codex config, hand-tuned
model = "gpt-5"
approval_policy = "on-request"

[projects."/home/u/src"]
trust_level = "trusted"

# servers I manage myself
[mcp_servers.mine]
command = "mine"   # keep this comment
args = []

# loadout owns this one
[mcp_servers.owned]
command = "npx"
args = ["-y", "server-fs"]
env = { LOG = "warn" }
env_vars = ["TOKEN"]

[mcp_servers.github]
url = "https://api.example.com/mcp/"
bearer_token_env_var = "GITHUB_TOKEN"

[tui]
notifications = true
"#;
    assert_eq!(after, expected);
    let back = mcp::parse(after.as_bytes(), McpFormat::Toml, "/mcp_servers").unwrap();
    assert_eq!(back["owned"], servers["owned"]);
    assert_eq!(back["github"], servers["github"]);
    assert_eq!(back["mine"], json!({"command": "mine", "args": []}));
}

#[test]
fn toml_maps_written_inline_or_dotted_keep_their_style() {
    let servers = set(&[("new", json!({"command": "x", "args": ["1"]}))]);
    let inline = edit(
        "mcp_servers = { a = { command = \"a\" } }\nother = 1\n",
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::GrokToml,
        &servers,
        &[],
    );
    assert_eq!(
        mcp::parse(inline.as_bytes(), McpFormat::Toml, "/mcp_servers")
            .unwrap()
            .into_keys()
            .collect::<Vec<_>>(),
        ["a", "new"]
    );
    assert!(inline.starts_with("mcp_servers = { a = { command = \"a\" }"));
    assert!(inline.ends_with("other = 1\n"));
    assert!(!inline.contains("[mcp_servers"));

    let dotted = edit(
        "mcp_servers.a.command = \"a\"\nother = 1\n",
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::GrokToml,
        &servers,
        &[],
    );
    assert!(dotted.starts_with("mcp_servers.a.command = \"a\"\nother = 1\n"));
    assert_eq!(
        mcp::parse(dotted.as_bytes(), McpFormat::Toml, "/mcp_servers")
            .unwrap()
            .into_keys()
            .collect::<Vec<_>>(),
        ["a", "new"]
    );

    let inline_entry = edit(
        "[mcp_servers]\nowned = { command = \"old\" }\n",
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::GrokToml,
        &set(&[("owned", json!({"command": "x"}))]),
        &[],
    );
    assert_eq!(inline_entry, "[mcp_servers]\nowned = { command = \"x\" }\n");
}

#[test]
fn grok_toml_keeps_disabled_servers_and_other_settings() {
    let before = "disabled_mcp_servers = [\"mine\"]\n\n[mcp_servers.mine]\ncommand = \"m\"\nenabled = false\n";
    let servers = set(&[(
        "github",
        mcp::entry(&github(), McpShape::GrokToml, EnvSyntax::DollarBrace).unwrap(),
    )]);
    let after = edit(
        before,
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::GrokToml,
        &servers,
        &[],
    );
    assert_eq!(
        after,
        format!(
            "{before}\n[mcp_servers.github]\nurl = \"https://api.example.com/mcp/\"\nheaders = {{ Authorization = \"Bearer ${{GITHUB_TOKEN}}\" }}\n"
        )
    );
}

#[test]
fn an_edit_that_changes_nothing_returns_the_input_bytes() {
    for (before, format, pointer, shape) in [
        (CLAUDE, McpFormat::Json, "/mcpServers", McpShape::ClaudeJson),
        (OPENCODE, McpFormat::Jsonc, "/mcp", McpShape::OpencodeJson),
        (CODEX, McpFormat::Toml, "/mcp_servers", McpShape::CodexToml),
        (
            "{\"a\": 1}",
            McpFormat::Json,
            "/absent/servers",
            McpShape::CursorJson,
        ),
        ("x = 1\n", McpFormat::Toml, "/absent", McpShape::GrokToml),
    ] {
        let after = edit(before, format, pointer, shape, &Servers::new(), &["absent"]);
        assert_eq!(after, before);
    }
}

#[test]
fn byte_order_mark_and_crlf_line_endings_survive() {
    let before = "\u{feff}{\r\n  \"a\": 1,\r\n  \"mcpServers\": {}\r\n}\r\n";
    let after = edit(
        before,
        McpFormat::Json,
        "/mcpServers",
        McpShape::CursorJson,
        &set(&[("s", json!({"command": "x"}))]),
        &[],
    );
    assert_eq!(
        after,
        "\u{feff}{\r\n  \"a\": 1,\r\n  \"mcpServers\": {\r\n    \"s\": {\r\n      \"command\": \"x\"\r\n    }\r\n  }\r\n}\r\n"
    );
    let parsed = mcp::parse(after.as_bytes(), McpFormat::Json, "/mcpServers").unwrap();
    assert_eq!(parsed["s"], json!({"command": "x"}));
}

#[test]
fn a_missing_file_starts_from_empty_bytes_in_every_format() {
    let server = json!({"command": "x", "args": []});
    let servers = set(&[("s", server.clone())]);
    let json = edit(
        "",
        McpFormat::Json,
        "/mcpServers",
        McpShape::PiJson,
        &servers,
        &[],
    );
    assert_eq!(
        json,
        "{\n  \"mcpServers\": {\n    \"s\": {\n      \"command\": \"x\",\n      \"args\": []\n    }\n  }\n}\n"
    );
    let toml = edit(
        "",
        McpFormat::Toml,
        "/mcp_servers",
        McpShape::GrokToml,
        &servers,
        &[],
    );
    assert_eq!(toml, "[mcp_servers.s]\ncommand = \"x\"\nargs = []\n");
    let nested = edit(
        "",
        McpFormat::Jsonc,
        "/a/b~1c",
        McpShape::OpencodeJson,
        &servers,
        &[],
    );
    assert_eq!(
        mcp::parse(nested.as_bytes(), McpFormat::Jsonc, "/a/b~1c").unwrap()["s"],
        server
    );
    assert!(nested.contains("\"b/c\""));
    assert_eq!(
        edit(
            "",
            McpFormat::Json,
            "/mcpServers",
            McpShape::PiJson,
            &Servers::new(),
            &["s"]
        ),
        ""
    );
}

#[test]
fn muse_settings_get_schema_version_one_and_refuse_other_versions() {
    let servers = set(&[(
        "fs",
        mcp::entry(&filesystem(), McpShape::MuseJson, EnvSyntax::DollarBrace).unwrap(),
    )]);
    let fresh = edit(
        "",
        McpFormat::Json,
        "/mcp_servers",
        McpShape::MuseJson,
        &servers,
        &[],
    );
    let value: Json = serde_json::from_str(&fresh).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert!(fresh.starts_with("{\n  \"schema_version\": 1,\n  \"mcp_servers\""));

    let existing = "{\n  \"model\": \"m\"\n}\n";
    let after = edit(
        existing,
        McpFormat::Json,
        "/mcp_servers",
        McpShape::MuseJson,
        &servers,
        &[],
    );
    assert!(after.starts_with("{\n  \"schema_version\": 1,\n  \"model\": \"m\","));

    let kept = "{\"schema_version\": 1, \"mcp_servers\": {}}";
    let after = edit(
        kept,
        McpFormat::Json,
        "/mcp_servers",
        McpShape::MuseJson,
        &servers,
        &[],
    );
    assert_eq!(after.matches("schema_version").count(), 1);

    let error = mcp::edit(
        b"{\"schema_version\": 2}",
        McpFormat::Json,
        "/mcp_servers",
        McpShape::MuseJson,
        &servers,
        &[],
    )
    .unwrap_err();
    assert!(
        matches!(error, Error::Schema { ref found } if found == "2"),
        "{error}"
    );

    let removal_only = edit(
        "{\"mcp_servers\": {\"fs\": {}}}",
        McpFormat::Json,
        "/mcp_servers",
        McpShape::MuseJson,
        &Servers::new(),
        &["fs"],
    );
    assert!(!removal_only.contains("schema_version"));
}

#[test]
fn pi_refuses_names_that_differ_only_by_dash_or_underscore() {
    let existing = br#"{"mcpServers": {"my_tool": {"command": "u"}}}"#;
    let servers = set(&[("my-tool", json!({"command": "x"}))]);
    let error = mcp::edit(
        existing,
        McpFormat::Json,
        "/mcpServers",
        McpShape::PiJson,
        &servers,
        &[],
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::Collision { name, other } if name == "my-tool" && other == "my_tool"),
        "{error}"
    );
    assert!(
        mcp::edit(
            existing,
            McpFormat::Json,
            "/mcpServers",
            McpShape::PiJson,
            &servers,
            &names(&["my_tool"])
        )
        .is_ok()
    );
    assert!(
        mcp::edit(
            existing,
            McpFormat::Json,
            "/mcpServers",
            McpShape::CursorJson,
            &servers,
            &[]
        )
        .is_ok()
    );
}

#[test]
fn invalid_pointers_are_refused_before_parsing() {
    for pointer in ["", "mcpServers", "/", "/a//b", "/a/", "/a~2", "/a~"] {
        assert!(
            matches!(
                mcp::parse(b"{}", McpFormat::Json, pointer),
                Err(Error::Pointer { .. })
            ),
            "{pointer:?}"
        );
        assert!(
            matches!(
                mcp::edit(
                    b"{}",
                    McpFormat::Json,
                    pointer,
                    McpShape::CursorJson,
                    &Servers::new(),
                    &[]
                ),
                Err(Error::Pointer { .. })
            ),
            "{pointer:?}"
        );
    }
    let parsed = mcp::parse(
        br#"{"a/b": {"~x": {"s": 1}}}"#,
        McpFormat::Json,
        "/a~1b/~0x",
    )
    .unwrap();
    assert_eq!(parsed["s"], 1);
}

#[test]
fn a_map_location_that_is_not_a_table_is_refused() {
    let cases: [(&str, McpFormat, &str); 6] = [
        ("{\"mcpServers\": \"x\"}", McpFormat::Json, "/mcpServers"),
        ("[]", McpFormat::Json, "/mcpServers"),
        ("{\"a\": [1]}", McpFormat::Jsonc, "/a/b"),
        ("mcp_servers = 5\n", McpFormat::Toml, "/mcp_servers"),
        ("[[mcp_servers]]\nx = 1\n", McpFormat::Toml, "/mcp_servers"),
        ("a = \"s\"\n", McpFormat::Toml, "/a/b"),
    ];
    for (before, format, pointer) in cases {
        let shape = if format == McpFormat::Toml {
            McpShape::GrokToml
        } else {
            McpShape::CursorJson
        };
        assert!(
            matches!(
                mcp::parse(before.as_bytes(), format, pointer),
                Err(Error::NotAnObject { .. })
            ),
            "{before}"
        );
        let error = mcp::edit(
            before.as_bytes(),
            format,
            pointer,
            shape,
            &set(&[("s", json!({"command": "x"}))]),
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::NotAnObject { .. }),
            "{before}: {error}"
        );
    }
}

#[test]
fn malformed_requests_and_files_fail_with_a_named_error() {
    let servers = set(&[("s", json!({"command": "x"}))]);
    assert!(matches!(
        mcp::edit(
            b"",
            McpFormat::Json,
            "/m",
            McpShape::CodexToml,
            &servers,
            &[]
        ),
        Err(Error::ShapeFormat { .. })
    ));
    assert!(matches!(
        mcp::edit(
            b"",
            McpFormat::Toml,
            "/m",
            McpShape::ClaudeJson,
            &servers,
            &[]
        ),
        Err(Error::ShapeFormat { .. })
    ));
    assert!(matches!(
        mcp::edit(
            b"",
            McpFormat::Json,
            "/m",
            McpShape::PiJson,
            &servers,
            &names(&["s"])
        ),
        Err(Error::SetAndRemove { .. })
    ));
    assert!(matches!(
        mcp::parse(b"{\"m\": {\"s\": 1, \"s\": 2}}", McpFormat::Json, "/m"),
        Err(Error::Duplicate { .. })
    ));
    assert!(matches!(
        mcp::edit(
            b"{\"m\": {\"s\": 1, \"s\": 2}}",
            McpFormat::Json,
            "/m",
            McpShape::CursorJson,
            &servers,
            &[]
        ),
        Err(Error::Duplicate { .. })
    ));
    assert!(matches!(
        mcp::parse(&[0xff, 0xfe, b'{'], McpFormat::Json, "/m"),
        Err(Error::NotUtf8)
    ));
    assert!(matches!(
        mcp::parse(b"a = = 1", McpFormat::Toml, "/m"),
        Err(Error::Parse { .. })
    ));
    assert!(matches!(
        mcp::parse(b"[m]\ns = nan\n", McpFormat::Toml, "/m"),
        Err(Error::Value { .. })
    ));
    let null = set(&[("s", json!({"command": "x", "cwd": null}))]);
    assert!(matches!(
        mcp::edit(b"", McpFormat::Toml, "/m", McpShape::GrokToml, &null, &[]),
        Err(Error::Value { ref pointer, .. }) if pointer == "/m/s/cwd"
    ));
}

#[test]
fn every_shape_round_trips_through_its_format() {
    let formats = [
        (
            McpShape::ClaudeJson,
            McpFormat::Json,
            EnvSyntax::DollarBrace,
        ),
        (
            McpShape::CodexToml,
            McpFormat::Toml,
            EnvSyntax::CodexEnvVars,
        ),
        (
            McpShape::CursorJson,
            McpFormat::Json,
            EnvSyntax::DollarBraceEnv,
        ),
        (McpShape::GrokToml, McpFormat::Toml, EnvSyntax::DollarBrace),
        (
            McpShape::OpencodeJson,
            McpFormat::Jsonc,
            EnvSyntax::BraceEnv,
        ),
        (McpShape::PiJson, McpFormat::Json, EnvSyntax::DollarBrace),
        (McpShape::MuseJson, McpFormat::Json, EnvSyntax::DollarBrace),
        (McpShape::AntigravityJson, McpFormat::Json, EnvSyntax::None),
    ];
    let literal = stdio("tool", &["a b", "c\"d"], &[("LOG", text("warn\n"))]);
    for (shape, format, syntax) in formats {
        let servers = set(&[
            ("lit", mcp::entry(&literal, shape, syntax).unwrap()),
            (
                "web",
                mcp::entry(&http("https://x.example/mcp", &[]), shape, syntax).unwrap(),
            ),
        ]);
        let bytes = mcp::edit(b"", format, "/servers", shape, &servers, &[]).unwrap();
        let back = mcp::parse(&bytes, format, "/servers").unwrap();
        assert_eq!(back, servers, "{}", shape.id());
    }
}

fn context(home: &Path, vars: &[(&str, &Path)]) -> Context {
    let vars: BTreeMap<String, OsString> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.as_os_str().to_owned()))
        .collect();
    Context::resolve(
        Os::current().unwrap(),
        |name| vars.get(name).cloned(),
        Some(home.to_path_buf()),
    )
    .unwrap()
}

fn write(path: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    path.to_path_buf()
}

#[test]
fn read_merges_only_where_the_reserved_table_allows() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let loadout = temp.path().join("checkout");
    fs::create_dir_all(&loadout).unwrap();
    let context = context(&home, &[]);
    let claude = write(&home.join(".claude.json"), CLAUDE);

    let servers = mcp::read(
        &context,
        &loadout,
        &claude,
        McpFormat::Json,
        "/mcpServers",
        McpShape::ClaudeJson,
    )
    .unwrap();
    assert_eq!(
        servers.into_keys().collect::<Vec<_>>(),
        ["owned", "stale", "user-tool"]
    );

    for pointer in ["/oauthAccount", "/projects"] {
        let error = mcp::read(
            &context,
            &loadout,
            &claude,
            McpFormat::Json,
            pointer,
            McpShape::ClaudeJson,
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::Reserved { .. }),
            "{pointer}: {error}"
        );
    }
    let error = mcp::read(
        &context,
        &loadout,
        &claude,
        McpFormat::Json,
        "/mcpServers",
        McpShape::CursorJson,
    )
    .unwrap_err();
    assert!(matches!(error, Error::Reserved { .. }));

    let codex_projects = mcp::read(
        &context,
        &loadout,
        &write(&home.join(".codex/config.toml"), CODEX),
        McpFormat::Toml,
        "/projects",
        McpShape::CodexToml,
    );
    assert!(matches!(codex_projects, Err(Error::Reserved { .. })));

    let auth = write(&home.join(".codex/auth.json"), "{\"mcpServers\": {}}");
    let error = mcp::read(
        &context,
        &loadout,
        &auth,
        McpFormat::Json,
        "/mcpServers",
        McpShape::CursorJson,
    )
    .unwrap_err();
    assert!(error.to_string().contains("auth"), "{error}");

    let inside = write(&loadout.join("mcp.json"), "{}");
    assert!(matches!(
        mcp::read(
            &context,
            &loadout,
            &inside,
            McpFormat::Json,
            "/mcpServers",
            McpShape::CursorJson
        ),
        Err(Error::Reserved { .. })
    ));

    let custom = write(&home.join(".devin/mcp.json"), "{\"servers\": {\"a\": {}}}");
    assert_eq!(
        mcp::read(
            &context,
            &loadout,
            &custom,
            McpFormat::Json,
            "/servers",
            McpShape::CursorJson
        )
        .unwrap()
        .len(),
        1
    );
    let missing = home.join(".cursor/mcp.json");
    assert!(
        mcp::read(
            &context,
            &loadout,
            &missing,
            McpFormat::Json,
            "/mcpServers",
            McpShape::CursorJson
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn read_follows_agent_dir_overrides_for_the_shared_config() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let pool = temp.path().join("pool");
    let loadout = temp.path().join("checkout");
    let context = context(&home, &[("CLAUDE_CONFIG_DIR", &pool)]);
    let claude = write(&pool.join(".claude.json"), CLAUDE);
    assert!(
        mcp::read(
            &context,
            &loadout,
            &claude,
            McpFormat::Json,
            "/mcpServers",
            McpShape::ClaudeJson
        )
        .is_ok()
    );
    assert!(matches!(
        mcp::read(
            &context,
            &loadout,
            &claude,
            McpFormat::Json,
            "/oauthAccount",
            McpShape::ClaudeJson
        ),
        Err(Error::Reserved { .. })
    ));
}

#[cfg(unix)]
#[test]
fn read_refuses_aliases_of_auth_stores_and_special_files() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let loadout = temp.path().join("checkout");
    let context = context(&home, &[]);

    let auth = write(&home.join(".grok/auth.json"), "{\"mcpServers\": {}}");
    let alias = home.join("tools/mcp.json");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    symlink(&auth, &alias).unwrap();
    let error = mcp::read(
        &context,
        &loadout,
        &alias,
        McpFormat::Json,
        "/mcpServers",
        McpShape::CursorJson,
    )
    .unwrap_err();
    assert!(matches!(error, Error::Reserved { .. }), "{error}");

    let dir = home.join("custom/mcp.json");
    fs::create_dir_all(&dir).unwrap();
    assert!(matches!(
        mcp::read(
            &context,
            &loadout,
            &dir,
            McpFormat::Json,
            "/mcpServers",
            McpShape::CursorJson
        ),
        Err(Error::NotAFile { .. })
    ));

    let fifo = home.join("custom/pipe.json");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(matches!(
        mcp::read(
            &context,
            &loadout,
            &fifo,
            McpFormat::Json,
            "/mcpServers",
            McpShape::CursorJson
        ),
        Err(Error::NotAFile { .. })
    ));
}

#[test]
fn lock_takes_groks_config_init_lock_and_only_for_grok() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let loadout = temp.path().join("checkout");
    let context = context(&home, &[]);
    let config = home.join(".grok/config.toml");

    let lock = mcp::lock(&context, &loadout, &config).unwrap().unwrap();
    let lock_path = fs::canonicalize(home.join(".grok"))
        .unwrap()
        .join(".config-init.lock");
    assert_eq!(lock.path(), lock_path);
    assert!(lock_path.is_file());
    assert!(!config.exists());

    let contender = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
    assert!(contender.try_lock().is_err());
    let error = mcp::lock(&context, &loadout, &config).unwrap_err();
    assert!(matches!(error, Error::Lock { .. }), "{error}");
    drop(lock);
    contender.try_lock().unwrap();
    contender.unlock().unwrap();

    for other in [
        home.join(".codex/config.toml"),
        home.join(".grok/other.toml"),
        home.join("elsewhere/config.toml"),
    ] {
        assert!(mcp::lock(&context, &loadout, &other).unwrap().is_none());
    }
    assert!(!home.join("elsewhere").exists());
}

#[test]
fn lock_follows_grok_home_override() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let grok = temp.path().join("grok-home");
    let loadout = temp.path().join("checkout");
    let context = context(&home, &[("GROK_HOME", &grok)]);
    let lock = mcp::lock(&context, &loadout, &grok.join("config.toml"))
        .unwrap()
        .unwrap();
    assert!(lock.path().ends_with("grok-home/.config-init.lock"));
    assert!(
        mcp::lock(&context, &loadout, &home.join(".grok/config.toml"))
            .unwrap()
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn lock_never_follows_a_linked_lock_file() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let loadout = temp.path().join("checkout");
    let context = context(&home, &[]);
    let secret = write(&temp.path().join("vault/token"), "do not touch");
    fs::create_dir_all(home.join(".grok")).unwrap();
    symlink(&secret, home.join(".grok/.config-init.lock")).unwrap();
    let error = mcp::lock(&context, &loadout, &home.join(".grok/config.toml")).unwrap_err();
    assert!(matches!(error, Error::Lock { .. }), "{error}");
    assert_eq!(fs::read_to_string(&secret).unwrap(), "do not touch");

    let dangling = temp.path().join("vault/created-by-lock");
    fs::remove_file(home.join(".grok/.config-init.lock")).unwrap();
    symlink(&dangling, home.join(".grok/.config-init.lock")).unwrap();
    assert!(mcp::lock(&context, &loadout, &home.join(".grok/config.toml")).is_err());
    assert!(!dangling.exists());
}
