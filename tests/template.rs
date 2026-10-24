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

fn render_with_typo_absent(source: &str) -> Result<String, Error> {
    assert!(!variables().contains_key("typo"));
    render(source, &linux())
}

#[test]
fn a_missing_variable_cannot_hide_inside_builtins_containers_or_comparisons() {
    for source in [
        "{{ typo | pprint }}",
        "{% if typo is string %}yes{% else %}no{% endif %}",
        "{{ [typo] }}",
        "{{ {\"key\": typo} }}",
        "{{ {typo: 1} }}",
        "{{ [[1, typo]] }}",
        "{{ {\"k\": [{\"j\": typo}]} | length }}",
        "{{ (1, typo) | length }}",
        "{% if typo == \"x\" %}x{% else %}y{% endif %}",
        "{% if typo != \"x\" %}x{% endif %}",
        "{{ typo in [\"a\"] }}",
        "{{ \"a\" in [typo] }}",
        "{% set alias = typo %}set",
        "{% with alias = typo %}with{% endwith %}",
        "{{ dict(key=typo) | length }}",
        "{{ [1] | join(typo) }}",
        "{{ 1 is sameas(typo) }}",
        "{{ typo is none }}",
        "{{ typo is divisibleby(2) }}",
        "{{ typo.upper() }}",
        "{% filter replace(\"a\", typo) %}a{% endfilter %}",
        "{% set block | replace(\"a\", typo) %}a{% endset %}{{ block }}",
        "{% for x in [1] if typo %}{% endfor %}",
        "{{ [typo] | first is defined }}",
        "{{ typo ~ \"\" }}",
        "{{ \"%s\" | format(typo) }}",
    ] {
        match render_with_typo_absent(source) {
            Err(Error {
                line: Some(1),
                problem: Problem::Missing(variable),
                ..
            }) => assert_eq!(variable, "typo", "{source:?}"),
            other => panic!("{source:?} must fail on the missing `typo`, got {other:?}"),
        }
    }
}

#[test]
fn a_missing_attribute_or_item_is_named_in_full() {
    for (source, missing) in [
        ("{{ machine.typo | pprint }}", "machine.typo"),
        ("{{ [machine.typo] }}", "machine.typo"),
        ("{{ email.domain | pprint }}", "email.domain"),
        ("{{ {\"a\": 1}.b | pprint }}", "{\"a\": 1}.b"),
        ("{{ [1][5] | pprint }}", "[1][5]"),
        ("{{ machine[\"typo\"] is string }}", "machine[\"typo\"]"),
    ] {
        match render_with_typo_absent(source) {
            Err(Error {
                line: Some(1),
                problem: Problem::Missing(variable),
                ..
            }) => assert_eq!(variable, missing, "{source:?}"),
            other => panic!("{source:?} must fail on {missing}, got {other:?}"),
        }
    }
}

#[test]
fn an_undefined_builtin_result_is_an_error_too() {
    match render_with_typo_absent("x\n{{ machine | attr(\"typo\") | pprint }}") {
        Err(Error {
            line: Some(2),
            problem: Problem::Render(message),
            ..
        }) => assert_eq!(message, "`machine | attr(\"typo\")` is undefined"),
        other => panic!("an undefined attr() result must fail, got {other:?}"),
    }
    assert!(matches!(
        render_with_typo_absent("{{ [] | first | pprint }}"),
        Err(Error {
            problem: Problem::Render(_),
            ..
        })
    ));
}

#[test]
fn default_and_is_defined_are_the_only_escapes() {
    for (source, expected) in [
        ("{{ typo is defined }}", "False"),
        ("{{ typo is undefined }}", "True"),
        ("{{ typo | default(\"fallback\") }}", "fallback"),
        ("{{ typo | d(\"fallback\") }}", "fallback"),
        ("{{ machine.typo is defined }}", "False"),
        ("{{ machine.typo | default(machine.os) }}", "linux"),
        ("{{ email.domain is defined }}", "False"),
        ("{{ ([] | first) is defined }}", "False"),
        ("{{ [] | first | default(\"empty\") }}", "empty"),
        ("{{ name | default(\"unused\") }}", "Dan Myles"),
        (
            "{% if typo is defined and typo == \"x\" %}x{% else %}skipped{% endif %}",
            "skipped",
        ),
        (
            "{% if typo is defined %}{{ typo | pprint }}{% else %}absent{% endif %}",
            "absent",
        ),
        (
            "{% if work %}{{ [typo] }}{{ typo | pprint }}{% endif %}done",
            "done",
        ),
        ("{{ \"shown\" if typo is undefined else typo }}", "shown"),
    ] {
        assert_eq!(
            render_with_typo_absent(source).as_deref(),
            Ok(expected),
            "{source:?}"
        );
    }
    for (source, missing) in [
        (
            "{% if typo is defined or typo == \"x\" %}{% endif %}",
            "typo",
        ),
        ("{{ typo.attr is defined }}", "typo"),
        ("{{ typo | default(other_typo) }}", "other_typo"),
        ("{{ [typo] is defined }}", "typo"),
        ("{{ (typo ~ \"\") | default(\"x\") }}", "typo"),
    ] {
        match render_with_typo_absent(source) {
            Err(Error {
                problem: Problem::Missing(variable),
                ..
            }) => assert_eq!(variable, missing, "{source:?}"),
            other => panic!("{source:?} must fail on {missing}, got {other:?}"),
        }
    }
}

#[test]
fn the_line_is_where_the_missing_value_is_used() {
    let (line, variable) = missing("{{ name ~\n   \"-\" ~\n   typo }}");
    assert_eq!((line, variable.as_str()), (Some(3), "typo"));
    let (line, _) = missing("{% if work %}\n{% else %}\n\n  {{ [1,\n typo] }}\n{% endif %}");
    assert_eq!(line, Some(5));
}

#[test]
fn ordinary_builtins_and_statements_still_work() {
    let source = "{{ [1, 2] | join(\",\") }} {{ email | upper }} {{ email | lower }} \
                  {% for i in range(2) %}{{ loop.index }}{% endfor %} \
                  {% set ns = namespace(n=1) %}{% set ns.n = ns.n + 1 %}{{ ns.n }} \
                  {% filter upper %}x{% endfilter %} {{ dict(a=1).a }} \
                  {% set pair = (1, 2) %}{% set a, b = pair %}{{ a + b }} \
                  {% with w = font_size %}{{ w }}{% endwith %} \
                  {{ \"a-b\" | replace(\"-\", \"+\") }} {{ \"%s!\" | format(name) }} \
                  {{ \"\" if false }}|{{ email is string }}";
    assert_eq!(
        render(source, &linux()).unwrap(),
        "1,2 DAN@EXAMPLE.COM dan@example.com 12 2 X 1 3 13 a+b Dan Myles! |true"
            .replace("true", "True")
    );
}

#[test]
fn divisibleby_zero_is_a_template_error_not_a_panic() {
    match render("ok\n{{ 1 is divisibleby(0) }}", &linux()) {
        Err(Error {
            file,
            line: Some(2),
            problem: Problem::Render(message),
        }) => {
            assert_eq!(file, Path::new(FILE));
            assert!(message.contains("divisibleby(0)"), "{message}");
        }
        other => panic!("divisibleby(0) must be a template error, got {other:?}"),
    }
    assert!(render("{{ 1.0 is divisibleby(0.0) }}", &linux()).is_err());
    assert_eq!(
        render(
            "{{ 6 is divisibleby(3) }} {{ 7 is divisibleby(2) }} {{ 1.5 is divisibleby(0.5) }} \
             {{ font_size is divisibleby(13) }} {{ \"x\" is divisibleby(2) }}",
            &linux()
        )
        .unwrap(),
        "True False True True False"
    );
}

#[test]
fn an_engine_panic_is_this_files_error_not_a_crash() {
    match render("{{ [1, 2] | slice(4611686018427387904) }}", &linux()) {
        Err(Error {
            file,
            line: None,
            problem: Problem::Render(message),
        }) => {
            assert_eq!(file, Path::new(FILE));
            assert!(
                message.starts_with("the template engine failed: "),
                "{message}"
            );
        }
        other => panic!("an engine panic must be a template error, got {other:?}"),
    }
    assert_eq!(render("{{ name }}", &linux()).unwrap(), "Dan Myles");
}

#[test]
fn attribute_builtins_require_the_attribute_unless_given_a_default() {
    for source in [
        "{{ [machine] | map(attribute=\"hostname\") | join(\",\") }}",
        "{{ [machine] | sort(attribute=\"hostname\") | length }}",
        "{{ [machine] | sort(attribute=\"os, hostname\") | length }}",
        "{{ [machine] | groupby(\"hostname\") | length }}",
        "{{ [machine] | groupby(attribute=\"hostname\") | length }}",
        "{{ [machine] | unique(attribute=\"hostname\") | length }}",
        "{{ [machine] | min(attribute=\"hostname\") | length }}",
        "{{ [machine] | max(attribute=\"hostname\") | length }}",
    ] {
        match render(source, &linux()) {
            Err(Error {
                line: Some(1),
                problem: Problem::Render(message),
                ..
            }) => assert!(
                message.contains("an item has no `hostname`"),
                "{source:?}: {message}"
            ),
            other => panic!("{source:?} must fail on the missing attribute, got {other:?}"),
        }
    }
    assert_eq!(
        render(
            "{{ [machine] | map(attribute=\"hostname\", default=\"h\") | join }} \
             {{ [machine] | map(attribute=\"os\") | join }} \
             {{ [machine] | groupby(\"hostname\", default=\"g\") | map(attribute=\"grouper\") | join }} \
             {{ [machine, machine] | sort(attribute=\"os,arch\") | length }} \
             {{ [machine] | unique(attribute=\"os\") | length }} \
             {{ [machine] | max(attribute=\"name\") | attr(\"name\") }} \
             {{ [{\"a\": [1, 2]}] | map(attribute=\"a.1\") | join }} \
             {{ [\"a\"] | map(\"upper\") | join }}",
            &linux()
        )
        .unwrap(),
        "h linux g 2 1 work-laptop 2 A"
    );
}
