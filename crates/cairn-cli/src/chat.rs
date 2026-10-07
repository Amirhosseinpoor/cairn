//! `cairn chat` — the interactive session (SPEC §10).
//!
//! One loop owns the terminal: keys, agent events and a tick come in; the
//! screen and the agent's next move go out. The screen itself is
//! `cairn-tui`; a turn is [`headless::run_turn`], the same code `cairn run`
//! uses, with its events, approvals and questions pointed at the interface.

use std::collections::HashMap;
use std::future::Future;
use std::io::{IsTerminal, Stdout};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::{codes, ExitStatus};
use cairn_core::event::EventData;
use cairn_core::message::{Block, Role};
use cairn_core::Mode;
use cairn_tools::{Answer, ApprovalRequest, Approver, EventSink, Question, Questioner, Reply};
use cairn_tui::app::{self, App, Item, Look, NoticeKind};
use cairn_tui::history::History;
use cairn_tui::interact::Command;
use cairn_tui::keymap::{Chords, Keymap};
use cairn_tui::keys::{Code, Key};
use cairn_tui::overlay::{ApprovalAnswer, DiffView, Overlay};
use cairn_tui::theme::{ColorSupport, Glyphs, Theme};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::Notify;

use crate::args::ChatArgs;
use crate::commands::{sessions, Startup};
use crate::headless::{self, ChatLink, OutputFormat};
use crate::output::Fail;
use crate::provide;

/// Frames per animation step: the spinner moves every 80 ms (§10.8).
const TICK: Duration = Duration::from_millis(80);

// ------------------------------------------------------- the agent's side

/// Events from the running turn, waiting for the loop to fold them in.
#[derive(Default)]
struct Inbox {
    events: Mutex<Vec<EventData>>,
    wake: Notify,
}

impl Inbox {
    fn take(&self) -> Vec<EventData> {
        std::mem::take(&mut *self.events.lock().expect("inbox"))
    }
}

impl EventSink for Inbox {
    fn emit(&self, event: EventData) {
        self.events.lock().expect("inbox").push(event);
        self.wake.notify_one();
    }
}

/// One approval waiting for its answer.
#[derive(Default)]
struct Slot {
    answer: Mutex<Option<Answer>>,
    ready: Notify,
}

/// Approvals wait here until the person answers.
#[derive(Default)]
struct Approvals {
    waiting: Mutex<HashMap<String, Arc<Slot>>>,
}

impl Approvals {
    fn answer(&self, request_id: &str, answer: Answer) {
        if let Some(slot) = self.waiting.lock().expect("approvals").remove(request_id) {
            *slot.answer.lock().expect("slot") = Some(answer);
            slot.ready.notify_one();
        }
    }
}

struct UiApprover(Arc<Approvals>);

impl Approver for UiApprover {
    fn ask(&self, request: ApprovalRequest) -> BoxFuture<'_, Answer> {
        let slot = Arc::new(Slot::default());
        {
            let mut waiting = self.0.waiting.lock().expect("approvals");
            // A request that timed out dropped its future and left the slot.
            waiting.retain(|_, slot| Arc::strong_count(slot) > 1);
            waiting.insert(request.request_id, Arc::clone(&slot));
        }
        async move {
            loop {
                if let Some(answer) = slot.answer.lock().expect("slot").take() {
                    return answer;
                }
                slot.ready.notified().await;
            }
        }
        .boxed()
    }
}

/// Questions from `ask_user` are not wired to the interface yet: "nobody
/// there" is the honest answer, and the model is told so.
struct NoQuestions;

impl Questioner for NoQuestions {
    fn ask(&self, _question: Question) -> BoxFuture<'_, Option<Reply>> {
        async { None }.boxed()
    }
}

// -------------------------------------------------------------- terminal

/// Raw mode and the alternate screen, put back on drop, on a panic and when
/// the process is told to stop (§10.9).
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
}

impl Screen {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste
        )?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
        let _ = self.terminal.show_cursor();
    }
}

fn translate(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let m = event.modifiers;
    let code = match event.code {
        KeyCode::Char(c) => Code::Char(c),
        KeyCode::Enter => Code::Enter,
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Delete => Code::Delete,
        KeyCode::Left => Code::Left,
        KeyCode::Right => Code::Right,
        KeyCode::Up => Code::Up,
        KeyCode::Down => Code::Down,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::Tab => Code::Tab,
        KeyCode::BackTab => Code::BackTab,
        KeyCode::Esc => Code::Esc,
        KeyCode::F(n) => Code::F(n),
        _ => return None,
    };
    let shifted_letter = matches!(code, Code::Char(c) if c.is_alphabetic());
    Some(Key {
        code,
        ctrl: m.contains(KeyModifiers::CONTROL),
        alt: m.contains(KeyModifiers::ALT),
        // A capital letter is already in the character.
        shift: m.contains(KeyModifiers::SHIFT) && !shifted_letter && code != Code::BackTab,
    })
}

// ---------------------------------------------------------------- set-up

fn look(startup: &Startup) -> Look {
    let ui = &startup.loaded.config.ui;
    let env = |k: &str| std::env::var(k).ok();
    let mut support = ColorSupport::detect(&env, std::io::stdout().is_terminal());
    if ui.color == Some(false) {
        support = ColorSupport::None;
    }
    let theme = Theme::named(&ui.theme).unwrap_or_else(Theme::cairn_dark);
    let ascii = ui.screen_reader || env("TERM").as_deref() == Some("linux");
    Look {
        theme,
        support,
        glyphs: if ascii {
            Glyphs::ascii()
        } else {
            Glyphs::unicode()
        },
        animation: match ui.animation {
            cairn_config::Animation::On => true,
            cairn_config::Animation::Off => false,
            cairn_config::Animation::Auto => !ui.screen_reader && env("CI").is_none(),
        },
        screen_reader: ui.screen_reader,
        show_reasoning: match ui.show_reasoning {
            cairn_config::ShowReasoning::Always => app::ShowReasoning::Always,
            cairn_config::ShowReasoning::Collapsed => app::ShowReasoning::Collapsed,
            cairn_config::ShowReasoning::Never => app::ShowReasoning::Never,
        },
        diff_layout: match ui.diff_layout {
            cairn_config::DiffLayout::Auto => app::DiffLayout::Auto,
            cairn_config::DiffLayout::Inline => app::DiffLayout::Inline,
            cairn_config::DiffLayout::Side => app::DiffLayout::Side,
        },
        diff_side_by_side_min_width: ui.diff_side_by_side_min_width,
    }
}

/// The files `@` offers: a bounded walk that skips what nobody mentions.
fn list_files(root: &Path) -> Vec<String> {
    const SKIP: [&str; 4] = ["target", "node_modules", "dist", "build"];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0_u8)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || out.len() >= 5_000 {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                if depth < 6 && !SKIP.contains(&name.as_str()) {
                    stack.push((path, depth + 1));
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

fn cycle(mode: Mode) -> Mode {
    match mode {
        Mode::Plan => Mode::Build,
        Mode::Build => Mode::Auto,
        Mode::Auto | Mode::AutoUnsafe => Mode::Plan,
    }
}

fn notice(app: &mut App, kind: NoticeKind, text: impl Into<String>) {
    app.transcript.push(Item::Notice(kind, text.into()));
}

/// The text of messages already in a session, as transcript items.
fn replay(app: &mut App, messages: &[cairn_core::message::Message]) {
    for message in messages {
        let text = message
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            continue;
        }
        match message.role {
            Role::User => app.transcript.push(Item::User(text)),
            Role::Assistant => app.transcript.push(Item::Assistant {
                text,
                streaming: false,
            }),
            _ => {}
        }
    }
}

type TurnFuture = Pin<Box<dyn Future<Output = Result<(Vec<EventData>, String), Fail>>>>;

struct Running {
    future: TurnFuture,
    cancel: CancellationToken,
}

/// `cairn chat`.
pub fn run(cli: &crate::args::Cli, args: &ChatArgs, startup: &Startup) -> Result<i32, Fail> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Fail::usage(
            "`cairn chat` needs an interactive terminal",
            "use `cairn run -p '<prompt>'` in scripts and pipes".to_string(),
        ));
    }
    if std::env::var("TERM").as_deref() == Ok("dumb") {
        return Err(Fail::usage(
            "TERM=dumb cannot draw the interface",
            "set TERM to a real terminal type, or use `cairn run -p '<prompt>'`".to_string(),
        ));
    }
    if cli.offline || startup.loaded.config.network.offline {
        return Err(Fail::new(
            codes::PROV_OFFLINE,
            ExitStatus::Provider,
            "offline: providers are unreachable (`network.offline` / `--offline`)",
            Some("drop --offline to reach a provider".to_string()),
        ));
    }
    // Fail early on a model that cannot be built, before the screen changes.
    let live = provide::build(&startup.loaded.config)?;
    let store = sessions::store(startup);
    let mut session = args.session.clone();
    if session.is_none() && args.resume {
        let filter = cairn_session::ListFilter {
            workspace: Some(startup.workspace().to_path_buf()),
            limit: Some(1),
            ..cairn_session::ListFilter::default()
        };
        session = store
            .list_all(&filter)
            .into_iter()
            .next()
            .map(|s| s.header.session_id);
    }
    let mode = cli
        .mode
        .as_deref()
        .and_then(|m| Mode::parse(m).ok())
        .unwrap_or(startup.loaded.config.mode);

    let mut app = App::new(look(startup), mode, &live.model_id);
    app.workspace = Some({
        let dir = startup.workspace().to_string_lossy().into_owned();
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() && dir.starts_with(&home) => dir.replacen(&home, "~", 1),
            _ => dir,
        }
    });
    app.history = History::load(startup.loaded.paths.state_home.join("history"));
    app.editor = cairn_tui::editor::Editor::new(match startup.loaded.config.input.mode {
        cairn_config::EditorMode::Vi => cairn_tui::editor::EditMode::Vi,
        cairn_config::EditorMode::Emacs => cairn_tui::editor::EditMode::Emacs,
    });
    if let Some(id) = &session {
        let path = store.find(id).ok_or_else(|| {
            Fail::not_found(
                format!("session '{id}' not found"),
                "run `cairn sessions` to list stored sessions".to_string(),
            )
        })?;
        let file = store.load(&path).map_err(Fail::from_cairn)?;
        let state = cairn_agent::transcript::resume_state(&file);
        replay(&mut app, &state.messages);
        app.session = Some(id.clone());
    }
    let keybindings = startup.loaded.paths.config_home.join("keybindings.toml");
    let (keymap, problems) = match std::fs::read_to_string(&keybindings) {
        Ok(text) => Keymap::with_overrides(&text),
        Err(_) => (Keymap::default(), Vec::new()),
    };
    for problem in &problems {
        notice(&mut app, NoticeKind::Warning, problem.to_string());
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Fail::new(
                "ERR_GENERIC",
                ExitStatus::Generic,
                format!("cannot start the async runtime: {e}"),
                None,
            )
        })?;
    let session_ctx = Ctx {
        startup,
        store,
        live_model: live.model_id.clone(),
        session,
    };
    let result = runtime.block_on(event_loop(app, keymap, session_ctx));
    result.map_err(|e| {
        Fail::new(
            "ERR_GENERIC",
            ExitStatus::Generic,
            format!("terminal error: {e}"),
            None,
        )
    })
}

/// What the loop keeps between turns.
struct Ctx<'a> {
    startup: &'a Startup,
    store: cairn_session::Store,
    live_model: String,
    session: Option<String>,
}

impl Ctx<'_> {
    /// One turn, as a future the loop polls beside the keyboard.
    fn start(&self, prompt: &str, mode: Mode, link: &ChatLink) -> Result<Running, Fail> {
        let startup = self.startup;
        let live = provide::build(&startup.loaded.config)?;
        let plan = headless::Plan {
            prompt: Some(prompt.to_string()),
            format: OutputFormat::Json,
            live,
            store: self.store.clone(),
            workspace: startup.workspace().to_path_buf(),
            mode: mode.as_str().to_string(),
            session: self.session.clone(),
            input: Vec::new(),
            auto_recover: startup.loaded.config.session.auto_recover,
            quiet: true,
            cache_home: startup.loaded.paths.cache_home.clone(),
            config: startup.loaded.config.clone(),
            paths: startup.loaded.paths.clone(),
            run_mode: mode,
            allow_ask: false,
            max_iterations: None,
            chat: Some(link.clone()),
        };
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let future = async move {
            let turn = headless::run_turn(&plan, token).await?;
            let id = turn.session_id.clone();
            Ok((headless::end_events(&plan, turn), id))
        };
        Ok(Running {
            future: Box::pin(future),
            cancel,
        })
    }
}

fn error_event(fail: &Fail) -> EventData {
    EventData::Error {
        code: fail.code.to_string(),
        message: fail.message.clone(),
        recoverable: true,
        hint: fail.hint.clone().unwrap_or_default(),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one select, one arm per source of work"
)]
async fn event_loop(mut app: App, keymap: Keymap, mut ctx: Ctx<'_>) -> std::io::Result<i32> {
    let mut screen = Screen::enter()?;
    let inbox = Arc::new(Inbox::default());
    let approvals = Arc::new(Approvals::default());
    let link = ChatLink {
        events: Arc::clone(&inbox) as Arc<dyn EventSink>,
        approver: Arc::new(UiApprover(Arc::clone(&approvals))),
        questioner: Arc::new(NoQuestions),
    };
    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    let mut chords = Chords::default();
    let mut running: Option<Running> = None;
    let mut files: Option<Vec<String>> = None;
    let mut dirty = true;
    let mut exit: Option<i32> = None;

    while exit.is_none() {
        if dirty {
            let height = screen.terminal.size()?.height;
            app.page_lines = usize::from(height.saturating_sub(8)).max(1);
            screen.terminal.draw(|frame| {
                let area = frame.area();
                let cursor = cairn_tui::view::draw(&app, frame.buffer_mut(), area);
                if let Some((x, y)) = cursor {
                    frame.set_cursor_position((x, y));
                }
            })?;
            dirty = false;
        }
        let mut commands: Vec<Command> = Vec::new();
        tokio::select! {
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    if let Some(key) = translate(key) {
                        commands = app.on_key(key, &keymap, &mut chords);
                    }
                    dirty = true;
                }
                Some(Ok(Event::Paste(text))) => {
                    app.on_paste(&text);
                    dirty = true;
                }
                Some(Ok(Event::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => exit = Some(0),
            },
            () = inbox.wake.notified() => {}
            _ = ticker.tick(), if app.animating() => {
                app.tick();
                dirty = true;
            }
            done = async { running.as_mut().expect("guarded").future.as_mut().await }, if running.is_some() => {
                running = None;
                match done {
                    Ok((events, id)) => {
                        ctx.session = Some(id.clone());
                        app.session = Some(id);
                        for event in events {
                            app.apply(&event);
                        }
                    }
                    Err(fail) => {
                        app.apply(&error_event(&fail));
                        app.running = None;
                    }
                }
                dirty = true;
            }
        }
        for event in inbox.take() {
            app.apply(&event);
            dirty = true;
        }
        for command in commands {
            dirty = true;
            match command {
                Command::Submit(text) => match ctx.start(&text, app.mode, &link) {
                    Ok(turn) => running = Some(turn),
                    Err(fail) => {
                        app.apply(&error_event(&fail));
                        app.running = None;
                    }
                },
                Command::Cancel => {
                    if let Some(r) = &running {
                        r.cancel.cancel();
                    }
                }
                Command::ForceCancel => {
                    if running.take().is_some() {
                        app.apply(&EventData::TurnEnded {
                            turn_id: 0,
                            status: cairn_core::event::TurnStatus::Cancelled,
                            duration_ms: 0,
                            cost_usd: 0.0,
                        });
                    }
                }
                Command::CycleMode => {
                    app.mode = cycle(app.mode);
                    let name = app.mode.as_str();
                    notice(
                        &mut app,
                        NoticeKind::Info,
                        format!("Mode: {name} (applies to the next turn)."),
                    );
                }
                Command::Quit { .. } => exit = Some(0),
                Command::Approval { request_id, answer } => {
                    let mapped = match answer {
                        ApprovalAnswer::Once => Answer::Once,
                        ApprovalAnswer::Always => Answer::Always,
                        ApprovalAnswer::Deny => Answer::Deny,
                        ApprovalAnswer::Edit => {
                            notice(
                                &mut app,
                                NoticeKind::Info,
                                "Denied. Edit your request and send it again.",
                            );
                            Answer::Deny
                        }
                    };
                    approvals.answer(&request_id, mapped);
                }
                Command::MentionQuery(_) => {
                    let list = files.get_or_insert_with(|| list_files(ctx.startup.workspace()));
                    app.mention_candidates.clone_from(list);
                }
                Command::Slash { name, args } => {
                    slash(&mut app, name, &args, &ctx, &mut exit);
                }
                Command::ClearView
                | Command::Hunk { .. }
                | Command::AcceptAllHunks
                | Command::Plan(_)
                | Command::PasteImage
                | Command::QuickOpen => {
                    if matches!(command, Command::PasteImage | Command::QuickOpen) {
                        notice(
                            &mut app,
                            NoticeKind::Info,
                            "Not available in this build yet.",
                        );
                    }
                }
            }
        }
    }
    if let Some(r) = &running {
        r.cancel.cancel();
    }
    drop(screen);
    Ok(exit.unwrap_or(0))
}

/// The slash commands this build runs itself. The rest say so.
fn slash(app: &mut App, name: &str, args: &str, ctx: &Ctx<'_>, exit: &mut Option<i32>) {
    match name {
        "help" => app.overlay = Overlay::Help,
        "quit" => *exit = Some(0),
        "clear" => {
            app.transcript.clear();
            app.scroll = 0;
        }
        "mode" | "plan" | "auto" => {
            let wanted = match name {
                "plan" => Some("plan"),
                "auto" => Some("auto"),
                _ if args.is_empty() => None,
                _ => Some(args),
            };
            match wanted.map(Mode::parse) {
                None => {
                    let current = app.mode.as_str();
                    notice(app, NoticeKind::Info, format!("Mode: {current}."));
                }
                Some(Ok(mode)) => {
                    app.mode = mode;
                    notice(
                        app,
                        NoticeKind::Info,
                        format!("Mode: {} (applies to the next turn).", mode.as_str()),
                    );
                }
                Some(Err(message)) => notice(app, NoticeKind::Warning, message),
            }
        }
        "model" => {
            let model = ctx.live_model.clone();
            notice(
                app,
                NoticeKind::Info,
                format!("Model: {model}. Change it in config (`model`) or with --model."),
            );
        }
        "cost" => {
            let cost = app
                .cost_usd
                .map_or_else(|| "unknown".to_string(), |c| format!("${c:.4}"));
            let (tin, tout) = (app.tokens_in, app.tokens_out);
            notice(
                app,
                NoticeKind::Info,
                format!("Session: {tin} tokens in, {tout} out, cost {cost}."),
            );
        }
        "diff" => match git_diff(ctx.startup.workspace()) {
            Some(text) => {
                let files = cairn_tui::overlay::parse_unified(&text);
                if files.is_empty() {
                    notice(app, NoticeKind::Info, "No changes.");
                } else {
                    app.overlay = Overlay::Diff(DiffView::new(files));
                }
            }
            None => notice(
                app,
                NoticeKind::Warning,
                "Cannot read the diff here (not a git repository?).",
            ),
        },
        other => notice(
            app,
            NoticeKind::Info,
            format!("/{other} is not available in this build yet."),
        ),
    }
}

fn git_diff(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--no-color", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
