//! Terminal plumbing for routers whose `/dev/tty` cannot report a window size.
//!
//! crossterm asks `/dev/tty` for `TIOCGWINSZ` before falling back to stdout. On
//! some router firmwares that node exists but does not implement the ioctl
//! (ENOTTY), even though stdin/stdout are real TTYs. crossterm then fails with
//! "Not a tty (os error 25)" before the dashboard can start, which is exactly
//! what happens on Keenetic devices.
//!
//! [`RouterBackend`] delegates drawing to crossterm but resolves the size from
//! stdout first, then stdin, then `COLUMNS`/`LINES`, and finally falls back to
//! 80x24 instead of failing.

use std::io::{self, Stdout};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Size assumed when no TTY (and no COLUMNS/LINES) reports a usable size.
const FALLBACK: Size = Size::new(80, 24);
/// Reject obviously bogus COLUMNS/LINES values.
const MIN_COLUMNS: u16 = 20;
const MIN_LINES: u16 = 5;

/// Crossterm backend with a router-proof terminal size.
pub struct RouterBackend {
    inner: CrosstermBackend<Stdout>,
}

impl RouterBackend {
    pub fn new(stdout: Stdout) -> Self {
        Self {
            inner: CrosstermBackend::new(stdout),
        }
    }
}

impl Backend for RouterBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        Ok(terminal_size())
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: terminal_size(),
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Best effort terminal size; never fails, never returns a zero size.
pub fn terminal_size() -> Size {
    if let Some(size) = ioctl_size(libc::STDOUT_FILENO) {
        return size;
    }
    if let Some(size) = ioctl_size(libc::STDIN_FILENO) {
        return size;
    }
    if let Some(size) = env_size() {
        return size;
    }
    FALLBACK
}

fn ioctl_size(fd: libc::c_int) -> Option<Size> {
    let mut window: libc::winsize = unsafe { std::mem::zeroed() };
    // libc::Ioctl is c_ulong on linux-gnu and c_int on linux-musl (the router).
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as libc::Ioctl, &mut window) };
    if result == 0 && window.ws_col >= MIN_COLUMNS && window.ws_row >= MIN_LINES {
        Some(Size::new(window.ws_col, window.ws_row))
    } else {
        None
    }
}

fn env_size() -> Option<Size> {
    let columns = std::env::var("COLUMNS").ok()?;
    let lines = std::env::var("LINES").ok()?;
    parse_env_size(&columns, &lines)
}

fn parse_env_size(columns: &str, lines: &str) -> Option<Size> {
    let width: u16 = columns.trim().parse().ok()?;
    let height: u16 = lines.trim().parse().ok()?;
    if width >= MIN_COLUMNS && height >= MIN_LINES {
        Some(Size::new(width, height))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn parses_reasonable_env_size() {
        assert_eq!(parse_env_size("120", "30"), Some(Size::new(120, 30)));
        assert_eq!(parse_env_size(" 90 ", " 24 "), Some(Size::new(90, 24)));
    }

    #[test]
    fn rejects_bad_env_size() {
        assert_eq!(parse_env_size("wide", "30"), None);
        assert_eq!(parse_env_size("120", ""), None);
        assert_eq!(parse_env_size("0", "0"), None);
        assert_eq!(parse_env_size("10", "2"), None);
    }

    #[test]
    fn ioctl_size_rejects_non_tty_descriptors() {
        let null = File::open("/dev/null").expect("/dev/null must exist");
        assert_eq!(ioctl_size(null.as_raw_fd()), None);
    }

    #[test]
    fn terminal_size_is_never_zero() {
        let size = terminal_size();
        assert!(size.width >= MIN_COLUMNS, "width {}", size.width);
        assert!(size.height >= MIN_LINES, "height {}", size.height);
    }
}
