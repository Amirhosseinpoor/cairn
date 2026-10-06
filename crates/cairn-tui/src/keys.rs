//! Key events, independent of any terminal library.
//!
//! The editor, the keybinding table and the tests all speak [`Key`]; only the
//! terminal loop translates from `crossterm`.

/// What was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Esc,
    F(u8),
}

/// A key with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    #[must_use]
    pub const fn new(code: Code) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    #[must_use]
    pub const fn char(c: char) -> Self {
        Self::new(Code::Char(c))
    }

    #[must_use]
    pub const fn ctrl(c: char) -> Self {
        Self {
            ctrl: true,
            ..Self::new(Code::Char(c))
        }
    }

    #[must_use]
    pub const fn alt(c: char) -> Self {
        Self {
            alt: true,
            ..Self::new(Code::Char(c))
        }
    }

    #[must_use]
    pub const fn with_ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }

    #[must_use]
    pub const fn with_alt(mut self) -> Self {
        self.alt = true;
        self
    }

    #[must_use]
    pub const fn with_shift(mut self) -> Self {
        self.shift = true;
        self
    }

    /// A plain printable character with no control or alt modifier.
    #[must_use]
    pub const fn printable(&self) -> Option<char> {
        match self.code {
            Code::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }
}

/// `"ctrl+shift+left"`-style spelling, as used in `keybindings.toml`.
///
/// # Errors
/// A message naming what could not be understood.
pub fn parse(spec: &str) -> Result<Key, String> {
    let lowered = spec.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("an empty key".to_string());
    }
    let mut key = Key::new(Code::Esc);
    let parts: Vec<&str> = lowered.split('+').collect();
    let Some((last, mods)) = parts.split_last() else {
        return Err(format!("`{spec}` is not a key"));
    };
    for m in mods {
        match *m {
            "ctrl" | "control" => key.ctrl = true,
            "alt" | "option" | "meta" => key.alt = true,
            "shift" => key.shift = true,
            other => return Err(format!("unknown modifier `{other}` in `{spec}`")),
        }
    }
    key.code = match *last {
        "enter" | "return" => Code::Enter,
        "backspace" => Code::Backspace,
        "delete" | "del" => Code::Delete,
        "left" => Code::Left,
        "right" => Code::Right,
        "up" => Code::Up,
        "down" => Code::Down,
        "home" => Code::Home,
        "end" => Code::End,
        "pageup" => Code::PageUp,
        "pagedown" => Code::PageDown,
        "tab" => Code::Tab,
        "esc" | "escape" => Code::Esc,
        "space" => Code::Char(' '),
        "plus" => Code::Char('+'),
        f if f.starts_with('f') && f.len() > 1 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = f[1..]
                .parse()
                .map_err(|_| format!("`{f}` is not a function key"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("`{f}` is not a function key"));
            }
            Code::F(n)
        }
        single if single.chars().count() == 1 => Code::Char(single.chars().next().unwrap_or(' ')),
        other => return Err(format!("unknown key `{other}` in `{spec}`")),
    };
    // Shift+Tab is its own key code.
    if key.code == Code::Tab && key.shift && !key.ctrl && !key.alt {
        key.code = Code::BackTab;
        key.shift = false;
    }
    Ok(key)
}

/// The inverse of [`parse`], for messages and the help overlay.
#[must_use]
pub fn show(key: &Key) -> String {
    let mut parts: Vec<String> = Vec::new();
    if key.ctrl {
        parts.push("ctrl".into());
    }
    if key.alt {
        parts.push("alt".into());
    }
    if key.shift || key.code == Code::BackTab {
        parts.push("shift".into());
    }
    parts.push(match key.code {
        Code::Char(' ') => "space".into(),
        Code::Char(c) => c.to_string(),
        Code::Enter => "enter".into(),
        Code::Backspace => "backspace".into(),
        Code::Delete => "delete".into(),
        Code::Left => "left".into(),
        Code::Right => "right".into(),
        Code::Up => "up".into(),
        Code::Down => "down".into(),
        Code::Home => "home".into(),
        Code::End => "end".into(),
        Code::PageUp => "pageup".into(),
        Code::PageDown => "pagedown".into(),
        Code::Tab | Code::BackTab => "tab".into(),
        Code::Esc => "esc".into(),
        Code::F(n) => format!("f{n}"),
    });
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_their_spelling() {
        for spec in [
            "ctrl+a",
            "alt+b",
            "ctrl+left",
            "shift+enter",
            "enter",
            "esc",
            "f1",
            "ctrl+alt+x",
            "space",
            "pageup",
        ] {
            let key = parse(spec).unwrap();
            assert_eq!(show(&key), spec, "{spec}");
        }
        assert_eq!(parse("Shift+Tab").unwrap().code, Code::BackTab);
        assert_eq!(show(&parse("shift+tab").unwrap()), "shift+tab");
        assert_eq!(parse("CTRL+Q").unwrap(), Key::ctrl('q'));
    }

    #[test]
    fn nonsense_is_rejected_with_a_reason() {
        assert!(parse("").is_err());
        assert!(parse("hyper+x").unwrap_err().contains("hyper"));
        assert!(parse("ctrl+banana").unwrap_err().contains("banana"));
        assert!(parse("f13").is_err());
    }

    #[test]
    fn printable_means_no_control_or_alt() {
        assert_eq!(Key::char('x').printable(), Some('x'));
        assert_eq!(Key::ctrl('x').printable(), None);
        assert_eq!(Key::alt('x').printable(), None);
        assert_eq!(Key::new(Code::Enter).printable(), None);
        assert_eq!(Key::char('X').with_shift().printable(), Some('X'));
    }
}
