//! Windows: a Scheduled Task for the current user, `\Loadout Update`, registered from XML with
//! `schtasks /Create /XML`. It runs only while the user is logged on (an interactive token, so
//! no password is stored), with least privilege, and never two at once. It starts the GUI
//! launcher beside `loadout.exe`, so no console window opens at each run; the launcher runs the
//! same supervisor as `loadout update --background`.

use super::{Error, LAUNCHER, Scheduler, failed, query, required, xml};
use crate::context::Context;
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

fn not_running(answer: &crate::process::Captured) -> bool {
    let said = format!("{} {}", answer.stdout, answer.stderr);
    said.contains("not running") || said.contains("no running instance")
}

fn not_found(answer: &crate::process::Captured) -> bool {
    answer.stderr.contains("cannot find") || answer.stdout.contains("cannot find")
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

    #[test]
    fn the_definition_is_utf16_little_endian_with_a_byte_order_mark() {
        assert_eq!(utf16("<A>"), vec![0xFF, 0xFE, b'<', 0, b'A', 0, b'>', 0]);
    }
}
