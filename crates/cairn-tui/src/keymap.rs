//! Keybindings (SPEC §10.4): the built-in table, user overrides from
//! `keybindings.toml`, chords of up to two keys, and the rules that keep
//! overrides honest.

use std::collections::BTreeMap;

use crate::keys::{self, Code, Key};

/// What a key can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Submit,
    Newline,
    Complete,
    CycleMode,
    Cancel,
    CloseOverlay,
    CopyOrQuit,
    QuitIfEmpty,
    ClearView,
    HistorySearch,
    ExpandTool,
    ToggleTodos,
    QuickOpen,
    KillLine,
    KillToEnd,
    KillWord,
    PasteImage,
    Yank,
    ToggleHelp,
    QuitConfirm,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    HistoryPrev,
    HistoryNext,
    Down,
    Up,
    Select,
    Allow,
    AllowAlways,
    Deny,
    EditRequest,
    AcceptHunk,
    RejectHunk,
    PrevHunk,
    NextHunk,
    NextFile,
    PrevFile,
    Yes,
    No,
}

impl Action {
    pub const ALL: [Self; 41] = [
        Self::Submit,
        Self::Newline,
        Self::Complete,
        Self::CycleMode,
        Self::Cancel,
        Self::CloseOverlay,
        Self::CopyOrQuit,
        Self::QuitIfEmpty,
        Self::ClearView,
        Self::HistorySearch,
        Self::ExpandTool,
        Self::ToggleTodos,
        Self::QuickOpen,
        Self::KillLine,
        Self::KillToEnd,
        Self::KillWord,
        Self::PasteImage,
        Self::Yank,
        Self::ToggleHelp,
        Self::QuitConfirm,
        Self::PageUp,
        Self::PageDown,
        Self::JumpTop,
        Self::JumpBottom,
        Self::HistoryPrev,
        Self::HistoryNext,
        Self::Down,
        Self::Up,
        Self::Select,
        Self::Allow,
        Self::AllowAlways,
        Self::Deny,
        Self::EditRequest,
        Self::AcceptHunk,
        Self::RejectHunk,
        Self::PrevHunk,
        Self::NextHunk,
        Self::NextFile,
        Self::PrevFile,
        Self::Yes,
        Self::No,
    ];

    /// The name used in `keybindings.toml`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Newline => "newline",
            Self::Complete => "complete",
            Self::CycleMode => "cycle_mode",
            Self::Cancel => "cancel",
            Self::CloseOverlay => "close_overlay",
            Self::CopyOrQuit => "copy_or_quit",
            Self::QuitIfEmpty => "quit_if_empty",
            Self::ClearView => "clear_view",
            Self::HistorySearch => "history_search",
            Self::ExpandTool => "expand_tool",
            Self::ToggleTodos => "toggle_todos",
            Self::QuickOpen => "quick_open",
            Self::KillLine => "kill_line",
            Self::KillToEnd => "kill_to_end",
            Self::KillWord => "kill_word",
            Self::PasteImage => "paste_image",
            Self::Yank => "yank",
            Self::ToggleHelp => "toggle_help",
            Self::QuitConfirm => "quit",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
            Self::JumpTop => "jump_top",
            Self::JumpBottom => "jump_bottom",
            Self::HistoryPrev => "history_prev",
            Self::HistoryNext => "history_next",
            Self::Down => "down",
            Self::Up => "up",
            Self::Select => "select",
            Self::Allow => "allow",
            Self::AllowAlways => "allow_always",
            Self::Deny => "deny",
            Self::EditRequest => "edit_request",
            Self::AcceptHunk => "accept_hunk",
            Self::RejectHunk => "reject_hunk",
            Self::PrevHunk => "prev_hunk",
            Self::NextHunk => "next_hunk",
            Self::NextFile => "next_file",
            Self::PrevFile => "prev_file",
            Self::Yes => "yes",
            Self::No => "no",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// Where a binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    Global,
    Input,
    Transcript,
    Overlay,
    Approval,
    Diff,
    Prompt,
}

impl Context {
    pub const ALL: [Self; 7] = [
        Self::Global,
        Self::Input,
        Self::Transcript,
        Self::Overlay,
        Self::Approval,
        Self::Diff,
        Self::Prompt,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Input => "input",
            Self::Transcript => "transcript",
            Self::Overlay => "overlay",
            Self::Approval => "approval",
            Self::Diff => "diff",
            Self::Prompt => "prompt",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// A binding file problem, with its stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct KeymapError {
    /// `E-CFG-KEYCONFLICT` or `E-CFG-KEYRESERVED` (or `E-CFG-KEYBINDINGS`
    /// for a file that does not parse).
    pub code: &'static str,
    pub message: String,
}

/// §10.4: combinations the operating system owns.
const RESERVED: [&str; 4] = ["alt+f4", "cmd+q", "ctrl+alt+tab", "super+q"];

/// A sequence of one or two keys.
pub type Chord = Vec<Key>;

/// The bindings in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    table: BTreeMap<Context, Vec<(Chord, Action)>>,
}

fn ch(spec: &str) -> Chord {
    spec.split_whitespace()
        .map(|k| keys::parse(k).unwrap_or(Key::new(Code::Esc)))
        .collect()
}

impl Default for Keymap {
    fn default() -> Self {
        use Action as A;
        use Context as C;
        let mut table: BTreeMap<Context, Vec<(Chord, Action)>> = BTreeMap::new();
        let mut add = |ctx: Context, spec: &str, action: Action| {
            table.entry(ctx).or_default().push((ch(spec), action));
        };
        // §10.4, row by row.
        add(C::Input, "enter", A::Submit);
        add(C::Input, "shift+enter", A::Newline);
        add(C::Input, "alt+enter", A::Newline);
        add(C::Input, "tab", A::Complete);
        add(C::Global, "shift+tab", A::CycleMode);
        add(C::Global, "esc", A::Cancel);
        add(C::Overlay, "esc", A::CloseOverlay);
        add(C::Global, "ctrl+c", A::CopyOrQuit);
        add(C::Global, "ctrl+d", A::QuitIfEmpty);
        add(C::Global, "ctrl+l", A::ClearView);
        add(C::Global, "ctrl+r", A::HistorySearch);
        add(C::Global, "ctrl+o", A::ExpandTool);
        add(C::Global, "ctrl+t", A::ToggleTodos);
        add(C::Global, "ctrl+p", A::QuickOpen);
        add(C::Input, "ctrl+u", A::KillLine);
        add(C::Input, "ctrl+k", A::KillToEnd);
        add(C::Input, "ctrl+w", A::KillWord);
        add(C::Input, "ctrl+v", A::PasteImage);
        add(C::Input, "ctrl+y", A::Yank);
        add(C::Global, "ctrl+g", A::ToggleHelp);
        add(C::Global, "f1", A::ToggleHelp);
        add(C::Global, "ctrl+q", A::QuitConfirm);
        add(C::Transcript, "pageup", A::PageUp);
        add(C::Transcript, "pagedown", A::PageDown);
        add(C::Transcript, "ctrl+home", A::JumpTop);
        add(C::Transcript, "ctrl+end", A::JumpBottom);
        add(C::Input, "up", A::HistoryPrev);
        add(C::Input, "down", A::HistoryNext);
        add(C::Overlay, "j", A::Down);
        add(C::Overlay, "k", A::Up);
        add(C::Overlay, "down", A::Down);
        add(C::Overlay, "up", A::Up);
        add(C::Overlay, "enter", A::Select);
        add(C::Approval, "a", A::Allow);
        add(C::Approval, "shift+a", A::AllowAlways);
        add(C::Approval, "d", A::Deny);
        add(C::Approval, "e", A::EditRequest);
        add(C::Diff, "space", A::AcceptHunk);
        add(C::Diff, "r", A::RejectHunk);
        add(C::Diff, "h", A::PrevHunk);
        add(C::Diff, "l", A::NextHunk);
        add(C::Diff, "n", A::NextFile);
        add(C::Diff, "p", A::PrevFile);
        add(C::Prompt, "y", A::Yes);
        add(C::Prompt, "n", A::No);
        Self { table }
    }
}

/// What feeding one key to the keymap produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Action(Action),
    /// The first key of a chord; the next one decides.
    Pending,
    /// Not bound here.
    None,
}

/// Chord state between keys.
#[derive(Debug, Clone, Default)]
pub struct Chords {
    first: Option<(Key, u64)>,
}

/// §10.4: the second key of a chord must come within this many milliseconds.
pub const CHORD_WINDOW_MS: u64 = 1000;

impl Keymap {
    /// The bindings of one context.
    #[must_use]
    pub fn bindings(&self, ctx: Context) -> &[(Chord, Action)] {
        self.table.get(&ctx).map_or(&[], Vec::as_slice)
    }

    /// The keys bound to `action` anywhere, for help text.
    #[must_use]
    pub fn keys_for(&self, action: Action) -> Vec<String> {
        self.table
            .values()
            .flatten()
            .filter(|(_, a)| *a == action)
            .map(|(chord, _)| chord.iter().map(keys::show).collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// Look `key` up in `ctx`, falling back to `global`. `now_ms` is any
    /// monotonic millisecond clock.
    pub fn resolve(&self, ctx: Context, key: Key, chords: &mut Chords, now_ms: u64) -> Resolved {
        let contexts: Vec<Context> = if ctx == Context::Global {
            vec![ctx]
        } else {
            vec![ctx, Context::Global]
        };
        if let Some((first, at)) = chords.first.take() {
            if now_ms.saturating_sub(at) <= CHORD_WINDOW_MS {
                for c in &contexts {
                    for (chord, action) in self.bindings(*c) {
                        if chord.len() == 2 && chord[0] == first && chord[1] == key {
                            return Resolved::Action(*action);
                        }
                    }
                }
            }
            // The window passed or the second key did not match: this key
            // starts over.
        }
        for c in &contexts {
            for (chord, action) in self.bindings(*c) {
                if chord.len() == 1 && chord[0] == key {
                    return Resolved::Action(*action);
                }
            }
        }
        for c in &contexts {
            if self
                .bindings(*c)
                .iter()
                .any(|(chord, _)| chord.len() == 2 && chord[0] == key)
            {
                chords.first = Some((key, now_ms));
                return Resolved::Pending;
            }
        }
        Resolved::None
    }

    /// Apply `keybindings.toml` over the built-ins.
    ///
    /// Returns the keymap and every problem found. A context with a problem
    /// keeps its built-in bindings and ignores the file's for that context
    /// (§10.4 rule 3); other contexts still take their overrides.
    #[must_use]
    pub fn with_overrides(text: &str) -> (Self, Vec<KeymapError>) {
        let mut map = Self::default();
        let mut errors = Vec::new();
        let doc: toml::Value = match toml::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return (
                    map,
                    vec![KeymapError {
                        code: "E-CFG-KEYBINDINGS",
                        message: format!("keybindings.toml does not parse: {e}"),
                    }],
                );
            }
        };
        let Some(bindings) = doc.get("bindings").and_then(toml::Value::as_table) else {
            return (map, errors);
        };
        let mut by_context: BTreeMap<Context, Vec<(String, String)>> = BTreeMap::new();
        for (key, value) in bindings {
            if key == "context" {
                let Some(contexts) = value.as_table() else {
                    continue;
                };
                for (name, table) in contexts {
                    let Some(ctx) = Context::from_name(name) else {
                        errors.push(KeymapError {
                            code: "E-CFG-KEYBINDINGS",
                            message: format!("unknown context `{name}`"),
                        });
                        continue;
                    };
                    for (k, v) in table.as_table().into_iter().flatten() {
                        if let Some(action) = v.as_str() {
                            by_context
                                .entry(ctx)
                                .or_default()
                                .push((k.clone(), action.to_string()));
                        }
                    }
                }
            } else if let Some(action) = value.as_str() {
                by_context
                    .entry(Context::Global)
                    .or_default()
                    .push((key.clone(), action.to_string()));
            }
        }
        for (ctx, entries) in by_context {
            match Self::parse_context(ctx, &entries) {
                Ok(parsed) => {
                    let list = map.table.entry(ctx).or_default();
                    for (chord, action) in parsed {
                        // User bindings override: drop what they shadow.
                        list.retain(|(c, _)| *c != chord);
                        list.push((chord, action));
                    }
                }
                Err(mut e) => errors.append(&mut e),
            }
        }
        (map, errors)
    }

    fn parse_context(
        ctx: Context,
        entries: &[(String, String)],
    ) -> Result<Vec<(Chord, Action)>, Vec<KeymapError>> {
        let mut errors = Vec::new();
        let mut parsed: Vec<(String, Chord, Action)> = Vec::new();
        for (spec, action_name) in entries {
            let normalized = spec
                .to_ascii_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if normalized
                .split(' ')
                .any(|k| RESERVED.contains(&k.replace(' ', "").as_str()))
            {
                errors.push(KeymapError {
                    code: "E-CFG-KEYRESERVED",
                    message: format!("`{spec}` is reserved by the operating system"),
                });
                continue;
            }
            let Some(action) = Action::from_name(action_name) else {
                errors.push(KeymapError {
                    code: "E-CFG-KEYBINDINGS",
                    message: format!("`{spec}` names unknown action `{action_name}`"),
                });
                continue;
            };
            let mut chord = Vec::new();
            let mut ok = true;
            for part in normalized.split(' ') {
                match keys::parse(part) {
                    Ok(k) => chord.push(k),
                    Err(e) => {
                        errors.push(KeymapError {
                            code: "E-CFG-KEYBINDINGS",
                            message: e,
                        });
                        ok = false;
                    }
                }
            }
            if ok && chord.len() > 2 {
                errors.push(KeymapError {
                    code: "E-CFG-KEYBINDINGS",
                    message: format!("`{spec}` is longer than a two-key chord"),
                });
                ok = false;
            }
            if ok {
                parsed.push((spec.clone(), chord, action));
            }
        }
        // §10.4 rule 1: once per context, and a key may not both stand alone
        // and begin a chord.
        for (i, (spec_a, a, _)) in parsed.iter().enumerate() {
            for (spec_b, b, _) in &parsed[i + 1..] {
                let same = a == b;
                let prefix = (a.len() == 1 && b.len() == 2 && b[0] == a[0])
                    || (b.len() == 1 && a.len() == 2 && a[0] == b[0]);
                if same || prefix {
                    errors.push(KeymapError {
                        code: "E-CFG-KEYCONFLICT",
                        message: format!(
                            "in context `{}`, `{spec_a}` and `{spec_b}` conflict",
                            ctx.name()
                        ),
                    });
                }
            }
        }
        if errors.is_empty() {
            Ok(parsed.into_iter().map(|(_, c, a)| (c, a)).collect())
        } else {
            Err(errors)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (Keymap, Chords) {
        (Keymap::default(), Chords::default())
    }

    fn act(map: &Keymap, ctx: Context, key: Key) -> Resolved {
        map.resolve(ctx, key, &mut Chords::default(), 0)
    }

    /// §10.4's table, a representative row per context.
    #[test]
    fn the_built_in_bindings_are_the_specified_ones() {
        let (m, _) = fresh();
        let cases = [
            (Context::Input, "enter", Action::Submit),
            (Context::Input, "shift+enter", Action::Newline),
            (Context::Input, "alt+enter", Action::Newline),
            (Context::Input, "tab", Action::Complete),
            (Context::Global, "shift+tab", Action::CycleMode),
            (Context::Global, "esc", Action::Cancel),
            (Context::Overlay, "esc", Action::CloseOverlay),
            (Context::Global, "ctrl+c", Action::CopyOrQuit),
            (Context::Global, "ctrl+d", Action::QuitIfEmpty),
            (Context::Global, "ctrl+l", Action::ClearView),
            (Context::Global, "ctrl+r", Action::HistorySearch),
            (Context::Global, "ctrl+o", Action::ExpandTool),
            (Context::Global, "ctrl+t", Action::ToggleTodos),
            (Context::Global, "ctrl+p", Action::QuickOpen),
            (Context::Global, "ctrl+g", Action::ToggleHelp),
            (Context::Global, "f1", Action::ToggleHelp),
            (Context::Global, "ctrl+q", Action::QuitConfirm),
            (Context::Transcript, "pageup", Action::PageUp),
            (Context::Transcript, "ctrl+end", Action::JumpBottom),
            (Context::Input, "up", Action::HistoryPrev),
            (Context::Overlay, "j", Action::Down),
            (Context::Approval, "a", Action::Allow),
            (Context::Approval, "shift+a", Action::AllowAlways),
            (Context::Approval, "d", Action::Deny),
            (Context::Approval, "e", Action::EditRequest),
            (Context::Diff, "space", Action::AcceptHunk),
            (Context::Diff, "r", Action::RejectHunk),
            (Context::Diff, "n", Action::NextFile),
            (Context::Prompt, "y", Action::Yes),
        ];
        for (ctx, key, want) in cases {
            assert_eq!(
                act(&m, ctx, keys::parse(key).unwrap()),
                Resolved::Action(want),
                "{ctx:?} {key}"
            );
        }
        // A key with no binding resolves to nothing.
        assert_eq!(act(&m, Context::Input, Key::char('x')), Resolved::None);
    }

    #[test]
    fn a_context_falls_back_to_global() {
        let (m, _) = fresh();
        assert_eq!(
            act(&m, Context::Input, Key::ctrl('r')),
            Resolved::Action(Action::HistorySearch)
        );
        assert_eq!(
            act(&m, Context::Approval, Key::ctrl('g')),
            Resolved::Action(Action::ToggleHelp)
        );
    }

    #[test]
    fn every_action_has_a_unique_name_that_round_trips() {
        let names: std::collections::BTreeSet<&str> =
            Action::ALL.iter().map(|a| a.name()).collect();
        assert_eq!(names.len(), Action::ALL.len());
        for a in Action::ALL {
            assert_eq!(Action::from_name(a.name()), Some(a));
        }
        // And every action is bound somewhere by default (nothing is dead).
        let m = Keymap::default();
        let unbound: Vec<&str> = Action::ALL
            .iter()
            .filter(|a| m.keys_for(**a).is_empty())
            .map(|a| a.name())
            .collect();
        assert!(unbound.is_empty(), "{unbound:?}");
    }

    #[test]
    fn user_bindings_override_and_chords_resolve_within_the_window() {
        let (m, errors) = Keymap::with_overrides(
            "[bindings]\n\"ctrl+x ctrl+s\" = \"submit\"\n\"ctrl+t\" = \"toggle_todos\"\n[bindings.context.input]\n\"ctrl+j\" = \"newline\"\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        let mut chords = Chords::default();
        assert_eq!(
            m.resolve(Context::Input, Key::ctrl('x'), &mut chords, 100),
            Resolved::Pending
        );
        assert_eq!(
            m.resolve(Context::Input, Key::ctrl('s'), &mut chords, 600),
            Resolved::Action(Action::Submit)
        );
        // Too slow: the second key starts over and means nothing here.
        assert_eq!(
            m.resolve(Context::Input, Key::ctrl('x'), &mut chords, 0),
            Resolved::Pending
        );
        assert_eq!(
            m.resolve(Context::Input, Key::ctrl('s'), &mut chords, 1500),
            Resolved::None
        );
        // Wrong second key.
        assert_eq!(
            m.resolve(Context::Input, Key::ctrl('x'), &mut chords, 0),
            Resolved::Pending
        );
        assert_eq!(
            m.resolve(Context::Input, Key::char('q'), &mut chords, 10),
            Resolved::None
        );
        // A context binding.
        assert_eq!(
            act(&m, Context::Input, Key::ctrl('j')),
            Resolved::Action(Action::Newline)
        );
    }

    /// T-CFG-021: conflicts are found without a terminal.
    #[test]
    fn a_conflict_in_one_context_drops_that_contexts_overrides_only() {
        let (m, errors) = Keymap::with_overrides(
            "[bindings.context.input]\n\"ctrl+j\" = \"newline\"\n\"Ctrl+J\" = \"submit\"\n[bindings.context.diff]\n\"x\" = \"accept_hunk\"\n",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "E-CFG-KEYCONFLICT");
        assert!(
            errors[0].message.contains("input") && errors[0].message.contains("ctrl+j"),
            "{}",
            errors[0]
        );
        // Input keeps its built-ins (no ctrl+j); diff takes its override.
        assert_eq!(act(&m, Context::Input, Key::ctrl('j')), Resolved::None);
        assert_eq!(
            act(&m, Context::Input, Key::new(Code::Enter)),
            Resolved::Action(Action::Submit)
        );
        assert_eq!(
            act(&m, Context::Diff, Key::char('x')),
            Resolved::Action(Action::AcceptHunk)
        );
    }

    #[test]
    fn a_key_cannot_both_stand_alone_and_begin_a_chord() {
        let (_, errors) = Keymap::with_overrides(
            "[bindings]\n\"ctrl+x\" = \"quit\"\n\"ctrl+x ctrl+s\" = \"submit\"\n",
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "E-CFG-KEYCONFLICT");
    }

    #[test]
    fn reserved_combinations_are_refused() {
        for key in ["alt+f4", "cmd+q", "ctrl+alt+tab"] {
            let (_, errors) =
                Keymap::with_overrides(&format!("[bindings]\n\"{key}\" = \"quit\"\n"));
            assert_eq!(errors.len(), 1, "{key}");
            assert_eq!(errors[0].code, "E-CFG-KEYRESERVED", "{key}");
        }
    }

    #[test]
    fn unknown_actions_keys_and_long_chords_are_reported() {
        for (text, want) in [
            ("[bindings]\n\"ctrl+x\" = \"fly\"\n", "unknown action `fly`"),
            ("[bindings]\n\"ctrl+banana\" = \"quit\"\n", "banana"),
            (
                "[bindings]\n\"a b c\" = \"quit\"\n",
                "longer than a two-key chord",
            ),
            (
                "[bindings.context.nowhere]\n\"a\" = \"quit\"\n",
                "unknown context `nowhere`",
            ),
            ("not toml [", "does not parse"),
        ] {
            let (m, errors) = Keymap::with_overrides(text);
            assert!(!errors.is_empty(), "{text}");
            assert!(
                errors.iter().any(|e| e.message.contains(want)),
                "{text}: {errors:?}"
            );
            assert_eq!(m, Keymap::default(), "{text}: a bad file changes nothing");
        }
    }

    #[test]
    fn an_override_replaces_the_key_it_shadows() {
        let (m, errors) =
            Keymap::with_overrides("[bindings.context.input]\n\"enter\" = \"newline\"\n");
        assert!(errors.is_empty());
        assert_eq!(
            act(&m, Context::Input, Key::new(Code::Enter)),
            Resolved::Action(Action::Newline)
        );
    }
}
