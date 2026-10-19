use std::{
    io::{self, Stdout, Write as _},
    process::{Command, Stdio},
};

use anyhow::Result;
use crossterm::{
    clipboard::CopyToClipboard,
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};

pub type KuxTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalGuard {
    terminal: KuxTerminal,
    #[cfg(unix)]
    foreground_interrupt: Option<tokio::signal::unix::Signal>,
}

impl TerminalGuard {
    /// Enters raw mode and the terminal's alternate screen.
    ///
    /// # Errors
    ///
    /// Returns an error when raw mode, terminal commands, or backend initialization fails.
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self {
            terminal,
            #[cfg(unix)]
            foreground_interrupt: None,
        })
    }

    pub const fn terminal_mut(&mut self) -> &mut KuxTerminal {
        &mut self.terminal
    }

    /// Restores the host terminal while an interactive child process runs.
    ///
    /// # Errors
    ///
    /// Returns an error when interrupt handling, raw mode, or terminal commands cannot be updated.
    pub fn suspend(&mut self) -> Result<()> {
        // Register before restoring canonical terminal input: Ctrl-C is delivered
        // to both Kux and its child in their shared foreground process group.
        #[cfg(unix)]
        if self.foreground_interrupt.is_none() {
            self.foreground_interrupt = Some(register_foreground_interrupt()?);
        }
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        )?;
        Ok(())
    }

    /// Clears the host screen and renders identity for an interactive child session.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal cannot be cleared or written.
    pub fn show_interactive_banner(&mut self, banner: &str) -> Result<()> {
        execute!(
            self.terminal.backend_mut(),
            Clear(ClearType::All),
            cursor::MoveTo(0, 0),
            SetBackgroundColor(Color::Green),
            SetForegroundColor(Color::Black),
            SetAttribute(Attribute::Bold),
            Print(banner),
            SetAttribute(Attribute::Reset),
            Print("\r\n")
        )?;
        Ok(())
    }

    /// Re-enters the Kux terminal after an interactive child process exits.
    ///
    /// # Errors
    ///
    /// Returns an error when raw mode, terminal commands, or screen clearing fails.
    pub fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;
        force_full_redraw(&mut self.terminal)?;
        #[cfg(unix)]
        {
            self.foreground_interrupt = None;
        }
        Ok(())
    }

    /// Copies text through the host clipboard when available, falling back to
    /// the terminal's OSC 52 clipboard escape sequence.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal command cannot be written.
    pub fn copy_to_clipboard(&mut self, value: &str) -> Result<()> {
        if copy_to_native_clipboard(value) {
            return Ok(());
        }
        execute!(
            self.terminal.backend_mut(),
            CopyToClipboard::to_clipboard_from(value)
        )?;
        Ok(())
    }
}

#[cfg(unix)]
fn register_foreground_interrupt() -> io::Result<tokio::signal::unix::Signal> {
    tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}

fn force_full_redraw<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let area = terminal.size()?.into();
    terminal.resize(area)
}

#[cfg(target_os = "macos")]
fn copy_to_native_clipboard(value: &str) -> bool {
    copy_with_command("pbcopy", &[], value)
}

#[cfg(target_os = "linux")]
fn copy_to_native_clipboard(value: &str) -> bool {
    copy_with_command("wl-copy", &[], value)
        || copy_with_command("xclip", &["-selection", "clipboard"], value)
        || copy_with_command("xsel", &["--clipboard", "--input"], value)
        || copy_with_command("clip.exe", &[], value)
}

#[cfg(target_os = "windows")]
fn copy_to_native_clipboard(value: &str) -> bool {
    copy_with_command("clip.exe", &[], value)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const fn copy_to_native_clipboard(_value: &str) -> bool {
    false
}

fn copy_with_command(program: &str, args: &[&str], value: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(value.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|status| status.success())
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
        buffer::Buffer,
    };

    use super::force_full_redraw;

    #[test]
    fn force_full_redraw_resets_ratatui_back_buffer() {
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget("abc", frame.area()))
            .expect("first draw");

        terminal.backend_mut().clear().expect("external clear");
        force_full_redraw(&mut terminal).expect("reset frame state");
        terminal
            .draw(|frame| frame.render_widget("abc", frame.area()))
            .expect("redraw");

        assert_eq!(terminal.backend().buffer(), &Buffer::with_lines(["abc"]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_interrupt_reaches_child_without_terminating_kux() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

        const CHILD_MARKER: &str = "KUX_TEST_FOREGROUND_INTERRUPT_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let _interrupt = super::register_foreground_interrupt().expect("register SIGINT");
            let status = tokio::process::Command::new("/bin/sh")
                .args(["-c", "kill -INT 0; exit 17"])
                .status()
                .await
                .expect("start foreground child");
            assert_eq!(status.signal(), Some(libc::SIGINT));
            return;
        }

        // Isolate signal registration and group delivery from the test runner.
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "terminal::tests::foreground_interrupt_reaches_child_without_terminating_kux",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .process_group(0)
            .output()
            .expect("run isolated SIGINT test");
        assert!(
            output.status.success(),
            "isolated interrupt test failed: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
