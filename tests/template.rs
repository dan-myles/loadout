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
    for zero in [
        "0 is divisibleby(0)",
        "true is divisibleby(false)",
        "1.0 is divisibleby(0.0)",
        "1.5 is divisibleby(-0.0)",
        "3 is divisibleby(0.0)",
        "1.5 is divisibleby(false)",
    ] {
        match render(&format!("{{{{ {zero} }}}}"), &linux()) {
            Err(Error {
                line: Some(1),
                problem: Problem::Render(message),
                ..
            }) => assert_eq!(message, "divisibleby(0) divides by zero", "{zero}"),
            other => panic!("{zero} must be the zero-divisor error, got {other:?}"),
        }
    }
    assert_eq!(
        render(
            "{{ 6 is divisibleby(3) }} {{ 7 is divisibleby(2) }} {{ 1.5 is divisibleby(0.5) }} \
             {{ font_size is divisibleby(13) }} {{ \"x\" is divisibleby(2) }} \
             {{ true is divisibleby(0.5) }} {{ 9223372036854775808.0 is divisibleby(2) }} \
             {{ true is divisibleby(1) }} {{ 4 is divisibleby(2.0) }} {{ \"x\" is divisibleby(0) }} \
             {{ -9223372036854775808 is divisibleby(-1) }}",
            &linux()
        )
        .unwrap(),
        "True False True True False True True True True False True",
        "every input that didn't panic keeps minijinja's own result"
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

fn fails_with(source: &str, needle: &str) {
    match render(source, &linux()) {
        Err(Error {
            line: Some(_),
            problem: Problem::Render(message) | Problem::Missing(message),
            ..
        }) => assert!(message.contains(needle), "{source:?}: {message}"),
        other => panic!("{source:?} must fail with {needle:?}, got {other:?}"),
    }
}

fn renders(cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(
            render(source, &linux()).as_deref(),
            Ok(*expected),
            "{source:?}"
        );
    }
}

#[test]
fn selectattr_and_rejectattr_need_the_attribute_except_for_definedness_tests() {
    for source in [
        "{{ [machine] | selectattr(\"hostname\") | length }}",
        "{{ [machine] | rejectattr(\"hostname\") | length }}",
        "{{ [machine] | selectattr(\"hostname\", \"string\") | length }}",
        "{{ [machine] | rejectattr(\"hostname\", \"eq\", \"x\") | length }}",
        "{{ [machine] | selectattr(\"hostname.child\", \"undefined\") | length }}",
        "{{ [machine] | rejectattr(\"hostname.child\", \"defined\") | length }}",
    ] {
        fails_with(source, "an item has no `hostname`");
    }
    fails_with(
        "{{ [machine] | selectattr(\"os\", \"eq\", typo) | length }}",
        "typo",
    );
    renders(&[
        (
            "{{ [machine] | selectattr(\"os\", \"eq\", \"linux\") | length }}",
            "1",
        ),
        (
            "{{ [machine] | rejectattr(\"os\", \"eq\", \"linux\") | length }}",
            "0",
        ),
        ("{{ [machine] | selectattr(\"os\") | length }}", "1"),
        (
            "{{ [machine] | selectattr(\"hostname\", \"defined\") | length }}",
            "0",
        ),
        (
            "{{ [machine] | selectattr(\"hostname\", \"undefined\") | length }}",
            "1",
        ),
        (
            "{{ [machine] | rejectattr(\"hostname\", \"undefined\") | length }}",
            "0",
        ),
        ("{{ [] | selectattr(\"hostname\") | length }}", "0"),
    ]);
}

#[test]
fn a_map_result_is_checked_before_any_later_filter_or_test_sees_it() {
    for consumer in [
        "map(\"pprint\") | join",
        "length",
        "select(\"string\") | length",
        "reject | length",
        "sum",
        "join",
        "chain([1]) | list | length",
        "zip([1]) | list | pprint",
        "sort | length",
        "unique | length",
        "min",
        "max",
        "first is defined",
        "list | pprint",
    ] {
        fails_with(
            &format!("{{{{ [machine] | map(\"attr\", \"hostname\") | {consumer} }}}}"),
            "an item's result is undefined",
        );
    }
    fails_with(
        "{{ [[]] | map(\"first\") | length }}",
        "an item's result is undefined",
    );
    fails_with(
        "{{ [\"\"] | map(\"last\") | join }}",
        "an item's result is undefined",
    );
    fails_with(
        "{{ [[machine]] | map(\"map\", \"attr\", \"hostname\") | pprint }}",
        "an item's result is undefined",
    );
    fails_with(
        "{{ [machine] | map(attribute=\"hostname.child\", default=\"h\") | join }}",
        "an item has no `hostname`",
    );
    renders(&[
        ("{{ [\"a\", \"b\"] | map(\"upper\") | join }}", "AB"),
        ("{{ [[1], [2]] | map(\"first\") | sum }}", "3"),
        ("{{ range(3) | map(\"string\") | join }}", "012"),
        ("{{ [1, 2] | zip([3, 4]) | map(\"last\") | join }}", "34"),
        ("{{ [1] | chain([2]) | list | join }}", "12"),
        (
            "{{ [[1, 2], [3]] | map(\"map\", \"string\") | map(\"join\") | join(\",\") }}",
            "12,3",
        ),
        (
            "{{ range(4) | select(\"odd\") | map(\"string\") | reject(\"eq\", \"1\") | join }}",
            "3",
        ),
        ("{{ [] | map(\"upper\") | list | length }}", "0"),
        ("{{ [] | map(\"first\") | sum }}", "0"),
        ("{{ [machine] | map(\"attr\", \"os\") | join }}", "linux"),
        (
            "{{ [machine] | map(attribute=\"hostname\", default=\"h\") | join }}",
            "h",
        ),
        (
            "{{ [machine] | map(\"default\", \"d\") | map(\"length\") | join }}",
            "5",
        ),
        (
            "{% if work %}{{ [machine] | map(\"attr\", \"hostname\") }}{% endif %}done",
            "done",
        ),
    ]);
}

#[test]
fn every_result_in_a_block_header_chain_is_guarded_except_an_escapes_operand() {
    for source in [
        "{% filter first | pprint %}{% endfilter %}",
        "{% set captured | first | pprint %}{% endset %}{{ captured }}",
        "{% set captured | first %}{% endset %}done",
        "{% filter first %}{% endfilter %}",
        "{% filter first | pprint | default(\"empty\") %}{% endfilter %}",
    ] {
        fails_with(source, "`first` is undefined");
    }
    fails_with("{% filter first | default(typo) %}{% endfilter %}", "typo");
    renders(&[
        (
            "{% filter first | default(\"empty\") %}{% endfilter %}",
            "empty",
        ),
        (
            "{% filter first | d(\"empty\") | upper %}{% endfilter %}",
            "EMPTY",
        ),
        (
            "{% set captured | first | default(\"empty\") %}{% endset %}{{ captured }}",
            "empty",
        ),
        ("{% filter first | upper %}abc{% endfilter %}", "A"),
        ("{% filter upper | trim %} a {% endfilter %}", "A"),
        ("{% set captured | upper %}x{% endset %}{{ captured }}", "X"),
    ]);
}

#[test]
fn a_loop_of_unknown_length_has_no_length_to_read_or_pair() {
    let unknown = "{% for c in \"ab\" %}";
    match render(
        &format!("{unknown}{{{{ loop.length }}}}{{% endfor %}}"),
        &linux(),
    ) {
        Err(Error {
            problem: Problem::Missing(variable),
            ..
        }) => assert_eq!(variable, "loop.length"),
        other => panic!("an unknown loop length must be missing, got {other:?}"),
    }
    for pairs in ["items | list | length", "dictsort | length"] {
        fails_with(
            &format!("{unknown}{{{{ loop | {pairs} }}}}{{% endfor %}}"),
            "`loop` has no value for `length` in a loop of unknown length",
        );
    }
    renders(&[
        (
            "{% for x in [1, 2] %}{{ loop | dictsort | length }}{% endfor %}",
            "99",
        ),
        (
            "{{ {\"b\": 1, \"a\": 2} | dictsort | map(\"first\") | join }}",
            "ab",
        ),
        ("{{ {\"a\": 1} | items | map(\"last\") | join }}", "1"),
        (
            "{% for c in \"ab\" %}{{ loop.index }}{{ c }}{% endfor %}",
            "1a2b",
        ),
        (
            "{% for x in [1] | zip([2]) %}{{ loop.length }}{% endfor %}",
            "1",
        ),
    ]);
}

#[test]
fn loop_cycle_needs_a_value_instead_of_dividing_by_zero() {
    renders(&[(
        "{% for x in [1, 2, 3] %}{{ loop.cycle(\"a\", \"b\") }}{% endfor %}",
        "aba",
    )]);
    for source in [
        "{% for x in [1] %}\n{{ loop.cycle() }}{% endfor %}",
        "{% for x in [1] %}\n{{ loop.cycle(*[]) }}{% endfor %}",
    ] {
        match render(source, &linux()) {
            Err(Error {
                line: Some(2),
                problem: Problem::Render(message),
                ..
            }) => assert!(
                message.contains("loop.cycle() needs at least one value"),
                "{message}"
            ),
            other => panic!("{source:?} must be a template error, got {other:?}"),
        }
    }
    for other in [
        "{{ email.cycle() }}",
        "{{ email.cycle(\"a\") }}",
        "{{ {\"index0\": 0, \"depth0\": 0}.cycle() }}",
        "{% set ns = namespace(a=1) %}{{ ns.cycle() }}",
    ] {
        match render(other, &linux()) {
            Err(Error {
                problem: Problem::Render(message),
                ..
            }) => assert!(
                !message.contains("loop.cycle()"),
                "{other:?} is not a loop and keeps minijinja's own error: {message}"
            ),
            other_result => panic!("{other:?} has no cycle method, got {other_result:?}"),
        }
    }
    renders(&[(
        "{% for row in [[1, 2]] %}{% for x in row %}{{ loop.cycle(\"o\", \"e\") }}{% endfor %}{% endfor %}",
        "oe",
    )]);
}

#[test]
fn a_whole_loop_of_unknown_length_is_never_copied_stored_or_passed_on() {
    for body in [
        "{{ dict(loop) | pprint }}",
        "{{ dict(**loop) | pprint }}",
        "{{ namespace(loop) | items | length }}",
        "{{ namespace(**loop).index }}",
        "{{ dict(loop) | length }}",
        "{% set alias = loop %}{{ alias.index }}",
        "{% with alias = loop %}{{ alias.index }}{% endwith %}",
        "{{ [loop] | length }}",
        "{{ {\"l\": loop} | length }}",
        "{{ loop | pprint }}",
        "{{ loop | items | length }}",
        "{{ loop | dictsort | length }}",
        "{{ loop | list | length }}",
        "{{ loop is mapping }}",
        "{{ loop }}",
        "{{ \"%s\" | format(**loop) }}",
        "{{ range(1) | map(\"string\", **loop) | join }}",
        "{{ dict(*[loop]) | length }}",
        "{{ loop.cycle(loop) }}",
        "{{ loop.changed(loop) }}",
    ] {
        fails_with(
            &format!("{{% for c in \"ab\" %}}{body}{{% endfor %}}"),
            "`loop` has no value for `length` in a loop of unknown length",
        );
    }
    renders(&[
        (
            "{% for c in \"ab\" %}{{ loop.index }}{{ loop.first }}{{ loop.changed(c) }}{{ loop.cycle(\"x\", \"y\") }}{% endfor %}",
            "1TrueTruex2FalseTruey",
        ),
        (
            "{% for c in \"ab\" %}{{ loop.length | default(\"?\") }}{% endfor %}",
            "??",
        ),
        (
            "{% for c in \"ab\" %}{{ loop.length is defined }}{% endfor %}",
            "FalseFalse",
        ),
        (
            "{% for c in \"ab\" %}{{ loop[\"index\"] }}{{ loop.depth }}{% endfor %}",
            "1121",
        ),
        (
            "{% for c in \"ab\" %}{{ dict(i=loop.index).i }}{% endfor %}",
            "12",
        ),
        (
            "{% for c in \"ab\" %}{{ loop is defined }}{{ (loop | default(1)).index }}{% endfor %}",
            "True1True2",
        ),
        (
            "{% for x in [1, 2] %}{{ dict(loop) | length }}{% endfor %}",
            "99",
        ),
        (
            "{% for x in [1, 2] %}{{ dict(**loop).index }}{% endfor %}",
            "12",
        ),
        (
            "{% for x in [1, 2] %}{{ namespace(loop).revindex }}{% endfor %}",
            "21",
        ),
        (
            "{% for x in [1, 2] %}{{ namespace(**loop).length }}{% endfor %}",
            "22",
        ),
        (
            "{% for x in [1, 2] %}{{ loop | items | length }}{% endfor %}",
            "99",
        ),
        (
            "{% for x in [1, 2] %}{% set alias = loop %}{{ alias.revindex0 }}{% endfor %}",
            "10",
        ),
        (
            "{% for row in [[1], [2]] recursive %}{{ loop.depth }}{% if row is sequence %}{{ loop([]) }}{% endif %}{% endfor %}",
            "11",
        ),
        ("{{ {\"k\": (\"a\" if false)} | length }}", "1"),
    ]);
}

#[test]
fn only_the_real_loop_object_counts_as_a_loop() {
    let lookalike = "dict(index0=0, index=1, length=none, revindex=3, revindex0=4, first=5, last=6, depth=7, depth0=8)";
    renders(&[
        (&format!("{{{{ {lookalike} | length }}}}"), "9"),
        (&format!("{{{{ {lookalike} | items | length }}}}"), "9"),
        (
            &format!("{{% set ns = namespace({lookalike}) %}}{{{{ ns.depth0 }}}}"),
            "8",
        ),
        (
            "{{ ({\"index0\": 0} | chain({\"index\": 1}, {\"length\": 2}, {\"revindex\": 3}, {\"revindex0\": 4}, {\"first\": 5}, {\"last\": 6}, {\"depth\": 7}, {\"depth0\": 8})) | length }}",
            "9",
        ),
    ]);
    for receiver in [
        lookalike.to_owned(),
        "({\"index0\": 0} | chain({\"index\": 1}, {\"length\": 2}, {\"revindex\": 3}, {\"revindex0\": 4}, {\"first\": 5}, {\"last\": 6}, {\"depth\": 7}, {\"depth0\": 8}))".to_owned(),
    ] {
        match render(&format!("{{{{ {receiver}.cycle() }}}}"), &linux()) {
            Err(Error {
                problem: Problem::Render(message),
                ..
            }) => assert!(!message.contains("loop.cycle()"), "{receiver}: {message}"),
            other => panic!("a map has no cycle method, got {other:?}"),
        }
    }
    renders(&[(
        "{{ [1, 2] | zip([3, 4]) | map(\"first\") | join }}{{ range(2) | list | length }}",
        "122",
    )]);
}

fn cyclic_at(source: &str, line: usize, target: &str, owner: &str) {
    match render(source, &linux()) {
        Err(Error {
            line: Some(found),
            problem: Problem::Render(message),
            ..
        }) => {
            assert_eq!(found, line, "{source:?}: {message}");
            assert_eq!(
                message,
                format!("assigning to `{target}` would make `{owner}` contain itself"),
                "{source:?}"
            );
        }
        other => panic!("{source:?} must refuse the cyclic assignment, got {other:?}"),
    }
}

#[test]
fn an_assignment_that_would_make_a_namespace_contain_itself_is_refused_first() {
    let ns = "{% set ns = namespace(n=1) %}\n";
    for after in [
        "{{ ns | length }}",
        "{{ ns }}",
        "{{ ns | pprint }}",
        "{{ ns == ns }}",
        "{{ ns | items | length }}",
        "{{ ns | dictsort | length }}",
        "",
    ] {
        cyclic_at(
            &format!("{ns}{{% set ns.self = ns %}}{after}"),
            2,
            "ns.self",
            "ns",
        );
    }
    for (source, line, target, owner) in [
        (
            "{% set a = namespace() %}{% set b = namespace() %}{% set a.b = b %}\n{% set b.a = a %}{{ a }}",
            2,
            "b.a",
            "b",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.items = [{\"x\": [ns]}] %}",
            2,
            "ns.items",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.t = (1, (2, ns)) %}",
            2,
            "ns.t",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.k = {ns: 1} %}",
            2,
            "ns.k",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.z = [ns] | zip([1]) %}",
            2,
            "ns.z",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.c = [ns] | chain([1]) %}",
            2,
            "ns.c",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.g = [ns] | groupby(\"n\", default=1) %}",
            2,
            "ns.g",
            "ns",
        ),
        (
            "{% set ns = namespace() %}{% set alias = ns %}\n{% set ns.a = alias %}",
            2,
            "ns.a",
            "ns",
        ),
        (
            "{% set outer = namespace() %}{% set inner = namespace(o=outer) %}\n{% set outer.i = inner %}",
            2,
            "outer.i",
            "outer",
        ),
        (
            "{% set outer = namespace() %}{% set inner = namespace() %}{% set outer.i = inner %}\n{% set inner.back = {\"to\": [outer]} %}",
            2,
            "inner.back",
            "inner",
        ),
        (
            "{% set ns = namespace() %}{% set ns.a = 1 %}\n{% set ns.a = ns %}",
            2,
            "ns.a",
            "ns",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.a, ns.b = 1, ns %}",
            2,
            "ns.b",
            "ns",
        ),
        (
            "{% set ns = namespace(inner=namespace()) %}\n{% set ns.inner.up = ns %}",
            2,
            "ns.inner.up",
            "ns.inner",
        ),
        (
            "{% set ns = namespace() %}\n{% set ns.z | zip([ns]) %}ab{% endset %}",
            2,
            "ns.z",
            "ns",
        ),
        (
            "{% set ns = namespace() %}{% set d = dict(n=ns) %}\n{% set ns.copy = namespace(d) %}",
            2,
            "ns.copy",
            "ns",
        ),
    ] {
        cyclic_at(source, line, target, owner);
    }
}

#[test]
fn acyclic_namespaces_aliases_and_shared_values_still_assign() {
    renders(&[
        (
            "{% set inner = namespace(v=1) %}{% set outer = namespace(i=inner) %}{% set outer.list = [inner, {\"k\": inner}] %}{{ outer.i.v }}{{ outer.list | length }}",
            "12",
        ),
        (
            "{% set leaf = namespace(v=1) %}{% set a = namespace() %}{% set b = namespace() %}{% set a.l = leaf %}{% set b.l = leaf %}{% set a.b = b %}{{ a.b.l.v }}{{ a.l.v }}",
            "11",
        ),
        (
            "{% set ns = namespace(n=0) %}{% for i in range(4) %}{% set ns.n = ns.n + i %}{% endfor %}{{ ns.n }}",
            "6",
        ),
        (
            "{% set ns = namespace() %}{% for x in [1] %}{% set ns.c = loop.changed(1) %}{{ ns.c }}{{ loop.changed(1) }}{% endfor %}",
            "TrueFalse",
        ),
        (
            "{% set ns = namespace() %}{% set (ns.a, (x, ns.b)) = (1, (2, 3)) %}{{ ns.a }}{{ x }}{{ ns.b }}",
            "123",
        ),
        (
            "{% set ns = namespace() %}{% set ns.s %}text{% endset %}{{ ns.s }}",
            "text",
        ),
        (
            "{% set ns = namespace() %}{% set ns.s | upper %}text{% endset %}{{ ns.s }}",
            "TEXT",
        ),
        (
            "{% set ns = namespace(n=1) %}{% set ns.copy = dict(ns) %}{% set ns.merged = ns | chain({\"m\": 2}) %}{{ ns.copy | length }}{{ ns.merged | length }}{{ ns | length }}",
            "133",
        ),
        (
            "{% set ns = namespace(n=1) %}{% set view = ns | chain({}) %}{% set ns.n = 2 %}{{ view.n }}{{ ns.n }}",
            "12",
        ),
        (
            "{% set ns = namespace(inner=namespace()) %}{% set ns.inner.v = 5 %}{{ ns.inner.v }}",
            "5",
        ),
        (
            "{% set ns = namespace() %}{% set ns.r = range(3) %}{{ ns.r | list | length }}",
            "3",
        ),
        (
            "{% set m = {\"cycle\": dict} %}{{ m.cycle() | length }}",
            "0",
        ),
        ("{{ namespace(cycle=range).cycle(2) | list | length }}", "2"),
    ]);
    match render(
        "{% set nope = namespace() %}{% set nope.x = typo %}",
        &linux(),
    ) {
        Err(Error {
            problem: Problem::Missing(variable),
            ..
        }) => assert_eq!(
            variable, "typo",
            "the value is still evaluated before the receiver"
        ),
        other => panic!("a missing value must fail first, got {other:?}"),
    }
    assert!(render("{% set x = 1 %}{% set x.y = 2 %}", &linux()).is_err());
}

#[test]
fn messages_name_values_without_rendering_them() {
    let ns = "{% set ns = namespace(n=1) %}";
    fails_with(&format!("{ns}{{{{ [machine] | map(ns) | join }}}}"), "");
    for (source, needle) in [
        (
            format!("{ns}{{{{ [[]] | map(\"first\") | length }}}}"),
            "map(\"first\"): an item's result is undefined",
        ),
        (
            format!("{ns}{{{{ [ns] | map(attribute=\"missing\") | length }}}}"),
            "map(attribute=\"missing\")",
        ),
        (
            format!("{ns}{{{{ ns.cycle() }}}}"),
            "map has no method named cycle",
        ),
    ] {
        fails_with(&source, needle);
    }
}

#[test]
fn only_the_running_loop_is_checked_and_every_route_to_it_is_covered() {
    for body in [
        "{% set outer = loop %}",
        "{{ dict(loop | default(1)) | length }}",
        "{{ dict(loop | d(1)) | length }}",
        "{{ loop | default(0) is mapping }}",
        "{{ [loop if true else 1] | length }}",
        "{{ dict((loop, 1)[0]) | length }}",
        "{% for x in [1] %}{% endfor %}{{ dict(loop) | length }}",
        "{% with alias = loop %}{% endwith %}",
    ] {
        fails_with(&format!("{{% for c in \"ab\" %}}{body}{{% endfor %}}"), "`");
    }
    fails_with(
        "{% for c in \"ab\" recursive %}{{ dict(loop) | length }}{% endfor %}",
        "in a loop of unknown length",
    );
    renders(&[
        (
            "{% for c in \"ab\" %}{% for x in [1] %}{{ dict(loop) | length }}{% endfor %}{% endfor %}",
            "99",
        ),
        (
            "{% for x in [1, 2] %}{% set outer = loop %}{% for c in \"ab\" %}{{ dict(outer) | length }}{{ outer.index }}{% endfor %}{% endfor %}",
            "91919292",
        ),
        (
            "{% for c in \"ab\" %}{{ loop is defined }}{% endfor %}",
            "TrueTrue",
        ),
        (
            "{% for c in \"ab\" %}{{ (loop | default(0)).index }}{{ (loop if true else 0).first }}{% endfor %}",
            "1True2False",
        ),
        (
            "{% for row in [[[]], []] recursive %}{{ dict(loop) | length }}{{ loop(row) }}{% endfor %}",
            "999",
        ),
    ]);
    for reserved in [
        "{% for c in \"ab\" %}{% set loop = 1 %}{% endfor %}",
        "{% for loop in [1] %}{% endfor %}",
        "{% with loop = 1 %}{% endwith %}",
    ] {
        assert!(
            matches!(
                template::check(Path::new(FILE), reserved),
                Err(Error {
                    problem: Problem::Syntax(_),
                    ..
                })
            ),
            "{reserved} must not be able to shadow loop"
        );
    }
}

#[test]
fn a_set_statement_has_no_target_count_limit() {
    let targets: Vec<String> = (0..300).map(|index| format!("ns.a{index}")).collect();
    let source = format!(
        "{{% set ns = namespace() %}}{{% set {} = range(300) %}}{{{{ ns.a0 }}}}-{{{{ ns.a299 }}}}",
        targets.join(", ")
    );
    assert_eq!(render(&source, &linux()).unwrap(), "0-299");
    match render(
        "{% set ns = namespace() %}{% set ns.a, ns.b = [1] %}",
        &linux(),
    ) {
        Err(Error {
            problem: Problem::Render(message),
            ..
        }) => assert_eq!(message, "sequence of wrong length (expected 2, got 1)"),
        other => panic!("a short sequence keeps minijinja's unpack error, got {other:?}"),
    }
}

#[test]
fn each_target_list_is_unpacked_once_when_reached_as_minijinja_does() {
    renders(&[
        (
            "{% set ns = namespace(a=1, b=2) %}{% set ns.c, ns.d = ns %}{{ ns.c }}{{ ns.d }}{{ ns | length }}",
            "ab4",
        ),
        (
            "{% set ns = namespace(a=1) %}{% set ns.x, (ns.y, ns.z) = 1, ns %}{{ ns.y }}{{ ns.z }}",
            "ax",
        ),
        (
            "{% set ns = namespace(a=1, b=2) %}{% set x, ns.c, (y, ns.d) = [1, \"q\", ns] | list | first | string, \"r\", [3, 4] %}{{ x }}{{ ns.c }}{{ y }}{{ ns.d }}",
            "1r34",
        ),
        (
            "{% set ns = namespace() %}{% set (ns.a, ns.b), ns.c = (1, 2), 3 %}{{ ns.a }}{{ ns.b }}{{ ns.c }}",
            "123",
        ),
    ]);
    match render(
        "{% set ns = namespace() %}\n{% set ns.x, (ns.y, ns.z) = 1, ns %}",
        &linux(),
    ) {
        Err(Error {
            line: Some(2),
            problem: Problem::Render(message),
            ..
        }) => assert_eq!(
            message, "sequence of wrong length (expected 2, got 1)",
            "the nested list is unpacked after ns.x is stored, so it sees one key"
        ),
        other => panic!("a short nested list must keep minijinja's error, got {other:?}"),
    }
    match render(
        "{% set ns = namespace() %}{% set ns.a, ns.b = [1, 2, 3] %}",
        &linux(),
    ) {
        Err(Error {
            problem: Problem::Render(message),
            ..
        }) => assert_eq!(message, "sequence of wrong length (expected 2, got 3)"),
        other => panic!("a long list must keep minijinja's error, got {other:?}"),
    }
    match render(
        "{% set ns = namespace() %}{% set ns.a, ns.b = 7 %}",
        &linux(),
    ) {
        Err(Error {
            problem: Problem::Render(message),
            ..
        }) => assert_eq!(message, "value is not iterable"),
        other => panic!("a scalar must keep minijinja's error, got {other:?}"),
    }
}
