//! Line-based interactive prompts for `setup`, `hosts add/edit`, rules and
//! settings. They require a TTY; the non-interactive CLI uses flags instead.

use std::io::{self, BufRead, IsTerminal, Write};

pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn require_tty() -> Result<(), String> {
    if stdin_is_tty() {
        Ok(())
    } else {
        Err(
            "this command is interactive and needs a TTY; pass the explicit options instead"
                .to_string(),
        )
    }
}

pub fn line(label: &str, default: Option<&str>) -> Result<String, String> {
    require_tty()?;
    let mut stdout = io::stdout();
    match default {
        Some(default) if !default.is_empty() => {
            write!(stdout, "{label} [{default}]: ").map_err(|err| err.to_string())?;
        }
        _ => {
            write!(stdout, "{label}: ").map_err(|err| err.to_string())?;
        }
    }
    stdout.flush().map_err(|err| err.to_string())?;
    let mut input = String::new();
    io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|err| err.to_string())?;
    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        Ok(default.unwrap_or("").to_string())
    } else {
        Ok(trimmed)
    }
}

pub fn required(label: &str, default: Option<&str>) -> Result<String, String> {
    loop {
        let value = line(label, default)?;
        if !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
        eprintln!("a value is required");
    }
}

pub fn confirm(label: &str, default: bool) -> Result<bool, String> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let value = line(&format!("{label} [{hint}]"), Some(""))?;
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Ok(default);
        }
        match value.as_str() {
            "y" | "yes" | "да" | "1" | "on" => return Ok(true),
            "n" | "no" | "нет" | "0" | "off" => return Ok(false),
            _ => eprintln!("please answer yes or no"),
        }
    }
}

pub fn choose(label: &str, options: &[String], default: usize) -> Result<usize, String> {
    if options.is_empty() {
        return Err("nothing to choose from".to_string());
    }
    for (index, option) in options.iter().enumerate() {
        println!("  {}. {}", index + 1, option);
    }
    let fallback = (default + 1).to_string();
    loop {
        let value = line(label, Some(&fallback))?;
        match value.trim().parse::<usize>() {
            Ok(number) if number >= 1 && number <= options.len() => return Ok(number - 1),
            _ => eprintln!("choose 1..={}", options.len()),
        }
    }
}

pub fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "1" | "on" | "да" => Ok(true),
        "false" | "no" | "n" | "0" | "off" | "нет" => Ok(false),
        other => Err(format!("expected true/false, got {other:?}")),
    }
}

pub fn parse_optional_u64(value: &str) -> Result<Option<u64>, String> {
    let value = value.trim();
    if value.is_empty() || value == "-" || value.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    value.parse::<u64>().map(Some).map_err(|_| {
        format!("expected a non-negative integer or '-' for the default, got {value:?}")
    })
}

pub fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToString::to_string)
        .collect()
}
