//! Windows: a Scheduled Task for the current user, `\Loadout Update`, registered from XML with
//! `schtasks /Create /XML`. It runs only while the user is logged on (an interactive token, so
//! no password is stored), with least privilege, and never two at once. It starts the GUI
//! launcher beside `loadout.exe`, so no console window opens at each run; the launcher runs the
//! same supervisor as `loadout update --background`.

use super::{Error, LAUNCHER, Scheduler, failed, query, query_output, required, xml};
use crate::context::Context;
use crate::process::Captured;
use crate::state::{self, Private};

const NAME: &str = "Loadout Update";
/// The supervisor's 60 minute hard limit plus its settle; a backstop only.
const BACKSTOP: &str = "PT1H2M";
/// A fixed start in the past: the repetition runs from it, so the definition is the same on
/// every install, and `StartWhenAvailable` makes up a run missed while the machine was off.
const START: &str = "2000-01-01T00:00:00";

/// One argument as `CommandLineToArgvW` reads it back: quoted when it holds a space, a tab, or a
/// quote, or is empty; backslashes doubled where they precede a quote or the closing quote.
fn argv_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for character in arg.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn task_xml(launcher: &str, service: &str, interval_minutes: u32) -> String {
    let arguments = format!("--service {}", argv_quote(service));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
         \x20 <RegistrationInfo>\r\n\
         \x20   <Description>loadout background update</Description>\r\n\
         \x20 </RegistrationInfo>\r\n\
         \x20 <Triggers>\r\n\
         \x20   <TimeTrigger>\r\n\
         \x20     <Repetition>\r\n\
         \x20       <Interval>PT{interval_minutes}M</Interval>\r\n\
         \x20       <StopAtDurationEnd>false</StopAtDurationEnd>\r\n\
         \x20     </Repetition>\r\n\
         \x20     <StartBoundary>{START}</StartBoundary>\r\n\
         \x20     <Enabled>true</Enabled>\r\n\
         \x20   </TimeTrigger>\r\n\
         \x20 </Triggers>\r\n\
         \x20 <Principals>\r\n\
         \x20   <Principal id=\"Author\">\r\n\
         \x20     <LogonType>InteractiveToken</LogonType>\r\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\r\n\
         \x20   </Principal>\r\n\
         \x20 </Principals>\r\n\
         \x20 <Settings>\r\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n\
         \x20   <AllowHardTerminate>true</AllowHardTerminate>\r\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\r\n\
         \x20   <Hidden>true</Hidden>\r\n\
         \x20   <ExecutionTimeLimit>{BACKSTOP}</ExecutionTimeLimit>\r\n\
         \x20   <Enabled>true</Enabled>\r\n\
         \x20 </Settings>\r\n\
         \x20 <Actions Context=\"Author\">\r\n\
         \x20   <Exec>\r\n\
         \x20     <Command>{}</Command>\r\n\
         \x20     <Arguments>{}</Arguments>\r\n\
         \x20   </Exec>\r\n\
         \x20 </Actions>\r\n\
         </Task>\r\n",
        xml(launcher),
        xml(&arguments),
    )
}

/// The task definition as `schtasks` reads it: UTF-16LE with a byte order mark, matching the
/// encoding its XML declaration names.
fn utf16(text: &str) -> Vec<u8> {
    [0xFF, 0xFE]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect()
}

pub(super) fn install(
    context: &Context,
    launcher: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<(), Error> {
    debug_assert!(launcher.ends_with(&format!("{LAUNCHER}.exe")));
    let staged = state::write_private(
        context,
        Private::TaskXml,
        &utf16(&task_xml(launcher, service, interval_minutes)),
    )?;
    let staged_text = super::safe(&staged)?.to_owned();
    let created = required(
        context,
        "schtasks",
        &["/Create", "/TN", NAME, "/XML", &staged_text, "/F"],
    );
    state::remove_private(context, Private::TaskXml)?;
    created.map(|_| ())
}

/// Ends and deletes the task. `/Query` must find it, or say it doesn't exist (`cannot find`);
/// any other failure is an error, and so is a failed `/End` (other than "not running") or
/// `/Delete`, leaving `service.toml` in place.
///
/// The not-found text is English; on a Windows that answers in another language, a missing task
/// is reported as an error with schtasks' own words rather than assumed gone.
pub(super) fn uninstall(context: &Context) -> Result<(), Error> {
    let args = ["/Query", "/TN", NAME];
    let found = query(context, "schtasks", &args)?;
    if !found.success() {
        if not_found(&found) {
            return Ok(());
        }
        return Err(failed("schtasks", &args, &found));
    }
    let end = ["/End", "/TN", NAME];
    let ended = query(context, "schtasks", &end)?;
    if !ended.success() && !not_running(&ended) {
        return Err(failed("schtasks", &end, &ended));
    }
    required(context, "schtasks", &["/Delete", "/TN", NAME, "/F"])?;
    Ok(())
}

fn not_running(answer: &Captured) -> bool {
    let said = format!("{} {}", answer.stdout, answer.stderr);
    said.contains("not running") || said.contains("no running instance")
}

fn not_found(answer: &Captured) -> bool {
    answer.stderr.contains("cannot find") || answer.stdout.contains("cannot find")
}

/// How the registered task differs from the one `install` would register for this record, if
/// it does: its command, arguments, and interval, read back from Task Scheduler and compared as
/// values. A task Task Scheduler can't find when its definition is read is a mismatch, even if
/// an earlier query found it; one whose definition can't be read back or decoded is an error.
/// Neither is ever a match.
pub(super) fn differs(
    context: &Context,
    launcher: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<Option<String>, String> {
    let args = ["/Query", "/TN", NAME, "/XML"];
    let answer = query_output(context, "schtasks", &args).map_err(|error| error.to_string())?;
    if answer.code != Some(0) {
        let answer = Captured::from(answer);
        if not_found(&answer) {
            return Ok(Some(format!(
                "Task Scheduler has no task {NAME} to read back"
            )));
        }
        return Err(failed("schtasks", &args, &answer).to_string());
    }
    compare(&answer.stdout, launcher, service, interval_minutes)
}

/// The task definition `schtasks /Query /XML` printed, against the one this record would
/// register.
fn compare(
    bytes: &[u8],
    launcher: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<Option<String>, String> {
    let text = decode(bytes)?;
    let arguments = format!("--service {}", argv_quote(service));
    let same = element(&text, "Command")?.as_deref() == Some(launcher)
        && element(&text, "Arguments")?.as_deref() == Some(arguments.as_str())
        && element(&text, "Interval")?
            .as_deref()
            .and_then(seconds)
            .is_some_and(|seconds| seconds == u64::from(interval_minutes) * 60);
    Ok((!same).then(|| super::differs_from_record(&format!("the task {NAME}"))))
}

/// The task XML as text: UTF-16 (little- or big-endian, by its byte order mark, or without one
/// when the second or first byte of its opening `<` is 0) or UTF-8, with or without a byte order
/// mark. Nothing is replaced or dropped: anything else is an error.
fn decode(bytes: &[u8]) -> Result<String, String> {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => from_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => from_utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => from_utf8(rest),
        [first, 0, ..] if *first != 0 => from_utf16(bytes, u16::from_le_bytes),
        [0, second, ..] if *second != 0 => from_utf16(bytes, u16::from_be_bytes),
        _ => from_utf8(bytes),
    }
}

fn from_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, String> {
    let (pairs, odd) = bytes.as_chunks::<2>();
    if !odd.is_empty() {
        return Err("the task's XML is UTF-16 with an odd number of bytes".to_owned());
    }
    let units: Vec<u16> = pairs.iter().copied().map(unit).collect();
    String::from_utf16(&units).map_err(|_| "the task's XML is not valid UTF-16".to_owned())
}

fn from_utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "the task's XML is not valid UTF-8".to_owned())
}

/// The value of the one `<tag>` element, with its XML references resolved; `None` when there is
/// none or more than one, which is not the task `install` registers.
fn element(text: &str, tag: &str) -> Result<Option<String>, String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = text.match_indices(&open);
    let (Some((start, _)), None) = (found.next(), found.next()) else {
        return Ok(None);
    };
    let start = start + open.len();
    let Some(end) = text[start..].find(&close) else {
        return Ok(None);
    };
    unescape(&text[start..start + end]).map(Some)
}

/// XML text with its five named references and its numeric character references resolved.
fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let semicolon = after
            .find(';')
            .ok_or_else(|| "the task's XML has an unterminated reference".to_owned())?;
        let name = &after[..semicolon];
        let character = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        out.push(
            character.ok_or_else(|| format!("the task's XML has an unknown reference &{name};"))?,
        );
        rest = &after[semicolon + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// An `xs:duration` as Task Scheduler writes an interval (`PT15M`, `PT1H`, `P1DT2H`), in
/// seconds; `None` for anything else.
fn seconds(duration: &str) -> Option<u64> {
    let rest = duration.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total: u64 = 0;
    for (mut part, units) in [
        (date, &[('D', 86_400)][..]),
        (time, &[('H', 3_600), ('M', 60), ('S', 1)][..]),
    ] {
        for &(unit, scale) in units {
            if let Some((number, after)) = part.split_once(unit) {
                let value: u64 = number.parse().ok()?;
                total = total.checked_add(value.checked_mul(scale)?)?;
                part = after;
            }
        }
        if !part.is_empty() {
            return None;
        }
    }
    Some(total)
}

pub(super) fn registered(context: &Context) -> Scheduler {
    match query(context, "schtasks", &["/Query", "/TN", NAME]) {
        Ok(answer) => Scheduler {
            registered: answer.success(),
            detail: if answer.success() {
                format!("task {NAME} is registered")
            } else {
                format!("there is no task {NAME}")
            },
        },
        Err(error) => Scheduler {
            registered: false,
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_read_back_exactly_as_commandlinetoargvw_splits_them() {
        assert_eq!(
            argv_quote(r"C:\state\service.toml"),
            r"C:\state\service.toml"
        );
        assert_eq!(
            argv_quote(r"C:\Users\A B\AppData\Local\loadout\service.toml"),
            r#""C:\Users\A B\AppData\Local\loadout\service.toml""#
        );
        assert_eq!(argv_quote(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(argv_quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(argv_quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(argv_quote(""), r#""""#);
    }

    #[test]
    fn the_task_starts_the_launcher_hidden_with_its_backstop_and_no_overlap() {
        let xml = task_xml(
            r"C:\Users\A&B\AppData\Local\Programs\loadout\loadout-background.exe",
            r"C:\Users\A&B\AppData\Local\loadout\service.toml",
            60,
        );
        assert!(xml.contains(
            "<Command>C:\\Users\\A&amp;B\\AppData\\Local\\Programs\\loadout\\loadout-background.exe</Command>"
        ), "{xml}");
        assert!(xml.contains(
            "<Arguments>--service C:\\Users\\A&amp;B\\AppData\\Local\\loadout\\service.toml</Arguments>"
        ), "{xml}");
        for line in [
            "<Interval>PT60M</Interval>",
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<ExecutionTimeLimit>PT1H2M</ExecutionTimeLimit>",
            "<Hidden>true</Hidden>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
        ] {
            assert!(xml.contains(line), "{line}: {xml}");
        }
        assert!(
            !xml.contains("<Password>") && !xml.contains("<UserId>"),
            "{xml}"
        );
    }

    #[test]
    fn a_path_with_a_space_is_quoted_inside_escaped_xml() {
        let xml = task_xml(r"C:\x\loadout-background.exe", r"C:\A B\service.toml", 15);
        assert!(
            xml.contains("<Arguments>--service &quot;C:\\A B\\service.toml&quot;</Arguments>"),
            "{xml}"
        );
    }

    const UNICODE_LAUNCHER: &str =
        r"C:\Users\José & Co\AppData\Local\Programs\loadout\loadout-background.exe";
    const UNICODE_SERVICE: &str = r"C:\Users\José & Co\AppData\Local\loadout\service.toml";

    fn unicode_task() -> String {
        task_xml(UNICODE_LAUNCHER, UNICODE_SERVICE, 60)
    }

    fn checked(bytes: &[u8]) -> Result<Option<String>, String> {
        compare(bytes, UNICODE_LAUNCHER, UNICODE_SERVICE, 60)
    }

    /// The task read back the way schtasks may print it, a user name that isn't ASCII, a space
    /// and an `&` included: UTF-16 with or without a byte order mark, and UTF-8 with or without
    /// one, all match.
    #[test]
    fn a_unicode_task_matches_in_every_encoding_schtasks_may_print() {
        let text = unicode_task();
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let utf8_bom = [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat();
        let be_bom = [&[0xFE, 0xFF][..], &be].concat();
        for bytes in [
            utf16(&text),
            le,
            be_bom,
            be,
            utf8_bom,
            text.clone().into_bytes(),
        ] {
            assert_eq!(checked(&bytes), Ok(None));
        }
    }

    /// Values are compared, not their XML spelling: a quote written literally or as a numeric
    /// reference, and the interval written in hours, are the same task.
    #[test]
    fn equivalent_xml_spellings_are_the_same_task() {
        let text = unicode_task();
        assert!(text.contains("&quot;"), "{text}");
        for spelling in [
            text.replace("&quot;", "\""),
            text.replace("&quot;", "&#34;"),
            text.replace("&quot;", "&#x22;"),
            text.replace("&amp;", "&#38;"),
            text.replace("<Interval>PT60M</Interval>", "<Interval>PT1H</Interval>"),
        ] {
            assert_eq!(checked(spelling.as_bytes()), Ok(None), "{spelling}");
        }
    }

    #[test]
    fn another_command_argument_or_interval_is_a_mismatch() {
        let text = unicode_task();
        for changed in [
            task_xml(r"C:\Other\loadout-background.exe", UNICODE_SERVICE, 60),
            task_xml(UNICODE_LAUNCHER, r"C:\Users\Jos\service.toml", 60),
            task_xml(UNICODE_LAUNCHER, UNICODE_SERVICE, 30),
            text.replace("<Arguments>", "<Arguments>--verbose "),
            text.replace(
                "<Interval>PT60M</Interval>",
                "<Interval>PT1M60S</Interval>x",
            ),
            text.replace("<Command>", "<Command>C:"),
            text.replace("</Exec>", "</Exec><Exec><Command>x</Command></Exec>"),
            String::new(),
        ] {
            let found = checked(&utf16(&changed));
            assert!(matches!(found, Ok(Some(_))), "{found:?}: {changed}");
        }
    }

    /// Output that can't be decoded exactly is an error, never a match or a guess.
    #[test]
    fn malformed_task_output_is_an_error() {
        let mut odd = utf16(&unicode_task());
        odd.push(b'>');
        let mut unpaired = vec![0xFF, 0xFE];
        unpaired.extend(utf16("<Task>")[2..].iter().copied());
        unpaired.extend([0x00, 0xD8]);
        let latin1: Vec<u8> = unicode_task()
            .chars()
            .map(|character| u8::try_from(u32::from(character)).unwrap())
            .collect();
        let bad_reference = unicode_task().replace("&amp;", "&ampersand;");
        for bytes in [odd, unpaired, latin1, bad_reference.into_bytes()] {
            assert!(checked(&bytes).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn intervals_are_read_as_durations() {
        assert_eq!(seconds("PT15M"), Some(900));
        assert_eq!(seconds("PT1H"), Some(3_600));
        assert_eq!(seconds("P1DT2H3M4S"), Some(93_784));
        for bad in ["15M", "PT", "PTxM", "P1M", "PT1H2", "PT1M1H"] {
            assert_eq!(seconds(bad).filter(|seconds| *seconds > 0), None, "{bad}");
        }
    }

    #[test]
    fn the_definition_is_utf16_little_endian_with_a_byte_order_mark() {
        assert_eq!(utf16("<A>"), vec![0xFF, 0xFE, b'<', 0, b'A', 0, b'>', 0]);
    }
}
