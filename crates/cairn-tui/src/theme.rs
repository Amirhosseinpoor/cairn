//! Colours, themes and glyphs (SPEC §10.5, §10.7, §10.8).
//!
//! A theme is a palette of named roles. What a role looks like depends on the
//! terminal: true colour as written, the nearest of 256 colours (CIE76
//! distance in Lab space), the fixed semantic mapping for 16 colours, or
//! nothing at all (`NO_COLOR`, `TERM=dumb`, a pipe). Statuses always carry a
//! symbol as well, so colour is never the only signal (REQ-TUI-009).

use std::collections::BTreeMap;

use ratatui::style::{Color, Style};

/// A semantic colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Bg,
    Fg,
    Dim,
    Accent,
    Red,
    Amber,
    Green,
    Blue,
}

impl Role {
    pub const ALL: [Self; 8] = [
        Self::Bg,
        Self::Fg,
        Self::Dim,
        Self::Accent,
        Self::Red,
        Self::Amber,
        Self::Green,
        Self::Blue,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bg => "bg",
            Self::Fg => "fg",
            Self::Dim => "dim",
            Self::Accent => "accent",
            Self::Red => "red",
            Self::Amber => "amber",
            Self::Green => "green",
            Self::Blue => "blue",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// # Errors
    /// A message when `text` is not `#rrggbb`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix('#')
            .ok_or_else(|| format!("`{text}` is not #rrggbb"))?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("`{text}` is not #rrggbb"));
        }
        let n = u32::from_str_radix(hex, 16).map_err(|_| format!("`{text}` is not #rrggbb"))?;
        Ok(Self(
            u8::try_from(n >> 16).unwrap_or(0),
            u8::try_from((n >> 8) & 0xff).unwrap_or(0),
            u8::try_from(n & 0xff).unwrap_or(0),
        ))
    }
}

/// How many colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    True,
    Ansi256,
    Ansi16,
    /// No colour at all: `NO_COLOR`, `TERM=dumb`, not a terminal.
    None,
}

impl ColorSupport {
    /// §10.7, from the environment. `env` looks a variable up; `is_tty` says
    /// whether stdout is a terminal.
    #[must_use]
    pub fn detect(env: &dyn Fn(&str) -> Option<String>, is_tty: bool) -> Self {
        let term = env("TERM").unwrap_or_default();
        let forced = env("CLICOLOR_FORCE").is_some_and(|v| v != "0" && !v.is_empty());
        if env("NO_COLOR").is_some() || term == "dumb" {
            return Self::None;
        }
        if !is_tty && !forced {
            return Self::None;
        }
        let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" {
            Self::True
        } else if term.contains("256color") || term.contains("kitty") || term == "alacritty" {
            Self::Ansi256
        } else {
            Self::Ansi16
        }
    }
}

/// A palette plus the mappings that sit on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    palette: BTreeMap<Role, Rgb>,
    /// `syntax` token class → role.
    pub syntax: BTreeMap<String, Role>,
    /// Mode name → role (`plan` → blue …).
    pub modes: BTreeMap<String, Role>,
}

/// A theme file that could not be used (`E-CFG-THEME`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("E-CFG-THEME: {0}")]
pub struct ThemeError(pub String);

const SYNTAX_KEYS: [&str; 5] = ["keyword", "string", "number", "comment", "function"];
const MODE_KEYS: [&str; 4] = ["plan", "build", "auto", "auto_unsafe"];

fn default_maps() -> (BTreeMap<String, Role>, BTreeMap<String, Role>) {
    let syntax = [
        ("keyword", Role::Accent),
        ("string", Role::Green),
        ("number", Role::Amber),
        ("comment", Role::Dim),
        ("function", Role::Blue),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let modes = [
        ("plan", Role::Blue),
        ("build", Role::Green),
        ("auto", Role::Amber),
        ("auto_unsafe", Role::Red),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    (syntax, modes)
}

impl Theme {
    fn built(name: &str, colours: [(Role, &str); 8]) -> Self {
        let (syntax, modes) = default_maps();
        Self {
            name: name.to_string(),
            palette: colours
                .into_iter()
                .map(|(r, hex)| (r, Rgb::parse(hex).unwrap_or(Rgb(0, 0, 0))))
                .collect(),
            syntax,
            modes,
        }
    }

    /// `cairn-dark`, the default (§10.5's example palette).
    #[must_use]
    pub fn cairn_dark() -> Self {
        Self::built(
            "cairn-dark",
            [
                (Role::Bg, "#0f1419"),
                (Role::Fg, "#d8dee9"),
                (Role::Dim, "#6b7684"),
                (Role::Accent, "#5fb3b3"),
                (Role::Red, "#e06c75"),
                (Role::Amber, "#e5c07b"),
                (Role::Green, "#98c379"),
                (Role::Blue, "#61afef"),
            ],
        )
    }

    #[must_use]
    pub fn cairn_light() -> Self {
        Self::built(
            "cairn-light",
            [
                (Role::Bg, "#fafafa"),
                (Role::Fg, "#2b303b"),
                (Role::Dim, "#7a8290"),
                (Role::Accent, "#1d7f7f"),
                (Role::Red, "#c0392b"),
                (Role::Amber, "#b7791f"),
                (Role::Green, "#2f7d32"),
                (Role::Blue, "#1f5fbf"),
            ],
        )
    }

    /// §10.8: black on white, every colour at full strength.
    #[must_use]
    pub fn high_contrast() -> Self {
        Self::built(
            "high-contrast",
            [
                (Role::Bg, "#000000"),
                (Role::Fg, "#ffffff"),
                (Role::Dim, "#c0c0c0"),
                (Role::Accent, "#00ffff"),
                (Role::Red, "#ff5555"),
                (Role::Amber, "#ffff55"),
                (Role::Green, "#55ff55"),
                (Role::Blue, "#6fa8ff"),
            ],
        )
    }

    /// A built-in theme by name.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "cairn-dark" => Some(Self::cairn_dark()),
            "cairn-light" => Some(Self::cairn_light()),
            "high-contrast" => Some(Self::high_contrast()),
            _ => None,
        }
    }

    /// Names `/theme` offers.
    #[must_use]
    pub fn builtin_names() -> [&'static str; 3] {
        ["cairn-dark", "cairn-light", "high-contrast"]
    }

    /// A theme from `~/.config/cairn/themes/*.toml` (§10.5). Unknown keys are
    /// errors, so a typo is found rather than ignored.
    ///
    /// # Errors
    /// [`ThemeError`] naming the problem.
    pub fn from_toml(text: &str) -> Result<Self, ThemeError> {
        let doc: toml::Value = toml::from_str(text).map_err(|e| ThemeError(e.to_string()))?;
        let root = doc
            .as_table()
            .ok_or_else(|| ThemeError("the file is not a table".into()))?;
        if let Some(extra) = root.keys().find(|k| *k != "theme") {
            return Err(ThemeError(format!("unknown top-level key `{extra}`")));
        }
        let theme = root
            .get("theme")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| ThemeError("missing [theme]".into()))?;
        let mut out = Self::cairn_dark();
        for (key, value) in theme {
            match key.as_str() {
                "name" => {
                    out.name = value
                        .as_str()
                        .ok_or_else(|| ThemeError("`name` must be a string".into()))?
                        .to_string();
                }
                "palette" => {
                    let palette = value
                        .as_table()
                        .ok_or_else(|| ThemeError("`palette` must be a table".into()))?;
                    for (role, hex) in palette {
                        let role = Role::from_name(role)
                            .ok_or_else(|| ThemeError(format!("unknown palette key `{role}`")))?;
                        let hex = hex.as_str().ok_or_else(|| {
                            ThemeError(format!("`{}` must be a #rrggbb string", role.name()))
                        })?;
                        out.palette
                            .insert(role, Rgb::parse(hex).map_err(ThemeError)?);
                    }
                }
                "syntax" => out.syntax = role_map(value, &SYNTAX_KEYS, "syntax", &out.syntax)?,
                "modes" => out.modes = role_map(value, &MODE_KEYS, "modes", &out.modes)?,
                other => return Err(ThemeError(format!("unknown key `theme.{other}`"))),
            }
        }
        Ok(out)
    }

    /// The colour a role has in this theme.
    #[must_use]
    pub fn rgb(&self, role: Role) -> Rgb {
        self.palette
            .get(&role)
            .copied()
            .unwrap_or(Rgb(255, 255, 255))
    }

    /// The role a mode is shown in.
    #[must_use]
    pub fn mode_role(&self, mode: &str) -> Role {
        self.modes.get(mode).copied().unwrap_or(Role::Fg)
    }

    /// The ratatui colour for `role` on a terminal with `support`.
    #[must_use]
    pub fn color(&self, role: Role, support: ColorSupport) -> Option<Color> {
        match support {
            ColorSupport::None => None,
            ColorSupport::True => {
                let Rgb(r, g, b) = self.rgb(role);
                Some(Color::Rgb(r, g, b))
            }
            ColorSupport::Ansi256 => Some(Color::Indexed(nearest_256(self.rgb(role)))),
            // §10.7's fixed mapping; the default colours stay the terminal's.
            ColorSupport::Ansi16 => match role {
                Role::Bg | Role::Fg => None,
                Role::Dim => Some(Color::DarkGray),
                Role::Accent => Some(Color::Cyan),
                Role::Green => Some(Color::Green),
                Role::Amber => Some(Color::Yellow),
                Role::Red => Some(Color::Red),
                Role::Blue => Some(Color::Blue),
            },
        }
    }

    /// A foreground style for `role`.
    #[must_use]
    pub fn fg(&self, role: Role, support: ColorSupport) -> Style {
        self.color(role, support)
            .map_or_else(Style::default, |c| Style::default().fg(c))
    }
}

fn role_map(
    value: &toml::Value,
    allowed: &[&str],
    section: &str,
    base: &BTreeMap<String, Role>,
) -> Result<BTreeMap<String, Role>, ThemeError> {
    let table = value
        .as_table()
        .ok_or_else(|| ThemeError(format!("`{section}` must be a table")))?;
    let mut out = base.clone();
    for (key, role) in table {
        if !allowed.contains(&key.as_str()) {
            return Err(ThemeError(format!("unknown key `theme.{section}.{key}`")));
        }
        let name = role
            .as_str()
            .ok_or_else(|| ThemeError(format!("`{key}` must name a palette colour")))?;
        let role = Role::from_name(name)
            .ok_or_else(|| ThemeError(format!("`{key}` names unknown colour `{name}`")))?;
        out.insert(key.clone(), role);
    }
    Ok(out)
}

// ------------------------------------------------------------ quantisation

fn srgb_to_linear(c: u8) -> f64 {
    let v = f64::from(c) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// CIE L*a*b* (D65) of an sRGB colour.
#[allow(
    clippy::many_single_char_names,
    reason = "the colour-science symbols are single letters"
)]
fn lab(Rgb(r, g, b): Rgb) -> (f64, f64, f64) {
    let (r, g, b) = (srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
    let x = (0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b) / 0.950_47;
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175 * b;
    let z = (0.019_333_9 * r + 0.119_192 * g + 0.950_304_1 * b) / 1.088_83;
    let f = |t: f64| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// The xterm-256 palette entry `index` (16..=255) as RGB.
fn xterm(index: u8) -> Rgb {
    if index >= 232 {
        let v = 8 + 10 * (index - 232);
        return Rgb(v, v, v);
    }
    let i = index - 16;
    let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
    Rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
}

/// The closest of xterm's 240 non-system colours by CIE76 distance.
#[must_use]
pub fn nearest_256(target: Rgb) -> u8 {
    let (l0, a0, b0) = lab(target);
    (16..=255_u8)
        .map(|i| {
            let (l, a, b) = lab(xterm(i));
            let d = (l - l0).powi(2) + (a - a0).powi(2) + (b - b0).powi(2);
            (i, d)
        })
        .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(16, |(i, _)| i)
}

// -------------------------------------------------------------------- glyphs

/// The symbols the interface draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    pub ok: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
    pub expand: &'static str,
    pub collapse: &'static str,
    pub tool: &'static str,
    pub prompt: &'static str,
    pub bullet: &'static str,
    pub online: &'static str,
    pub offline: &'static str,
    pub ellipsis: &'static str,
    pub horizontal: &'static str,
    pub spinner: &'static [&'static str],
}

impl Glyphs {
    /// Unicode symbols for ordinary terminals.
    #[must_use]
    pub const fn unicode() -> Self {
        Self {
            ok: "✓",
            fail: "✗",
            warn: "⚠",
            expand: "▸",
            collapse: "▾",
            tool: "⏺",
            prompt: "›",
            bullet: "·",
            online: "●",
            offline: "◌",
            ellipsis: "…",
            horizontal: "─",
            spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
        }
    }

    /// Plain ASCII for screen readers and `TERM=dumb` (§10.8).
    #[must_use]
    pub const fn ascii() -> Self {
        Self {
            ok: "[ok]",
            fail: "[x]",
            warn: "[!]",
            expand: ">",
            collapse: "v",
            tool: "*",
            prompt: ">",
            bullet: "-",
            online: "(*)",
            offline: "( )",
            ellipsis: "...",
            horizontal: "-",
            spinner: &["|", "/", "-", "\\"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// §10.7's compatibility matrix.
    #[test]
    fn colour_support_follows_the_environment() {
        use ColorSupport::{Ansi16, Ansi256, None, True};
        type Case<'a> = (Vec<(&'a str, &'a str)>, bool, ColorSupport);
        let cases: Vec<Case<'_>> = vec![
            (
                vec![("COLORTERM", "truecolor"), ("TERM", "xterm-256color")],
                true,
                True,
            ),
            (vec![("COLORTERM", "24bit")], true, True),
            (vec![("TERM", "xterm-256color")], true, Ansi256),
            (vec![("TERM", "tmux-256color")], true, Ansi256),
            (vec![("TERM", "xterm-kitty")], true, Ansi256),
            (vec![("TERM", "xterm")], true, Ansi16),
            (vec![("TERM", "linux")], true, Ansi16),
            (vec![], true, Ansi16),
            (
                vec![("TERM", "dumb"), ("COLORTERM", "truecolor")],
                true,
                None,
            ),
            (
                vec![("NO_COLOR", "1"), ("COLORTERM", "truecolor")],
                true,
                None,
            ),
            (
                vec![("NO_COLOR", ""), ("COLORTERM", "truecolor")],
                true,
                None,
            ),
            (vec![("COLORTERM", "truecolor")], false, None),
            (
                vec![("COLORTERM", "truecolor"), ("CLICOLOR_FORCE", "1")],
                false,
                True,
            ),
            (
                vec![("CLICOLOR_FORCE", "0"), ("TERM", "xterm")],
                false,
                None,
            ),
            (vec![("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")], true, None),
        ];
        for (vars, tty, want) in cases {
            assert_eq!(
                ColorSupport::detect(&env(&vars), tty),
                want,
                "{vars:?} tty={tty}"
            );
        }
    }

    #[test]
    fn hex_colours_parse_strictly() {
        assert_eq!(Rgb::parse("#0f1419"), Ok(Rgb(15, 20, 25)));
        assert!(Rgb::parse("0f1419").is_err());
        assert!(Rgb::parse("#0f14").is_err());
        assert!(Rgb::parse("#gggggg").is_err());
    }

    /// T-TUI-032: the 16-colour mapping is the one §10.7 names.
    #[test]
    fn sixteen_colours_use_the_semantic_mapping() {
        let t = Theme::cairn_dark();
        let s = ColorSupport::Ansi16;
        assert_eq!(t.color(Role::Accent, s), Some(Color::Cyan));
        assert_eq!(t.color(Role::Green, s), Some(Color::Green));
        assert_eq!(t.color(Role::Amber, s), Some(Color::Yellow));
        assert_eq!(t.color(Role::Red, s), Some(Color::Red));
        assert_eq!(t.color(Role::Blue, s), Some(Color::Blue));
        assert_eq!(t.color(Role::Dim, s), Some(Color::DarkGray));
        assert_eq!(t.color(Role::Fg, s), None, "the terminal's own text colour");
        assert_eq!(t.color(Role::Bg, s), None);
    }

    #[test]
    fn no_colour_means_no_colour() {
        let t = Theme::cairn_dark();
        for role in Role::ALL {
            assert_eq!(t.color(role, ColorSupport::None), None);
            assert_eq!(t.fg(role, ColorSupport::None), Style::default());
        }
        assert_eq!(
            t.color(Role::Red, ColorSupport::True),
            Some(Color::Rgb(0xe0, 0x6c, 0x75))
        );
    }

    #[test]
    fn two_hundred_fifty_six_colours_pick_the_nearest_entry() {
        // Pure colours land on the cube corners; greys on the grey ramp.
        assert_eq!(nearest_256(Rgb(255, 255, 255)), 231);
        assert_eq!(nearest_256(Rgb(0, 0, 0)), 16);
        assert_eq!(nearest_256(Rgb(255, 0, 0)), 196);
        assert_eq!(nearest_256(Rgb(0, 255, 0)), 46);
        assert_eq!(nearest_256(Rgb(0, 0, 255)), 21);
        assert!((232..=255).contains(&nearest_256(Rgb(128, 128, 128))));
        // The result is always an actual palette entry, and close to the target.
        let t = Theme::cairn_dark();
        for role in Role::ALL {
            let want = t.rgb(role);
            let got = xterm(nearest_256(want));
            let (l1, a1, b1) = lab(want);
            let (l2, a2, b2) = lab(got);
            let delta = ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt();
            assert!(delta < 30.0, "{role:?}: {want:?} → {got:?} ({delta})");
        }
    }

    #[test]
    fn modes_have_their_colours_and_every_theme_defines_them() {
        for name in Theme::builtin_names() {
            let t = Theme::named(name).unwrap();
            assert_eq!(t.name, name);
            assert_eq!(t.mode_role("plan"), Role::Blue);
            assert_eq!(t.mode_role("build"), Role::Green);
            assert_eq!(t.mode_role("auto"), Role::Amber);
            assert_eq!(t.mode_role("auto_unsafe"), Role::Red);
        }
        assert!(Theme::named("solarized").is_none());
    }

    /// §10.8: high contrast is black and white.
    #[test]
    fn high_contrast_is_black_and_white() {
        let t = Theme::high_contrast();
        assert_eq!(t.rgb(Role::Bg), Rgb(0, 0, 0));
        assert_eq!(t.rgb(Role::Fg), Rgb(255, 255, 255));
    }

    #[test]
    fn a_theme_file_overrides_what_it_names_and_rejects_what_it_does_not_know() {
        let t = Theme::from_toml(
            "[theme]\nname = \"mine\"\n[theme.palette]\nred = \"#ff0000\"\n[theme.syntax]\nkeyword = \"blue\"\n[theme.modes]\nplan = \"amber\"\n",
        )
        .unwrap();
        assert_eq!(t.name, "mine");
        assert_eq!(t.rgb(Role::Red), Rgb(255, 0, 0));
        assert_eq!(
            t.rgb(Role::Green),
            Theme::cairn_dark().rgb(Role::Green),
            "the rest is kept"
        );
        assert_eq!(t.syntax["keyword"], Role::Blue);
        assert_eq!(t.mode_role("plan"), Role::Amber);
        for (bad, want) in [
            ("[theme]\nbogus = 1\n", "unknown key `theme.bogus`"),
            ("", "missing [theme]"),
            (
                "[theme]\n[theme.palette]\npurple = \"#ff00ff\"\n",
                "unknown palette key `purple`",
            ),
            (
                "[theme]\n[theme.palette]\nred = \"red\"\n",
                "is not #rrggbb",
            ),
            (
                "[theme]\n[theme.syntax]\nkeyword = \"purple\"\n",
                "unknown colour `purple`",
            ),
            (
                "[theme]\n[theme.syntax]\nregex = \"red\"\n",
                "unknown key `theme.syntax.regex`",
            ),
            ("[other]\n", "unknown top-level key"),
            ("not toml [", "expected"),
        ] {
            let err = Theme::from_toml(bad).unwrap_err();
            assert!(err.to_string().contains(want), "{bad:?}: {err}");
            assert!(err.to_string().starts_with("E-CFG-THEME"));
        }
    }

    /// REQ-TUI-009: statuses are symbols too.
    #[test]
    fn glyph_sets_distinguish_success_failure_and_warning_without_colour() {
        for g in [Glyphs::unicode(), Glyphs::ascii()] {
            let all = [g.ok, g.fail, g.warn];
            assert!(all.iter().all(|s| !s.is_empty()));
            assert_ne!(g.ok, g.fail);
            assert_ne!(g.fail, g.warn);
            assert_ne!(g.ok, g.warn);
            assert!(g.spinner.len() >= 4);
        }
        assert!(
            Glyphs::ascii().ok.is_ascii() && Glyphs::ascii().spinner.iter().all(|s| s.is_ascii())
        );
        assert_eq!(Glyphs::unicode().spinner.len(), 10);
    }
}
