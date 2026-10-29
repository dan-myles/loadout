use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// Plain ASCII on Windows, whose older consoles have no Braille glyphs.
const FRAMES: &[&str] = if cfg!(windows) {
    &["|", "/", "-", "\\"]
} else {
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
};
const TICK: Duration = Duration::from_millis(100);

/// A spinner on stderr naming what loadout is waiting on, until it is dropped, which erases it.
/// It draws only when stderr is a terminal, so piped and logged output never holds one.
pub struct Spinner(Option<(mpsc::Sender<()>, JoinHandle<()>)>);

impl Spinner {
    /// `shown` false draws nothing: a background run, or a wait a prompt may interrupt.
    pub fn start(shown: bool, label: impl Into<String>) -> Spinner {
        if !shown || !io::stderr().is_terminal() {
            return Spinner(None);
        }
        let label = label.into();
        let (stop, stopped) = mpsc::channel::<()>();
        let drawing = thread::spawn(move || {
            // Overwritten with spaces, not an ANSI erase, which a Windows console without VT
            // processing would print literally. The trailing space keeps apart a prompt that an
            // installer's own `sudo` writes after it, such as a Homebrew cask's.
            let width = label.chars().count() + 3;
            for frame in FRAMES.iter().cycle() {
                draw(&format!("\r{frame} {label} "));
                if stopped.recv_timeout(TICK) != Err(RecvTimeoutError::Timeout) {
                    break;
                }
            }
            draw(&format!("\r{:width$}\r", ""));
        });
        Spinner(Some((stop, drawing)))
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some((stop, drawing)) = self.0.take() {
            drop(stop);
            let _ = drawing.join();
        }
    }
}

/// A line on stderr in a terminal, for a wait a spinner would garble, such as one `sudo` may
/// prompt in.
pub fn note(shown: bool, line: &str) {
    if shown && io::stderr().is_terminal() {
        eprintln!("{line}");
    }
}

fn draw(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}
