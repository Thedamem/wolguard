//! Small, dependency-free styling and table rendering for the CLI.
//!
//! Colour is emitted only for a TTY that did not opt out. Box drawing falls
//! back to ASCII when the locale does not look UTF-8 or `WOLGUARD_ASCII=1`.

use std::io::IsTerminal;

#[derive(Clone, Copy, Debug)]
pub struct Style {
    color: bool,
    unicode: bool,
}

impl Style {
    pub fn detect() -> Style {
        let color = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM")
                .map(|term| term != "dumb")
                .unwrap_or(true)
            && std::env::var("CLICOLOR")
                .map(|value| value != "0")
                .unwrap_or(true);
        let unicode = if std::env::var("WOLGUARD_ASCII")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            false
        } else {
            let locale = std::env::var("LC_ALL")
                .or_else(|_| std::env::var("LC_CTYPE"))
                .or_else(|_| std::env::var("LANG"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            !(locale == "c" || locale == "posix")
        };
        Style { color, unicode }
    }

    #[cfg(test)]
    pub fn plain() -> Style {
        Style {
            color: false,
            unicode: true,
        }
    }

    pub fn with_ascii(mut self, ascii: bool) -> Style {
        if ascii {
            self.unicode = false;
        }
        self
    }

    pub fn paint(self, text: impl AsRef<str>, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{}\x1b[0m", text.as_ref())
        } else {
            text.as_ref().to_string()
        }
    }

    pub fn bold(self, text: impl AsRef<str>) -> String {
        self.paint(text, "1")
    }

    pub fn dim(self, text: impl AsRef<str>) -> String {
        self.paint(text, "2")
    }

    pub fn green(self, text: impl AsRef<str>) -> String {
        self.paint(text, "32")
    }

    pub fn red(self, text: impl AsRef<str>) -> String {
        self.paint(text, "31")
    }

    pub fn yellow(self, text: impl AsRef<str>) -> String {
        self.paint(text, "33")
    }

    pub fn cyan(self, text: impl AsRef<str>) -> String {
        self.paint(text, "36")
    }

    pub fn class(self, class: Option<crate::ipclass::IpClass>) -> String {
        match class {
            Some(crate::ipclass::IpClass::White) => self.green("white"),
            Some(crate::ipclass::IpClass::Grey) => self.yellow("grey"),
            None => self.dim("-"),
        }
    }

    pub fn table(self, headers: &[&str], rows: &[Vec<String>], right: &[bool]) -> String {
        if headers.is_empty() {
            return String::new();
        }
        let mut widths: Vec<usize> = headers.iter().map(|header| display_width(header)).collect();
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                if index < widths.len() {
                    widths[index] = widths[index].max(display_width(cell));
                }
            }
        }
        let mut output = String::new();
        let (tl, tm, tr, ml, mm, mr, bl, bm, br, h, v) = if self.unicode {
            ('┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘', '─', '│')
        } else {
            ('+', '+', '+', '+', '+', '+', '+', '+', '+', '-', '|')
        };
        let border = |left: char, middle: char, right_char: char| {
            let mut line = String::new();
            line.push(left);
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push(middle);
                }
                line.push_str(&h.to_string().repeat(width + 2));
            }
            line.push(right_char);
            line.push('\n');
            line
        };
        output.push_str(&border(tl, tm, tr));
        output.push_str(
            &self.table_row(
                &headers
                    .iter()
                    .map(|header| header.to_string())
                    .collect::<Vec<_>>(),
                &widths,
                right,
                v,
                true,
            ),
        );
        output.push_str(&border(ml, mm, mr));
        for row in rows {
            output.push_str(&self.table_row(row, &widths, right, v, false));
        }
        output.push_str(&border(bl, bm, br));
        output
    }

    fn table_row(
        self,
        cells: &[String],
        widths: &[usize],
        right: &[bool],
        vertical: char,
        header: bool,
    ) -> String {
        let mut line = String::new();
        line.push(vertical);
        for (index, width) in widths.iter().enumerate() {
            let cell = cells.get(index).map(String::as_str).unwrap_or("");
            let visible = display_width(cell);
            let padding = width.saturating_sub(visible);
            line.push(' ');
            if right.get(index).copied().unwrap_or(false) {
                line.push_str(&" ".repeat(padding));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&" ".repeat(padding));
            }
            line.push(' ');
            line.push(vertical);
        }
        line.push('\n');
        if header {
            let plain = strip_ansi(&line);
            if self.color {
                format!("\x1b[1m{plain}\x1b[0m")
            } else {
                plain
            }
        } else {
            line
        }
    }

    pub fn heading(self, text: &str) -> String {
        self.bold(self.cyan(text))
    }
}

pub fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

fn display_width(text: &str) -> usize {
    strip_ansi(text).chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_style_has_no_ansi() {
        let style = Style::plain();
        assert_eq!(style.bold("x"), "x");
        assert_eq!(style.class(Some(crate::ipclass::IpClass::White)), "white");
    }

    #[test]
    fn table_columns_align_with_unicode_and_ansi() {
        let style = Style::plain();
        let table = style.table(
            &["Name", "State"],
            &[
                vec!["server".to_string(), style.green("online")],
                vec!["кафе".to_string(), "offline".to_string()],
            ],
            &[false, false],
        );
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 6);
        assert_eq!(display_width(lines[0]), display_width(lines[3]));
        assert!(table.contains("server"));
        assert!(table.contains("кафе"));
    }

    #[test]
    fn ascii_fallback_uses_plus_and_dash() {
        let style = Style {
            color: false,
            unicode: false,
        };
        let table = style.table(&["a"], &[vec!["b".to_string()]], &[false]);
        assert!(table.contains('+'));
        assert!(!table.contains('┌'));
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
    }
}
