//! RAII guard for terminal session cleanup.
//!
//! [`TerminalSessionGuard`] ensures the terminal is restored to a sane state
//! (leaving alternate screen, disabling raw mode, clearing mouse/keyboard
//! enhancements) when the TUI run loop exits — whether normally or via panic.
//!
//! Callers should create the guard immediately after entering raw mode and
//! the alternate screen, and call [`TerminalSessionGuard::disarm`] only after
//! an orderly shutdown has fully completed and the terminal has already been
//! restored. If the guard is dropped without being disarmed, it automatically
//! invokes [`restore_terminal_session`].

use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};

pub(super) struct TerminalSessionGuard {
    active: bool,
}

impl TerminalSessionGuard {
    pub(super) fn new() -> Self {
        Self { active: true }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalSessionGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }

        restore_terminal_session();
    }
}

pub(super) fn restore_terminal_session() {
    let mut stdout = io::stdout();
    let _ = restore_terminal_session_to(&mut stdout);
}

fn restore_terminal_session_to(stdout: &mut impl Write) -> io::Result<()> {
    let mut first_error = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .err();

    // Belt-and-suspenders reset for terminals that keep xterm mouse/focus
    // reporting enabled if the higher-level crossterm cleanup path is skipped
    // or partially applied.
    if let Err(err) = stdout.write_all(
        b"\x1b[<u\x1b[>4;0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l",
    ) {
        first_error.get_or_insert(err);
    }
    if let Err(err) = stdout.flush() {
        first_error.get_or_insert(err);
    }

    // Drain any queued mouse / CSI-u keyboard reports while raw mode is still
    // active so they cannot leak into the parent shell as literal text.
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        if event::read().is_err() {
            break;
        }
    }

    if let Err(err) = disable_raw_mode() {
        first_error.get_or_insert(err);
    }

    if let Some(err) = first_error {
        return Err(err);
    }
    Ok(())
}

pub(super) fn enter_dashboard_terminal_session(stdout: &mut impl Write) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(dashboard_keyboard_flags())
    )?;
    stdout.flush()
}

pub(super) fn reset_dashboard_terminal_session(stdout: &mut impl Write) -> io::Result<()> {
    restore_terminal_session_to(stdout)?;
    enter_dashboard_terminal_session(stdout)
}

fn dashboard_keyboard_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

pub(super) struct BrehonDashboardTerminalControl<W> {
    writer: W,
}

impl<W> BrehonDashboardTerminalControl<W>
where
    W: Write,
{
    pub(super) fn new(writer: W) -> Self {
        Self { writer }
    }
}

pub(super) struct BrehonDashboardRestoreToken {
    screen_policy: panesmith::AttachScreenPolicy,
}

impl<W> panesmith::PaneAttachTerminalControl for BrehonDashboardTerminalControl<W>
where
    W: Write,
{
    type Error = io::Error;
    type RestoreToken = BrehonDashboardRestoreToken;

    fn suspend_for_attach(
        &mut self,
        policy: panesmith::AttachScreenPolicy,
    ) -> Result<Self::RestoreToken, Self::Error> {
        self.writer.flush()?;
        enable_raw_mode()?;
        execute!(
            self.writer,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::empty()),
            EnableMouseCapture,
            cursor::Show
        )?;

        match policy {
            panesmith::AttachScreenPolicy::ReuseHostAlternateScreen => {}
            panesmith::AttachScreenPolicy::LeaveAlternateScreen => {
                execute!(self.writer, LeaveAlternateScreen)?;
            }
            panesmith::AttachScreenPolicy::EnterFreshAlternateScreen => {
                execute!(
                    self.writer,
                    EnterAlternateScreen,
                    Clear(ClearType::All),
                    cursor::MoveTo(0, 0)
                )?;
            }
        }

        self.writer.flush()?;
        Ok(BrehonDashboardRestoreToken {
            screen_policy: policy,
        })
    }

    fn restore_after_attach(&mut self, token: &mut Self::RestoreToken) -> Result<(), Self::Error> {
        match token.screen_policy {
            panesmith::AttachScreenPolicy::ReuseHostAlternateScreen => {}
            panesmith::AttachScreenPolicy::LeaveAlternateScreen => {
                execute!(self.writer, EnterAlternateScreen)?;
            }
            panesmith::AttachScreenPolicy::EnterFreshAlternateScreen => {
                execute!(self.writer, LeaveAlternateScreen)?;
            }
        }
        reset_dashboard_terminal_session(&mut self.writer)
    }
}
