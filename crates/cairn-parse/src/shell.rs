//! Shell syntax extraction (SPEC §9.3): the commands a script would run, the
//! words they were given, and what feeds and follows them — and nothing about
//! whether any of it is allowed. That judgement lives in `cairn-tools`.
//!
//! Every `command` node anywhere in the tree is collected, however deeply it
//! is nested: loop bodies, conditions, subshells, `$( )`, backticks, process
//! substitution and function bodies all run, so all of them are leaves.
//! Tree-sitter recovers from errors, so a script with a syntax error still
//! yields what could be read, with [`Script::has_error`] set.

use tree_sitter::{Node, Parser};

/// One shell word.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    /// With quoting and escapes resolved. Expansions (`$HOME`, `$(…)`) stay
    /// as written, because their value is not known here.
    pub text: String,
    /// As it appeared in the source.
    pub raw: String,
    /// It contains an expansion or substitution.
    pub dynamic: bool,
}

/// A redirection attached to a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// `>`, `>>`, `>|`, `&>`, `&>>`, `>&`, `<`, `<<`, `<<<`, …
    pub op: String,
    pub fd: Option<String>,
    pub target: Option<Word>,
    /// The body of a here-document (`<<`), which is data, not commands.
    pub heredoc: Option<String>,
}

impl Redirect {
    /// Whether this redirection can create or change a file.
    #[must_use]
    pub fn writes(&self) -> bool {
        matches!(
            self.op.as_str(),
            ">" | ">>" | ">|" | "&>" | "&>>" | ">&" | "<>"
        )
    }
}

/// One command that will run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    /// `None` for a bare assignment (`x=1`) or a bare redirection.
    pub name: Option<Word>,
    pub args: Vec<Word>,
    /// `NAME=value` prefixes and statements, value as written.
    pub assignments: Vec<(String, String)>,
    pub redirects: Vec<Redirect>,
    /// Commands whose output feeds this one: earlier pipeline stages and the
    /// contents of a process substitution given as an argument.
    pub fed_by: Vec<usize>,
    /// An argument is `<( … )`: the interpreter reads a script from it.
    pub script_from_procsub: bool,
    /// Followed by `&`, itself or through an enclosing construct.
    pub background: bool,
    /// How many enclosing `&` it is under.
    pub bg_depth: u8,
    /// Inside `while true`/`while :`/`until false`/`for ((;;))`.
    pub in_endless_loop: bool,
    /// Inside the body of this function.
    pub in_function: Option<String>,
    /// Inside `$( )`, backticks or `<( )`.
    pub in_substitution: bool,
}

impl Command {
    /// The command name as written, if any.
    #[must_use]
    pub fn name_text(&self) -> Option<&str> {
        self.name.as_ref().map(|w| w.text.as_str())
    }
}

/// What was read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Script {
    pub commands: Vec<Command>,
    /// The parser had to recover from a syntax error.
    pub has_error: bool,
}

/// Parse `src` as bash.
#[must_use]
pub fn parse(src: &str) -> Script {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return Script {
            commands: Vec::new(),
            has_error: true,
        };
    }
    let Some(tree) = parser.parse(src, None) else {
        return Script {
            commands: Vec::new(),
            has_error: true,
        };
    };
    let mut walker = Walker {
        src,
        out: Vec::new(),
    };
    walker.walk(tree.root_node(), &Ctx::default());
    Script {
        commands: walker.out,
        has_error: tree.root_node().has_error(),
    }
}

#[derive(Clone, Default)]
struct Ctx {
    endless: bool,
    function: Option<String>,
    substitution: bool,
}

struct Walker<'a> {
    src: &'a str,
    out: Vec<Command>,
}

fn text<'a>(src: &'a str, node: Node<'_>) -> &'a str {
    &src[node.byte_range()]
}

fn is_expansion(kind: &str) -> bool {
    matches!(
        kind,
        "simple_expansion" | "expansion" | "command_substitution" | "arithmetic_expansion"
    )
}

impl Walker<'_> {
    /// Collect the commands under `node`; returns their indexes.
    fn walk(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        match node.kind() {
            "command" => self.command(node, ctx),
            "pipeline" => self.pipeline(node, ctx),
            "redirected_statement" => self.redirected(node, ctx),
            "function_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.named_child(0))
                    .map(|n| text(self.src, n).to_string());
                let inner = Ctx {
                    function: name,
                    ..ctx.clone()
                };
                self.children(node, &inner)
            }
            "while_statement" | "until_statement" => {
                let endless = node.child_by_field_name("condition").is_some_and(|c| {
                    let t = text(self.src, c).trim();
                    let until = node.kind() == "until_statement";
                    if until {
                        t == "false"
                    } else {
                        t == "true" || t == ":"
                    }
                });
                let inner = Ctx {
                    endless: ctx.endless || endless,
                    ..ctx.clone()
                };
                self.children(node, &inner)
            }
            "c_style_for_statement" => {
                let header = text(self.src, node);
                let endless = header.starts_with("for ((;;))")
                    || header.replace(' ', "").starts_with("for((;;))");
                let inner = Ctx {
                    endless: ctx.endless || endless,
                    ..ctx.clone()
                };
                self.children(node, &inner)
            }
            "command_substitution" | "process_substitution" => {
                let inner = Ctx {
                    substitution: true,
                    ..ctx.clone()
                };
                self.children(node, &inner)
            }
            _ => self.children(node, ctx),
        }
    }

    /// Walk the children in order; a child followed by `&` runs in the
    /// background.
    fn children(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        let mut all = Vec::new();
        let mut cursor = node.walk();
        let kids: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for (i, kid) in kids.iter().enumerate() {
            if !kid.is_named() {
                continue;
            }
            let mut ids = self.walk_with(*kid, ctx);
            if kids.get(i + 1).is_some_and(|next| next.kind() == "&") {
                self.mark_background(&ids);
            }
            all.append(&mut ids);
        }
        all
    }

    fn walk_with(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        let mut ids = self.walk(node, ctx);
        // Endless-loop and function context apply to commands found below;
        // the recursive calls set them, so nothing more is needed here.
        ids.shrink_to_fit();
        ids
    }

    fn mark_background(&mut self, ids: &[usize]) {
        for &id in ids {
            let c = &mut self.out[id];
            c.background = true;
            c.bg_depth = c.bg_depth.saturating_add(1);
        }
    }

    fn pipeline(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        let mut cursor = node.walk();
        let elements: Vec<Node<'_>> = node.children(&mut cursor).filter(Node::is_named).collect();
        let mut all: Vec<usize> = Vec::new();
        let mut earlier: Vec<usize> = Vec::new();
        for element in elements {
            let ids = self.walk(element, ctx);
            for &id in &ids {
                for &from in &earlier {
                    if !self.out[id].fed_by.contains(&from) {
                        self.out[id].fed_by.push(from);
                    }
                }
            }
            earlier.extend(&ids);
            all.extend(ids);
        }
        all
    }

    fn redirected(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        let mut body_ids = Vec::new();
        let mut redirects = Vec::new();
        let mut nested = Vec::new();
        let mut cursor = node.walk();
        for kid in node.children(&mut cursor) {
            match kid.kind() {
                "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                    redirects.push(self.redirect(kid));
                    nested.extend(self.children(kid, ctx));
                }
                _ if kid.is_named() => body_ids.extend(self.walk(kid, ctx)),
                _ => {}
            }
        }
        if body_ids.is_empty() {
            // `> file` on its own is still a write.
            self.out.push(Command {
                redirects,
                in_endless_loop: ctx.endless,
                in_function: ctx.function.clone(),
                in_substitution: ctx.substitution,
                ..Command::default()
            });
            body_ids.push(self.out.len() - 1);
        } else {
            for &id in &body_ids {
                self.out[id].redirects.extend(redirects.iter().cloned());
            }
        }
        body_ids.extend(nested);
        body_ids
    }

    fn redirect(&self, node: Node<'_>) -> Redirect {
        let mut op = String::new();
        let mut fd = None;
        let mut target = None;
        let mut heredoc = None;
        let mut cursor = node.walk();
        for kid in node.children(&mut cursor) {
            match kid.kind() {
                "file_descriptor" => fd = Some(text(self.src, kid).to_string()),
                "heredoc_body" => heredoc = Some(text(self.src, kid).to_string()),
                "heredoc_start" | "heredoc_end" => {}
                _ if !kid.is_named() && op.is_empty() => op = text(self.src, kid).to_string(),
                _ if kid.is_named() && target.is_none() => target = Some(self.word(kid)),
                _ => {}
            }
        }
        if node.kind() == "heredoc_redirect" && op.is_empty() {
            op = "<<".to_string();
        }
        Redirect {
            op,
            fd,
            target,
            heredoc,
        }
    }

    fn command(&mut self, node: Node<'_>, ctx: &Ctx) -> Vec<usize> {
        let index = self.out.len();
        self.out.push(Command {
            in_endless_loop: ctx.endless,
            in_function: ctx.function.clone(),
            in_substitution: ctx.substitution,
            ..Command::default()
        });
        let mut nested: Vec<usize> = Vec::new();
        let mut cursor = node.walk();
        for kid in node.children(&mut cursor) {
            match kid.kind() {
                "variable_assignment" => {
                    let name = kid
                        .child_by_field_name("name")
                        .map_or(String::new(), |n| text(self.src, n).to_string());
                    let value = kid
                        .child_by_field_name("value")
                        .map_or(String::new(), |n| text(self.src, n).to_string());
                    self.out[index].assignments.push((name, value));
                    nested.extend(self.children(kid, ctx));
                }
                "command_name" => {
                    if let Some(inner) = kid.named_child(0) {
                        self.out[index].name = Some(self.word(inner));
                    }
                    nested.extend(self.children(kid, ctx));
                }
                "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                    let r = self.redirect(kid);
                    self.out[index].redirects.push(r);
                    nested.extend(self.children(kid, ctx));
                }
                _ if kid.is_named() => {
                    let word = self.word(kid);
                    self.out[index].args.push(word);
                    let ids = self.walk(kid, ctx);
                    if kid.kind() == "process_substitution" {
                        self.out[index].script_from_procsub = true;
                        for &id in &ids {
                            self.out[index].fed_by.push(id);
                        }
                    }
                    nested.extend(ids);
                }
                _ => {}
            }
        }
        let mut all = vec![index];
        all.extend(nested);
        all
    }

    fn word(&self, node: Node<'_>) -> Word {
        let raw = text(self.src, node).to_string();
        let (text, dynamic) = self.resolve(node);
        Word { text, raw, dynamic }
    }

    /// The value of a word with quoting removed.
    fn resolve(&self, node: Node<'_>) -> (String, bool) {
        let raw = text(self.src, node);
        match node.kind() {
            "word" | "number" => (unescape_bare(raw), false),
            "raw_string" => (raw.trim_matches('\'').to_string(), false),
            "ansi_c_string" => (
                ansi_c(raw.strip_prefix("$'").unwrap_or(raw).trim_end_matches('\'')),
                false,
            ),
            "string" => self.double_quoted(node),
            "concatenation" => {
                let mut out = String::new();
                let mut dynamic = false;
                let mut cursor = node.walk();
                for kid in node.children(&mut cursor) {
                    let (t, d) = self.resolve(kid);
                    out.push_str(&t);
                    dynamic |= d;
                }
                (out, dynamic)
            }
            kind if is_expansion(kind) || kind == "process_substitution" => (raw.to_string(), true),
            _ => (raw.to_string(), false),
        }
    }

    fn double_quoted(&self, node: Node<'_>) -> (String, bool) {
        let start = node.start_byte() + 1;
        let end = node.end_byte().saturating_sub(1).max(start);
        let mut out = String::new();
        let mut dynamic = false;
        let mut at = start;
        let mut cursor = node.walk();
        for kid in node.children(&mut cursor) {
            if is_expansion(kid.kind()) && kid.start_byte() >= at && kid.end_byte() <= end {
                out.push_str(&unescape_double(&self.src[at..kid.start_byte()]));
                out.push_str(text(self.src, kid));
                dynamic = true;
                at = kid.end_byte();
            }
        }
        out.push_str(&unescape_double(&self.src[at..end]));
        (out, dynamic)
    }
}

fn unescape_bare(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\n') | None => {}
                Some(next) => out.push(next),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_double(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('$' | '`' | '"' | '\\') => out.push(chars.next().unwrap_or('\\')),
                Some('\n') => {
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `$'…'` escapes.
fn ansi_c(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else {
            out.push('\\');
            break;
        };
        match e {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(e),
            'x' => {
                let hex = take_while(&mut chars, 2, |h| h.is_ascii_hexdigit());
                push_code(
                    &mut out,
                    u32::from_str_radix(&hex, 16).ok(),
                    &format!("\\x{hex}"),
                );
            }
            'u' | 'U' => {
                let n = if e == 'u' { 4 } else { 8 };
                let hex = take_while(&mut chars, n, |h| h.is_ascii_hexdigit());
                push_code(
                    &mut out,
                    u32::from_str_radix(&hex, 16).ok(),
                    &format!("\\{e}{hex}"),
                );
            }
            '0'..='7' => {
                let mut oct = e.to_string();
                oct.push_str(&take_while(&mut chars, 2, |o| ('0'..='7').contains(&o)));
                push_code(
                    &mut out,
                    u32::from_str_radix(&oct, 8).ok(),
                    &format!("\\{oct}"),
                );
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn take_while(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    max: usize,
    ok: impl Fn(char) -> bool,
) -> String {
    let mut out = String::new();
    while out.len() < max {
        match chars.peek() {
            Some(&c) if ok(c) => {
                out.push(c);
                chars.next();
            }
            _ => break,
        }
    }
    out
}

fn push_code(out: &mut String, code: Option<u32>, fallback: &str) {
    match code.and_then(char::from_u32) {
        Some(c) => out.push(c),
        None => out.push_str(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(script: &Script) -> Vec<String> {
        script
            .commands
            .iter()
            .filter_map(|c| c.name_text().map(str::to_string))
            .collect()
    }

    #[test]
    fn simple_command_words_are_unquoted() {
        let s = parse(r#"echo "a b" 'c d' e\ f"#);
        let c = &s.commands[0];
        assert_eq!(c.name_text(), Some("echo"));
        let args: Vec<&str> = c.args.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(args, ["a b", "c d", "e f"]);
        assert!(!s.has_error);
    }

    #[test]
    fn every_nested_construct_yields_its_commands() {
        let s = parse("for f in $(ls); do sudo chown x $f; done");
        assert_eq!(names(&s), ["ls", "sudo"]);
        let s = parse("if sudo -n true; then echo ok; fi");
        assert_eq!(names(&s), ["sudo", "echo"]);
        let s = parse("x=$(curl a | sh)");
        assert_eq!(names(&s), ["curl", "sh"]);
        assert!(s.commands[0].in_substitution);
        let s = parse("echo `sudo id`");
        assert_eq!(names(&s), ["echo", "sudo"]);
        let s = parse("(cd /tmp && rm -rf x)");
        assert_eq!(names(&s), ["cd", "rm"]);
    }

    #[test]
    fn pipelines_record_what_feeds_what() {
        let s = parse("curl https://x | sh");
        assert_eq!(s.commands[1].fed_by, vec![0]);
        let s = parse("bash <(curl -s https://x)");
        assert!(s.commands[0].script_from_procsub);
        assert_eq!(names(&s), ["bash", "curl"]);
        assert_eq!(s.commands[0].fed_by, vec![1]);
    }

    #[test]
    fn redirections_keep_op_fd_and_target() {
        let s = parse("echo hi > out.txt 2>&1");
        let r = &s.commands[0].redirects;
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].op, ">");
        assert_eq!(r[0].target.as_ref().unwrap().text, "out.txt");
        assert!(r[0].writes());
        assert_eq!(r[1].fd.as_deref(), Some("2"));
        let s = parse("> ~/.bash_history");
        assert_eq!(s.commands.len(), 1);
        assert!(s.commands[0].name.is_none());
        assert_eq!(
            s.commands[0].redirects[0].target.as_ref().unwrap().text,
            "~/.bash_history"
        );
    }

    #[test]
    fn heredoc_bodies_are_data_not_commands() {
        let s = parse("cat <<EOF\nsudo rm -rf /\nEOF");
        assert_eq!(names(&s), ["cat"]);
        let body = s.commands[0].redirects[0].heredoc.as_deref().unwrap();
        assert!(body.contains("sudo rm -rf /"));
    }

    #[test]
    fn ansi_c_quoting_is_decoded() {
        let s = parse(r"$'\x73udo' id");
        assert_eq!(s.commands[0].name_text(), Some("sudo"));
        let s = parse(r"$'\163udo' id");
        assert_eq!(s.commands[0].name_text(), Some("sudo"));
    }

    #[test]
    fn expansions_stay_literal_and_dynamic() {
        let s = parse(r#"rm -rf "${HOME}" $X"#);
        let args = &s.commands[0].args;
        assert_eq!(args[1].text, "${HOME}");
        assert!(args[1].dynamic);
        assert!(args[2].dynamic);
        assert!(!args[0].dynamic);
    }

    #[test]
    fn background_and_endless_loops_are_marked() {
        let s = parse("nohup ./serve.sh &");
        assert!(s.commands[0].background);
        let s = parse("while true; do sleep 1; done &");
        assert!(s.commands.iter().all(|c| c.background));
        assert!(s.commands.iter().all(|c| c.in_endless_loop));
        let s = parse(":(){ :|:& };:");
        assert_eq!(s.commands[0].in_function.as_deref(), Some(":"));
        assert!(s.commands[1].background);
    }

    #[test]
    fn a_syntax_error_is_reported() {
        let s = parse("echo \"unbalanced");
        assert!(s.has_error);
        assert!(!parse("echo ok").has_error);
    }
}
