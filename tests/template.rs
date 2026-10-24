use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loadout::machine::Facts;
use loadout::manifest::Variable;
use loadout::platform::{Arch, Os};
use loadout::template::{self, Error, Problem};

const FILE: &str = "files/gitconfig.tmpl";

fn linux() -> Facts {
    Facts {
        name: "work-laptop".to_owned(),
        os: Os::Linux,
        distro: Some("ubuntu".to_owned()),
        arch: Arch::X86_64,
        home: PathBuf::from("/home/dan"),
    }
}

fn windows() -> Facts {
    Facts {
        name: "gaming-pc".to_owned(),
        os: Os::Windows,
        distro: None,
        arch: Arch::Aarch64,
        home: PathBuf::from(r"C:\Users\dan"),
    }
}

fn variables() -> BTreeMap<String, Variable> {
    BTreeMap::from([
        ("name".to_owned(), Variable::Text("Dan Myles".to_owned())),
        (
            "email".to_owned(),
            Variable::Text("dan@example.com".to_owned()),
        ),
        ("work".to_owned(), Variable::Boolean(false)),
        ("label".to_owned(), Variable::Text("false".to_owned())),
        ("font_size".to_owned(), Variable::Integer(13)),
    ])
}

fn render(source: &str, facts: &Facts) -> Result<String, Error> {
    template::render(Path::new(FILE), source, &variables(), facts)
}

fn missing(source: &str) -> (Option<usize>, String) {
    match render(source, &linux()) {
        Err(Error {
            file,
            line,
            problem: Problem::Missing(variable),
        }) => {
            assert_eq!(file, Path::new(FILE));
            (line, variable)
        }
        other => panic!("{source:?} must fail on a missing variable, got {other:?}"),
    }
}

fn syntax(result: Result<impl std::fmt::Debug, Error>) -> (Option<usize>, String) {
    match result {
        Err(Error {
            line,
            problem: Problem::Syntax(message),
            ..
        }) => (line, message),
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn variables_and_machine_facts_fill_the_file() {
    let source = "[user]\n\tname = {{ name }}\n\temail = {{ email }}\n# {{ machine.name }} {{ machine.os }} {{ machine.distro }} {{ machine.arch }} {{ machine.home }}\n";
    assert_eq!(
        render(source, &linux()).unwrap(),
        "[user]\n\tname = Dan Myles\n\temail = dan@example.com\n# work-laptop linux ubuntu x86_64 /home/dan\n"
    );
    assert_eq!(
        render(
            "{{ machine.name }} {{ machine.os }} {{ machine.arch }} {{ machine.home }}",
            &windows()
        )
        .unwrap(),
        r"gaming-pc windows aarch64 C:\Users\dan"
    );
}

#[test]
fn if_else_follows_variable_types_and_facts() {
    let source = "{% if work %}work{% else %}home{% endif %} \
                  {% if label %}text is truthy{% endif %} \
                  {{ font_size + 1 }} {% if font_size > 12 %}big{% endif %} \
                  {% if machine.os == \"windows\" %}crlf{% elif machine.distro == \"ubuntu\" %}apt{% else %}other{% endif %}";
    assert_eq!(
        render(source, &linux()).unwrap(),
        "home text is truthy 14 big apt"
    );
    assert_eq!(
        render(source, &windows()).unwrap(),
        "home text is truthy 14 big crlf"
    );
    let mut work = variables();
    work.insert("work".to_owned(), Variable::Boolean(true));
    assert!(
        template::render(Path::new(FILE), source, &work, &linux())
            .unwrap()
            .starts_with("work ")
    );
}

#[test]
fn loops_and_set_work_on_local_values() {
    assert_eq!(
        render(
            "{% set greeting = \"hi \" ~ name %}{{ greeting }}{% for i in range(3) %} {{ i }}{% endfor %}",
            &linux()
        )
        .unwrap(),
        "hi Dan Myles 0 1 2"
    );
}

#[test]
fn text_without_tags_is_copied_byte_for_byte() {
    for source in [
        "plain\n",
        "keeps\r\nwindows\r\nline endings\r\n\r\n",
        "<a href=\"x\">&amp;</a>\n",
        "",
    ] {
        assert_eq!(render(source, &linux()).unwrap(), source);
    }
    let html = template::render(
        Path::new("files/page.html.tmpl"),
        "{{ name }} <{{ email }}>",
        &BTreeMap::from([
            ("name".to_owned(), Variable::Text("A & B".to_owned())),
            ("email".to_owned(), Variable::Text("<a@b>".to_owned())),
        ]),
        &linux(),
    )
    .unwrap();
    assert_eq!(html, "A & B <<a@b>>", "user files are never HTML-escaped");
}

#[test]
fn a_missing_variable_names_the_file_line_and_variable() {
    assert_eq!(
        missing("[user]\n\tsigningkey = {{ signing_key }}\n"),
        (Some(2), "signing_key".to_owned())
    );
    assert_eq!(missing("{% if gpg %}x{% endif %}").1, "gpg");
    assert_eq!(missing("{% for host in hosts %}{% endfor %}").1, "hosts");
    assert_eq!(missing("{{ editor | upper }}").1, "editor");
    assert_eq!(missing("{{ font_size + extra }}").1, "extra");
    assert_eq!(missing("{{ machine.hostname }}").1, "machine.hostname");
    assert_eq!(missing("{{ email.domain }}").1, "email.domain");

    let error = render("{{ signing_key }}", &linux()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "files/gitconfig.tmpl:1: variable `signing_key` is not defined for this machine"
    );
}

#[test]
fn a_branch_not_taken_needs_no_variable() {
    let source = "{% if machine.os == \"windows\" %}{{ windows_only }}{% else %}unix{% endif %}";
    assert_eq!(render(source, &linux()).unwrap(), "unix");
    assert_eq!(
        missing("{% if machine.os == \"linux\" %}{{ linux_only }}{% endif %}").1,
        "linux_only"
    );
    assert_eq!(
        render(
            "{% if gpg is defined %}{{ gpg }}{% endif %}{{ editor | default(\"vi\") }}",
            &linux()
        )
        .unwrap(),
        "vi"
    );
}

#[test]
fn templates_cannot_reach_files_or_the_environment() {
    for source in [
        "{% include \"files/secret\" %}",
        "{% import \"x.tmpl\" as x %}",
        "{% from \"x.tmpl\" import y %}",
        "{% extends \"base.tmpl\" %}",
        "{% block body %}{% endblock %}",
    ] {
        let (line, message) = syntax(template::check(Path::new(FILE), source));
        assert_eq!(line, Some(1));
        assert!(
            message.contains("cannot include, import, or extend"),
            "{message}"
        );
        syntax(render(source, &linux()));
    }
    let (_, message) = syntax(template::check(
        Path::new(FILE),
        "{% macro m() %}{% endmacro %}",
    ));
    assert_eq!(message, "unknown statement macro");

    assert!(
        std::env::var_os("PATH").is_some(),
        "the process has PATH, so a template that saw the environment would render it"
    );
    for name in ["PATH", "HOME", "env", "environ", "os"] {
        assert_eq!(missing(&format!("{{{{ {name} }}}}")).1, name);
    }
    let debug = render("{{ debug() }}", &linux()).unwrap_err();
    assert_eq!(
        debug.problem,
        Problem::Render("debug is unknown".to_owned())
    );
}

#[test]
fn a_none_distro_is_compared_but_never_printed() {
    assert_eq!(
        render(
            "{% if machine.distro == \"ubuntu\" %}apt{% else %}none{% endif %}",
            &windows()
        )
        .unwrap(),
        "none"
    );
    assert_eq!(
        render("{{ machine.distro or \"unknown\" }}", &windows()).unwrap(),
        "unknown"
    );
    match render("os\n{{ machine.distro }}", &windows()) {
        Err(Error {
            line: Some(2),
            problem: Problem::Render(message),
            ..
        }) => assert!(message.contains("machine.distro or"), "{message}"),
        other => panic!("printing none must fail, got {other:?}"),
    }
}

#[test]
fn check_needs_no_variables_but_rejects_bad_syntax() {
    template::check(
        Path::new(FILE),
        "{{ undefined_here }}{% if machine.os == \"windows\" %}{{ machine.nothing }}{% endif %}",
    )
    .unwrap();
    assert_eq!(
        syntax(template::check(
            Path::new(FILE),
            "a\nb\n{% if work %}\nno end\n"
        )),
        (
            Some(3),
            "unexpected end of input, expected end of block".to_owned()
        )
    );
    let (line, _) = syntax(template::check(Path::new(FILE), "ok\n{{ name"));
    assert_eq!(line, Some(2));
    let error = template::check(Path::new(FILE), "{% iff x %}").unwrap_err();
    assert_eq!(
        error.to_string(),
        "files/gitconfig.tmpl:1: syntax error: unknown statement iff"
    );
}

#[test]
fn render_reports_syntax_before_anything_else() {
    let (line, _) = syntax(render("{{ missing_var }}\n{% endif %}", &linux()));
    assert_eq!(line, Some(2));
}

#[test]
fn other_render_failures_keep_their_line() {
    match render("x\n{{ name | no_such_filter }}", &linux()) {
        Err(Error {
            line: Some(2),
            problem: Problem::Render(message),
            ..
        }) => assert!(message.contains("no_such_filter"), "{message}"),
        other => panic!("unknown filter must fail, got {other:?}"),
    }
    match render("{{ name + 1 }}", &linux()) {
        Err(Error {
            problem: Problem::Render(message),
            ..
        }) => assert!(message.contains("+ operator"), "{message}"),
        other => panic!("adding text to a number must fail, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_home_that_is_not_utf8_is_an_error_not_a_lossy_path() {
    use std::os::unix::ffi::OsStrExt;
    let mut facts = linux();
    facts.home = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/\xff"));
    let error = render("plain", &facts).unwrap_err();
    assert!(matches!(error.problem, Problem::Home(_)), "{error:?}");
}
