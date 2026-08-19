use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

use crate::state::{LauncherState, SessionStatus};
use crate::terminal::{TabId, TabInfo, TabTarget, WindowId};

use super::format::{ansi_to_lines, base64_encode, default_dir_emoji_and_color, format_coarse_age};
use super::{Action, App, Cursor, InputMode};

// -- Test harness --

/// Redirect `state_dir()` to a per-process tempdir before any test touches
/// disk. Without this, `commit_dir_edit` etc. would clobber the user's real
/// `~/.local/state/captain-miao/` files.
fn redirect_state_dir_for_tests() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("captain-miao-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: ONCE serializes the set; every TestDashboard::new caller
        // blocks here until set_var returns, so no concurrent reader observes
        // a half-written value. After this point the var is read-only.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &dir);
        }
    });
}

/// Serialises the tests that read `window-bindings.json` back off disk.
///
/// `redirect_state_dir_for_tests` gives the whole test *process* one state dir,
/// so every test shares that file while cargo runs them on parallel threads. A
/// test that writes it and then reads it back can otherwise observe another
/// test's projection — which fails in both directions: a missing precondition,
/// or worse, a post-condition that passes because someone else's write happened
/// to omit the token being asserted about.
fn bindings_file_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Poisoning is irrelevant here — the guard protects a scratch file, and a
    // panicking test has already failed on its own terms.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct TestDashboard {
    app: App,
    terminal: Terminal<TestBackend>,
}

impl TestDashboard {
    fn new(width: u16, height: u16) -> Self {
        redirect_state_dir_for_tests();
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.home_dir = "/home/test".to_string();
        // Tests run against fabricated paths; treat every path as an existing
        // directory by default so workdir-picker submits aren't rejected.
        // Individual tests override this to exercise the validation/precedence.
        app.dir_exists = |_| true;
        // `App::new` seeds this from `terminal::get()`, which detects the
        // backend from the *test process's* environment — so on a dev machine
        // inside zellij it would come up all-false. Pin the kitty-like
        // default; tests that exercise an unsupported path flip a flag
        // themselves.
        app.capabilities = crate::terminal::Capabilities::default();
        // `App::new` seeds this from the test process's terminal env (zellij/kitty/
        // none — nondeterministic across machines). Pin it to `None` so the default
        // harness treats every terminal-less mock session/binding as same-terminal
        // (today's behavior); foreign-terminal tests set it explicitly.
        app.terminal_identity = None;
        Self { app, terminal }
    }

    fn set_sessions(&mut self, sessions: Vec<LauncherState>) {
        // Mirror the live spawn path: a dashboard-spawned session has a recorded
        // (host, token) → window binding. Seed one for each local session that
        // carries a launch_id + window_id, so `window_id_for_session` resolves in
        // tests as it does live (the launcher no longer self-reports window_id for
        // such sessions). Remote sessions manage their own bindings in-test.
        for s in &sessions {
            if s.host.is_local()
                && let (Some(token), Some(wid)) = (&s.launch_id, &s.window_id)
            {
                self.app.window_bindings.record(
                    crate::state::HostId::local(),
                    token.clone(),
                    wid.clone(),
                );
            }
        }
        self.app.sessions = sessions;
        // Invalidate like `reload_sessions` does, and for the same reason: the
        // visible order is a cached list of *indices into `sessions`*, so
        // replacing that Vec without bumping the version leaves a projection of
        // the previous rows looking current — which reads as the old order and
        // panics outright once the row count drops. A harness that skipped this
        // is why the live `reload_sessions` ordering bug had no failing test.
        self.app.mark_dirty(Cursor::HoldIndex);
        let len = self.app.visible_sessions().len();
        if len > 0 && self.app.table_state.selected().is_none() {
            self.app.table_state.select(Some(0));
        }
    }

    fn render(&mut self) -> String {
        self.terminal.draw(|f| self.app.draw(f)).unwrap();
        buffer_to_string(self.terminal.backend().buffer())
    }

    fn press(&mut self, code: KeyCode) -> Option<Action> {
        self.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_ctrl(&mut self, code: KeyCode) -> Option<Action> {
        self.app
            .handle_key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    /// Dispatch a left-button-down at a screen cell. Two calls at the same cell
    /// in quick succession (well under the 500ms threshold in a test) register
    /// as a double-click.
    fn click(&mut self, column: u16, row: u16) -> Option<Action> {
        self.app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn selected(&self) -> Option<usize> {
        self.app.table_state.selected()
    }
}

fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let mut output = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output
}

/// Position of an **ASCII** needle in the rendered buffer, scanning cell by
/// cell rather than over `buffer_to_string` so the caller gets coordinates it
/// can ask for a style at. Cell-wise because a wide glyph earlier in the row
/// makes a byte offset into the flattened line meaningless.
fn find_cell(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    let bytes = needle.as_bytes();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width.saturating_sub(bytes.len() as u16) {
            if bytes
                .iter()
                .enumerate()
                .all(|(i, b)| buf[(x + i as u16, y)].symbol().as_bytes() == [*b])
            {
                return Some((x, y));
            }
        }
    }
    None
}

// -- Mock builders --

fn session(pid: u32, cwd: &str, status: SessionStatus) -> LauncherState {
    LauncherState {
        agent: crate::agent::AgentControl::Claude,
        launcher_pid: pid,
        session_id: Some(format!("sess-{pid}")),
        child_session_ids: Vec::new(),
        window_id: Some(WindowId::from(pid as u64 * 100)),
        tab_id: Some(TabId::from(pid as u64)),
        cwd: cwd.to_string(),
        status,
        last_tool: None,
        updated_at: LauncherState::now(),
        active_since: None,
        last_prompt: None,
        child_pid: None,
        last_error: None,
        context_tokens: None,
        model: None,
        name: None,
        first_prompt: None,
        pool_session: None,
        // A dashboard-spawned local session: it carries a launch_id and the
        // dashboard holds the matching (local, launch_id) → window binding.
        // `set_sessions` seeds that binding from `window_id`, so resolution works
        // in tests exactly as it does live. A few tests clear this to exercise the
        // hand-launched fallback (no launch_id, self-reported window_id).
        launch_id: Some(format!("launch-{pid}")),
        terminal: None,
        terminfo: None,
        flags: None,
        attached: None,
        host: crate::state::HostId::local(),
    }
}

fn session_with_tool(pid: u32, cwd: &str, tool: &str) -> LauncherState {
    let mut s = session(pid, cwd, SessionStatus::Active);
    s.last_tool = Some(tool.to_string());
    s
}

fn session_with_prompt(pid: u32, cwd: &str, status: SessionStatus, prompt: &str) -> LauncherState {
    let mut s = session(pid, cwd, status);
    s.last_prompt = Some(prompt.to_string());
    s
}

// -- Tests --

#[test]
fn auto_title_from_first_prompt_is_truncated() {
    use super::format::session_display_name;
    use crate::agent::SessionIndex;
    use std::collections::HashMap;
    use unicode_width::UnicodeWidthStr;

    let index = SessionIndex::default();
    let names = HashMap::new();

    let mut s = session(1, "/tmp", SessionStatus::Idle);

    // A short first prompt with no rename passes through unchanged.
    s.first_prompt = Some("fix the flaky test".into());
    assert_eq!(
        session_display_name(&s, &index, &names),
        "fix the flaky test"
    );

    // A long, multi-line opener is flattened to one line and clipped to the
    // auto-title budget (60 cells) with a trailing ellipsis.
    s.first_prompt = Some(
        "please refactor the launcher\nso the transcript and session-file watchers share a channel"
            .into(),
    );
    let title = session_display_name(&s, &index, &names);
    assert!(
        title.width() <= 60,
        "got width {} for {title:?}",
        title.width()
    );
    assert!(title.ends_with('…'));
    assert!(!title.contains('\n'));

    // A deliberate rename is a title by intent — returned in full even when it
    // exceeds the auto-title budget.
    let rename = "the user typed this deliberately long session name out in full themselves";
    assert!(rename.chars().count() > 60);
    s.name = Some(rename.into());
    assert_eq!(session_display_name(&s, &index, &names), rename);
}

#[test]
fn empty_dashboard() {
    let mut d = TestDashboard::new(120, 20);
    let out = d.render();
    assert!(out.contains("captain-miao"), "should show title");
    assert!(out.contains("0 sessions"), "should show zero count");
    assert!(out.contains("Enter"), "should show keybindings");
}

#[test]
fn shows_tool_name_when_active() {
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session_with_tool(1, "/home/test/proj", "Bash")]);
    let out = d.render();
    assert!(out.contains("Bash"));
}

#[test]
fn shows_all_status_variants() {
    let mut d = TestDashboard::new(130, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Starting),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Compacting),
        session(4, "/home/test/d", SessionStatus::Idle),
        session(5, "/home/test/f", SessionStatus::WaitingForApproval),
        session(6, "/home/test/g", SessionStatus::WaitingForDecision),
        session(7, "/home/test/h", SessionStatus::BackgroundActive),
        session(8, "/home/test/i", SessionStatus::BackgroundServer),
        session(9, "/home/test/j", SessionStatus::FailedToStart),
    ]);
    let out = d.render();
    assert!(out.contains("9 sessions"));
    assert!(out.contains("Starting"));
    assert!(out.contains("Active"));
    assert!(out.contains("Compacting"));
    assert!(out.contains("Idle"));
    assert!(out.contains("Approval"));
    assert!(out.contains("Decision"));
    assert!(out.contains("Task"));
    assert!(out.contains("Server"));
    assert!(out.contains("Failed"));
}

#[test]
fn navigation_clamps_at_ends() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);

    assert_eq!(d.selected(), Some(0));
    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(1));
    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(2));
    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(2), "j at the last row is a no-op");

    d.press(KeyCode::Char('k'));
    assert_eq!(d.selected(), Some(1));
    d.press(KeyCode::Char('k'));
    assert_eq!(d.selected(), Some(0));
    d.press(KeyCode::Char('k'));
    assert_eq!(d.selected(), Some(0), "k at the first row is a no-op");
}

#[test]
fn enter_returns_focus_action() {
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let action = d.press(KeyCode::Enter);
    // window_id = pid * 100 = 100
    assert!(matches!(action, Some(Action::FocusWindow(w)) if w == WindowId::from(100)));
}

// -- Mouse input --

#[test]
fn click_selects_correct_row_when_scrolled() {
    // Regression for CROSS-1: the click→row mapping must add the TableState
    // scroll offset, or every click on a scrolled table lands `offset` rows too
    // high (row 0 no matter where you click).
    let mut d = TestDashboard::new(120, 24);
    let sessions: Vec<_> = (1..=40)
        .map(|pid| session(pid, &format!("/home/test/p{pid}"), SessionStatus::Idle))
        .collect();
    d.set_sessions(sessions);
    // Selecting a row near the bottom makes ratatui scroll the table (non-zero
    // offset) to keep it visible; the offset is written during render.
    d.app.table_state.select(Some(39));
    d.render();

    let rect = d.app.last_table_rect.expect("table rect set on render");
    let offset = d.app.table_state.offset();
    assert!(
        offset > 0,
        "selecting the last row should have scrolled the table"
    );
    let visible_rows = rect.height.saturating_sub(3);
    assert!(
        visible_rows >= 2,
        "test needs at least two visible data rows"
    );

    // Click the second data row: top border (1) + header (1) chrome, then one
    // more. With the offset applied this must resolve to `offset + 1`, not `1`.
    let action = d.click(rect.x + 1, rect.y + 3);
    assert!(action.is_none(), "a single click only selects");
    assert_eq!(d.app.table_state.selected(), Some(offset + 1));
}

#[test]
fn double_click_focuses_local_row_like_enter() {
    // The second click of a double-click routes through the shared focus body,
    // so a local row focuses its window exactly as Enter does.
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.render();
    let rect = d.app.last_table_rect.expect("table rect set on render");
    let (col, row) = (rect.x + 1, rect.y + 2);

    assert!(d.click(col, row).is_none(), "first click only selects");
    // window_id = pid * 100 = 100, matching enter_returns_focus_action.
    match d.click(col, row) {
        Some(Action::FocusWindow(w)) => assert_eq!(w, WindowId::from(100)),
        other => panic!("expected FocusWindow(100), got {other:?}"),
    }
}

#[test]
fn double_click_attaches_running_remote_like_enter() {
    // CROSS-6/STATE-9: a double-click on an unattached running remote row now
    // attaches over ssh (like Enter), instead of silently no-oping.
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/proj", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-claude-42-1".into());
    d.set_sessions(vec![s]);
    d.render();
    let rect = d.app.last_table_rect.expect("table rect set on render");
    let (col, row) = (rect.x + 1, rect.y + 2);

    assert!(d.click(col, row).is_none(), "first click only selects");
    match d.click(col, row) {
        Some(Action::AttachRemoteRunning {
            host, pool_session, ..
        }) => {
            assert_eq!(host, HostId("box".into()));
            assert_eq!(pool_session, "cm-claude-42-1");
        }
        other => panic!("expected AttachRemoteRunning, got {other:?}"),
    }
}

#[test]
fn o_targets_focused_session_tab() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/myproj", SessionStatus::Active)]);

    let action = d.press(KeyCode::Char('o'));
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => {
            assert_eq!(cwd, "/home/test/myproj");
        }
        _ => panic!("expected NewSessionSplit"),
    }
}

#[test]
fn o_opens_workdir_picker_when_no_session_selected() {
    let mut d = TestDashboard::new(120, 10);
    let action = d.press(KeyCode::Char('o'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(picker_input_text(&d.app).is_empty());
}

#[test]
fn q_sets_should_quit() {
    let mut d = TestDashboard::new(120, 10);
    assert!(!d.app.should_quit);
    d.press(KeyCode::Char('q'));
    assert!(d.app.should_quit);
}

#[test]
fn status_msg_shown_in_footer() {
    // Wide enough that the full hint ribbon plus the trailing status message
    // fit without clipping (the padded two-tone hints eat more width than the
    // old plain strip).
    let mut d = TestDashboard::new(160, 10);
    d.app.status_msg = Some("Launched window 42".to_string());
    let out = d.render();
    assert!(out.contains("Launched window 42"));
}

/// The footer shows one status at a time; `Space m` is where the ones it
/// replaced are still readable. What makes that work is that every status goes
/// through `set_status` — so the log is fed there, not at each call site.
#[test]
fn space_m_opens_the_message_log_and_esc_closes_it() {
    let mut d = TestDashboard::new(120, 20);
    d.app.set_status("Launched window 42".to_string(), false);
    d.app
        .set_status("Kill failed: host is unreachable".to_string(), true);

    // The footer only carries the newest of the two.
    let footer = d.render();
    assert!(footer.contains("Kill failed"), "{footer}");
    assert!(!footer.contains("Launched window 42"), "{footer}");

    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('m'));
    assert_eq!(d.app.input_mode, InputMode::Messages);
    let out = d.render();
    assert!(out.contains("Messages"), "{out}");
    assert!(out.contains("Launched window 42"), "{out}");
    assert!(out.contains("Kill failed"), "{out}");

    // A stray key is swallowed by the pager rather than reaching the list
    // underneath — `x` here must not kill the selected session.
    d.press(KeyCode::Char('x'));
    assert_eq!(d.app.input_mode, InputMode::Messages);

    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.message_view.is_none());
}

/// The popup opens parked on the newest message: that's what the user pressed
/// the key to read. `usize::MAX` is only a request — the draw is what knows the
/// line count, so the clamp has to survive the first frame.
#[test]
fn the_message_log_opens_at_the_newest_entry() {
    let mut d = TestDashboard::new(80, 14);
    for i in 0..40 {
        d.app.set_status(format!("message {i}"), false);
    }
    d.app.open_message_log();
    let out = d.render();
    assert!(out.contains("message 39"), "{out}");
    assert!(!out.contains("message 0 "), "{out}");

    // `g` goes to the top, and the oldest kept entry is there.
    d.press(KeyCode::Char('g'));
    let out = d.render();
    assert!(out.contains("message 0"), "{out}");
    assert_eq!(d.app.message_view.as_ref().map(|v| v.scroll), Some(0));

    // `G` parks at the bottom again, clamped to a real offset by the draw.
    d.press(KeyCode::Char('G'));
    let out = d.render();
    assert!(out.contains("message 39"), "{out}");
    let view = d.app.message_view.as_ref().expect("still open");
    assert!(
        view.scroll < 40,
        "scroll clamped to the last page: {}",
        view.scroll
    );
}

#[test]
fn space_a_picker_sets_default_new_session_backend() {
    use crate::agent::AgentControl;
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);

    // Default backend is Claude: header cluster shows it and `o` launches it.
    assert_eq!(d.app.new_session_agent, AgentControl::Claude);
    assert!(d.render().contains("Default agent: Claude"));
    match d.press(KeyCode::Char('o')) {
        Some(Action::NewSessionSplit { agent, .. }) => assert_eq!(agent, AgentControl::Claude),
        _ => panic!("expected NewSessionSplit"),
    }

    // `Space a` opens the backend picker (cursor on the current default).
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('a'));
    assert!(d.app.pending_prefix.is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);

    // Move onto Codex and select it: the default flips and the next `o` follows.
    d.press(KeyCode::Down);
    d.press(KeyCode::Enter);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert_eq!(d.app.new_session_agent, AgentControl::Codex);
    assert!(d.render().contains("Default agent: Codex"));
    match d.press(KeyCode::Char('o')) {
        Some(Action::NewSessionSplit { agent, .. }) => assert_eq!(agent, AgentControl::Codex),
        _ => panic!("expected NewSessionSplit"),
    }
}

#[test]
fn workdir_picker_ctrl_t_overrides_backend_for_this_launch() {
    use crate::agent::AgentControl;
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);

    // Default stays Claude; `O` opens the workdir picker titled for Claude.
    d.press(KeyCode::Char('O'));
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(d.render().contains("New Claude Session"));

    // Ctrl-t flips the backend for this launch only — title follows, the
    // persistent default does not.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(d.render().contains("New Codex Session"));
    assert_eq!(d.app.new_session_agent, AgentControl::Claude);

    // Submitting a free-form path launches with the overridden backend.
    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }
    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { agent, .. }) => assert_eq!(agent, AgentControl::Codex),
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn worktree_paths_resolve_to_their_repo_root() {
    use crate::app::{dir_mark_key, display_basename, split_worktree};

    assert_eq!(
        split_worktree("/home/u/proj/.claude/worktrees/feature-auth"),
        ("/home/u/proj", Some("feature-auth"))
    );
    // Host-canonical `~` paths are the wire form, so they must work verbatim.
    assert_eq!(
        split_worktree("~/proj/.claude/worktrees/wt"),
        ("~/proj", Some("wt"))
    );
    // A name may contain `/` (the agent allows `feature/auth`), so the whole
    // remainder is the name rather than its first segment.
    assert_eq!(
        split_worktree("~/proj/.claude/worktrees/feature/auth"),
        ("~/proj", Some("feature/auth"))
    );
    assert_eq!(split_worktree("/home/u/proj"), ("/home/u/proj", None));
    assert_eq!(split_worktree("/home/u/proj/"), ("/home/u/proj", None));
    // The container itself is not a worktree.
    assert_eq!(
        split_worktree("/home/u/proj/.claude/worktrees"),
        ("/home/u/proj/.claude/worktrees", None)
    );

    // Every worktree of a repo shares one directory mark — the repo's.
    assert_eq!(
        dir_mark_key("~/proj/.claude/worktrees/a"),
        dir_mark_key("~/proj/.claude/worktrees/b")
    );
    assert_eq!(dir_mark_key("~/proj/.claude/worktrees/a"), "~/proj");

    // Tab titles name both halves: `feature-auth` alone doesn't say which repo.
    assert_eq!(display_basename("~/proj/.claude/worktrees/wt"), "proj@wt");
    assert_eq!(display_basename("~/proj"), "proj");
}

#[test]
fn work_tab_title_names_the_worktree_and_its_repo() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 18);
    let box_host = HostId("box".into());
    d.app.host_icons.insert(box_host.clone(), "🚀".into());
    let wt = "~/proj/.claude/worktrees/feature-auth";
    assert_eq!(
        d.app.work_tab_title(&HostId::local(), wt),
        "proj@feature-auth"
    );
    assert_eq!(
        d.app.work_tab_title(&box_host, wt),
        "[🚀] proj@feature-auth",
        "a remote worktree tab keeps both halves behind the host icon"
    );
    // Two worktrees of one repo get distinct tabs, so `w` in one can't land a
    // shell on the other's branch.
    assert_ne!(
        d.app.work_tab_title(&HostId::local(), wt),
        d.app
            .work_tab_title(&HostId::local(), "~/proj/.claude/worktrees/other")
    );
}

#[test]
fn workdir_picker_ctrl_g_arms_a_worktree_for_this_launch() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);

    // Off by default, and the footer offers the key for Claude.
    d.press(KeyCode::Char('O'));
    assert_eq!(d.app.input_mode, InputMode::Picker);
    let out = d.render();
    assert!(out.contains("Ctrl-g"), "footer should offer the key: {out}");
    assert!(!out.contains("Worktree"), "off by default: {out}");

    // Ctrl-w is readline delete-previous-word on the path input — which is why
    // the toggle isn't spelled with the obvious mnemonic. Pinned here as well
    // as in its own test, because this is the picker the collision would hit.
    for c in "/tmp/aa /tmp/bb".chars() {
        d.press(KeyCode::Char(c));
    }
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }

    // Ctrl-g arms it and drops into the name field, so Enter alone commits an
    // empty name — the agent-generated case — and leaves the path untouched.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    let out = d.render();
    assert!(out.contains("Worktree"), "armed: {out}");
    assert!(out.contains("Esc cancel"), "naming: {out}");
    d.press(KeyCode::Enter);
    let out = d.render();
    assert!(
        out.contains("auto-named"),
        "empty name reads as auto: {out}"
    );

    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { worktree, cwd, .. }) => {
            assert_eq!(worktree.as_deref(), Some(""));
            assert_eq!(cwd, "/tmp/x", "naming must not disturb the path");
        }
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn workdir_picker_names_the_worktree() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.press(KeyCode::Char('O'));
    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }

    // While naming, ordinary letters build the name rather than filtering the
    // list, and the keys that would otherwise switch agent/host are inert.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    for c in "feature-auth".chars() {
        d.press(KeyCode::Char(c));
    }
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let out = d.render();
    assert!(out.contains("feature-auth"), "{out}");
    assert!(
        out.contains("New Claude Session"),
        "Ctrl-t must not switch agent mid-name: {out}"
    );

    d.press(KeyCode::Enter);
    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { worktree, cwd, .. }) => {
            assert_eq!(worktree.as_deref(), Some("feature-auth"));
            assert_eq!(cwd, "/tmp/x");
        }
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn workdir_picker_esc_while_naming_disarms_the_worktree() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.press(KeyCode::Char('O'));
    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }

    // Esc while naming disarms rather than closing the picker: `Ctrl-g` opened
    // the field, so Esc is how that press is taken back.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Picker, "picker stays open");
    let out = d.render();
    assert!(!out.contains("Worktree"), "disarmed: {out}");

    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { worktree, .. }) => assert_eq!(worktree, None),
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn workdir_picker_hides_worktrees_for_an_agent_without_them() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.press(KeyCode::Char('O'));

    // Arm it on Claude, then switch to Codex: the request is dropped rather
    // than held invisibly, since Codex has no worktree flag to spend it on.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert!(d.render().contains("Worktree"));
    // Leave the name field first: while naming, Ctrl-t is deliberately inert.
    d.press(KeyCode::Enter);
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let out = d.render();
    assert!(out.contains("New Codex Session"));
    assert!(!out.contains("Worktree"), "disarmed on switch: {out}");
    assert!(!out.contains("Ctrl-g"), "hint hidden for Codex: {out}");

    // And pressing it on Codex reports rather than silently doing nothing.
    // Asserted on the status field, not the frame: an open picker's hint ribbon
    // owns the footer row the status would otherwise paint into.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert_eq!(
        d.app.status_msg.as_deref(),
        Some("Codex has no worktree support")
    );
    assert!(d.app.status_is_error);

    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }
    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { worktree, .. }) => assert_eq!(worktree, None),
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn workdir_picker_defaults_to_local_host() {
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);

    d.press(KeyCode::Char('O'));
    assert!(d.render().contains("New Claude Session"));
    // With only the local host configured, Ctrl-h is a harmless no-op (no
    // remote to cycle to) and the title carries no host suffix.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert!(d.render().contains("New Claude Session"));

    for c in "/tmp/x".chars() {
        d.press(KeyCode::Char(c));
    }
    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { host, .. }) => assert!(host.is_local()),
        other => panic!("expected NewSessionSplit, got {other:?}"),
    }
}

#[test]
fn resume_picker_names_the_host_it_lists() {
    use crate::agent::{AgentControl, ResumeCandidate};
    use crate::state::HostId;
    use std::time::SystemTime;

    let cand = |id: &str, cwd: &str| ResumeCandidate {
        agent: AgentControl::Claude,
        session_id: id.to_string(),
        cwd: cwd.to_string(),
        first_prompt: Some(format!("prompt for {id}")),
        custom_title: None,
        git_branch: None,
        mtime: SystemTime::UNIX_EPOCH,
    };

    // One host at a time (§9): the title states the scope, so the list is never
    // an implicit union the user has to infer.
    let mut d = TestDashboard::new(140, 14);
    d.app.open_resume_picker(
        HostId("buildbox".into()),
        vec![cand("bbbb2222", "/srv/remote-proj")],
    );
    assert_eq!(d.app.input_mode, InputMode::Picker);

    let out = d.render();
    assert!(
        out.contains("Resume Session on buildbox"),
        "picker title should name the host:\n{out}"
    );

    // The local list is titled plainly — no "local" noise in the common case.
    d.app
        .open_resume_picker(HostId::local(), vec![cand("aaaa1111", "/home/test/proj")]);
    let out = d.render();
    assert!(out.contains("Resume Session"), "missing title:\n{out}");
    assert!(
        !out.contains("on local"),
        "local host should not be named:\n{out}"
    );
}

/// The picker's live settings belong to the picker, not to the dashboard's
/// footer ribbon: `Ctrl-t` has to visibly do something *inside* the popup the
/// user is looking at. The ribbon keeps the static labels only.
///
/// And they sit *above* the input, not below the list: title, settings and the
/// path being typed are the three things that decide what Enter does, so they
/// read as one block rather than being separated by a screen of list.
#[test]
fn the_workdir_picker_carries_its_agent_on_its_own_status_line() {
    let mut d = TestDashboard::new(120, 20);
    d.press(KeyCode::Char('O'));
    let out = d.render();
    let line_of = |needle: &str| {
        out.lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} line in the popup:\n{out}"))
    };
    let settings = line_of("Agent");
    assert!(
        out.lines().nth(settings).unwrap().contains("Claude"),
        "{out}"
    );
    assert!(
        settings < line_of("Type a path"),
        "settings belong above the input:\n{out}"
    );
    // The value moved off the bottom bar; only the key label is left there.
    let bar = out.lines().last().unwrap();
    assert!(bar.contains("Ctrl-t"), "{bar}");
    assert!(
        !bar.contains("Claude"),
        "the bar must not carry the value: {bar}"
    );

    // Flipping the backend rewrites that line, not the bar.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let out = d.render();
    assert!(
        out.lines()
            .any(|l| l.contains("Agent") && l.contains("Codex")),
        "status line should follow Ctrl-t:\n{out}"
    );
}

/// The settings line and the path input are two different kinds of thing — one
/// is toggled with chords, the other typed into — so a rule divides them. It
/// spans the popup's full width and meets the side borders, which is what keeps
/// it reading as part of the frame instead of a stray dash.
#[test]
fn a_rule_divides_the_pickers_settings_from_its_input() {
    let mut d = TestDashboard::new(120, 20);
    d.press(KeyCode::Char('O'));
    let out = d.render();
    let line_of = |needle: &str| {
        out.lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} line in the popup:\n{out}"))
    };
    let rule = line_of("\u{251c}");
    assert!(
        line_of("Agent") < rule && rule < line_of("Type a path"),
        "the rule belongs between the settings and the input:\n{out}"
    );
    let row = out.lines().nth(rule).unwrap();
    assert!(
        row.contains("\u{2524}") && row.contains("\u{2500}\u{2500}"),
        "the rule should run edge to edge with tees:\n{row}"
    );
}

/// A remote resume list is an ssh round trip, so the popup opens before it and
/// says so. Without this the whole UI froze until the host answered — worst on
/// the `Ctrl-h` host switch, which read as a hang.
#[test]
fn the_resume_picker_opens_before_its_list_arrives() {
    use crate::agent::{AgentControl, ResumeCandidate};
    use crate::state::HostId;
    use std::time::SystemTime;

    let mut d = TestDashboard::new(140, 20);
    d.app
        .open_resume_picker(HostId("buildbox".into()), Vec::new());
    d.app.set_picker_loading(true);
    let out = d.render();
    assert!(out.contains("Resume Session on buildbox"), "{out}");
    assert!(out.contains("Loading…"), "no pending message:\n{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("Host") && l.contains("buildbox")),
        "status line should name the host being loaded:\n{out}"
    );

    // The arriving list replaces both.
    d.app.reseed_resume_picker(
        HostId("buildbox".into()),
        vec![ResumeCandidate {
            agent: AgentControl::Claude,
            session_id: "bbbb2222".into(),
            cwd: "/srv/remote-proj".into(),
            first_prompt: Some("fix the thing".into()),
            custom_title: None,
            git_branch: None,
            mtime: SystemTime::UNIX_EPOCH,
        }],
    );
    d.app.set_picker_loading(false);
    let out = d.render();
    assert!(!out.contains("Loading…"), "{out}");
    assert!(out.contains("fix the thing"), "{out}");
}

#[test]
fn enter_on_running_remote_session_emits_attach() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/proj", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-claude-42-1".into());
    d.set_sessions(vec![s]);

    // We aren't attached to it yet → Enter attaches (spawns the ssh window).
    match d.press(KeyCode::Enter) {
        Some(Action::AttachRemoteRunning {
            host, pool_session, ..
        }) => {
            assert_eq!(host, HostId("box".into()));
            assert_eq!(pool_session, "cm-claude-42-1");
        }
        other => panic!("expected AttachRemoteRunning, got {other:?}"),
    }
}

#[test]
fn selected_window_id_resolves_bound_remote_window() {
    use crate::state::HostId;
    use crate::terminal::WindowId;
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/p", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-1".into());
    s.window_id = None; // a pooled launcher has no kitty window of its own
    d.set_sessions(vec![s]);

    // Not attached yet → no local window to preview/focus.
    assert_eq!(d.app.selected_window_id(), None);
    // After attaching (binding recorded), it resolves the attach window so
    // preview / move-to-tab / focus act on the right window.
    d.app
        .record_window_binding(HostId("box".into()), "cm-1".into(), WindowId::from(777u64));
    assert_eq!(d.app.selected_window_id(), Some(WindowId::from(777u64)));
}

#[test]
fn window_lookups_route_a_bound_remote_attach_window() {
    // Every `s.window_id` reader now goes through `window_id_for_session` (the
    // single choke point — next-step #6 §15.3), so an *attached* remote session
    // resolves to its local `ssh attach` window everywhere, not just for
    // `selected_window_id`. A pooled launcher has no kitty window of its own, so
    // before attaching these all see nothing.
    use crate::state::HostId;
    use crate::terminal::WindowId;
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/p", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-1".into());
    s.window_id = None;
    d.set_sessions(vec![s]);

    // Unattached: no local window resolves for tab resolution or focus.
    // (`focus_visible_by_index` now *attaches* an unattached remote — like
    // Enter — rather than no-op, so `selected_window_id` is the pure
    // window-resolution proxy here; the attach path is pinned separately.)
    assert!(d.app.selected_window_id().is_none());
    assert!(d.app.unresolved_local_tab_windows().is_empty());

    // Attached: the bound window flows through every resolver.
    let attach = WindowId::from(777u64);
    d.app
        .record_window_binding(HostId("box".into()), "cm-1".into(), attach.clone());
    match d.app.focus_visible_by_index(0) {
        Some(Action::FocusWindow(w)) => assert_eq!(w, attach),
        other => panic!("expected FocusWindow({attach:?}), got {other:?}"),
    }
    assert_eq!(d.app.unresolved_local_tab_windows(), vec![attach]);
}

#[test]
fn hand_launched_session_resolves_via_window_id_fallback() {
    use crate::terminal::WindowId;
    // A session launched directly (`miao launch claude`, not via the dashboard)
    // carries no launch_id; its launcher self-reported window_id. With no binding,
    // the resolver falls back to that field so preview/focus still work (§15.3).
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/home/test/a", SessionStatus::Idle);
    s.launch_id = None; // hand-launched — set_sessions seeds no binding
    s.window_id = Some(WindowId::from(555u64));
    d.set_sessions(vec![s]);
    assert!(d.app.window_bindings.is_empty());
    assert_eq!(d.app.selected_window_id(), Some(WindowId::from(555u64)));
}

#[test]
fn dashboard_spawned_session_resolves_only_via_binding() {
    use crate::state::HostId;
    use crate::terminal::WindowId;
    // A dashboard-spawned local session carries a launch_id; it resolves *only*
    // through the recorded binding (§15.2) and never reads a stale window_id.
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/home/test/a", SessionStatus::Idle);
    s.launch_id = Some("L-7".into());
    s.window_id = Some(WindowId::from(111u64)); // stale; must be ignored
    d.app.sessions = vec![s]; // bypass set_sessions: no binding recorded yet
    d.app.table_state.select(Some(0));
    // launch_id present + no binding → None, NOT the stale 111.
    assert_eq!(d.app.selected_window_id(), None);

    // Recording the binding (as the spawn path does) makes it resolve.
    d.app
        .record_window_binding(HostId::local(), "L-7".into(), WindowId::from(900u64));
    assert_eq!(d.app.selected_window_id(), Some(WindowId::from(900u64)));
}

/// The pid a teardown waits on before closing a window must be one *this*
/// machine's process table can answer for, and only an unpooled session's is:
/// such a session is its window, so `launcher_pid` is the process running in it.
///
/// The two `None` cases are the point. A pooled session's window runs an attach
/// client and its `launcher_pid` lives in the pool's namespace — which under
/// pooled-localhost is this very machine, so the number would resolve to a live
/// local process that has nothing to do with the row. A row whose host has left
/// the config resolves to no backend at all, and that must read as "no pid"
/// rather than fall through to the unpooled answer, for exactly the same reason.
#[test]
fn only_an_unpooled_session_offers_a_pid_to_wait_on() {
    let d = TestDashboard::new(100, 10);
    let s = session(4242, "/tmp/p", SessionStatus::Idle);

    // Local (unpooled): the window runs the launcher, so its pid is the answer.
    assert_eq!(d.app.window_process_pid(&s), Some(4242));

    // A host with no backend behind it yields nothing to wait on, so the caller
    // closes the window the way it always did.
    let orphan = LauncherState {
        host: crate::state::HostId("since-removed".into()),
        ..s.clone()
    };
    assert_eq!(d.app.window_process_pid(&orphan), None);
}

/// The host glyph shares the workdir-icon column rather than holding a Host
/// column of its own — `<host><workdir>`, and no `Host` header anywhere. Both
/// halves are fixed slots, so the column is the same width whether or not a row
/// has a host glyph: nothing to its right may move as hosts connect.
///
/// A row in **another terminal instance** claims no host slot of its own. It
/// used to, and that glyph is gone: two dashboards in two terminals at once is
/// rare, and paying for it in every row's icon column cost more than it said —
/// the more so because the glyph was the column's one non-emoji and drew as a
/// missing-glyph box on Ghostty. Dimming and the detail panel carry the state
/// now, so this pins that the slot is left blank exactly like a local row's.
#[test]
fn the_host_glyph_shares_the_workdir_icon_column() {
    let mut d = TestDashboard::new(140, 12);
    d.app.terminal_identity = Some("kitty:me".into());
    let mut foreign = session(1, "/home/test/elsewhere", SessionStatus::Idle);
    foreign.terminal = Some("zellij:other".into());
    let mine = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![foreign, mine]);

    let out = d.render();
    assert!(
        !out.contains("Host"),
        "the Host column should be gone:\n{out}"
    );
    // No divider between the halves. Everything left of the name is the row's
    // icon cell — the detail panel's own border lives well to the right of it.
    use unicode_width::UnicodeWidthStr;
    let icons = |name: &str| {
        let line = out
            .lines()
            .find(|l| l.contains(name) && l.contains("Idle"))
            .unwrap_or_else(|| panic!("no {name} row:\n{out}"));
        line.split(name).next().unwrap().to_string()
    };
    let foreign_icons = icons("session-1");
    let plain_icons = icons("session-2");
    // Both rows are on this machine, so both leave the host slot blank: the
    // only glyph in either icon cell is its own workdir mark. Counted rather
    // than compared verbatim, since the two rows differ by the cursor and by
    // which mark their cwd hashes to.
    for (label, cell) in [("foreign", &foreign_icons), ("local", &plain_icons)] {
        let glyphs = cell
            .split("Idle")
            .nth(1)
            .unwrap_or_else(|| panic!("no status on the {label} row:\n{out}"))
            .chars()
            .filter(|c| !c.is_whitespace())
            .count();
        assert_eq!(glyphs, 1, "the {label} row's icon cell:\n{out}");
    }
    for cell in [&foreign_icons, &plain_icons] {
        assert!(
            !cell.contains('\u{2502}'),
            "the halves are adjacent, no divider: {cell:?}"
        );
    }
    // The blank host slot is reserved, not closed up: both rows put their
    // workdir icon at the same column, which is the whole point of the fixed
    // slots. Measured in cells, since the glyphs either side differ in width.
    assert_eq!(
        foreign_icons.width(),
        plain_icons.width(),
        "a row with no host glyph must still reserve its slot:\n{out}"
    );
}

#[test]
fn foreign_terminal_row_is_window_inert() {
    use crate::state::HostId;
    use crate::terminal::WindowId;
    // A local row stamped with a terminal instance other than the dashboard's own
    // resolves to no window — its (overlapping) id belongs to a foreign namespace.
    let mut d = TestDashboard::new(120, 10);
    d.app.terminal_identity = Some("kitty:me".into());

    // Token-bearing (dashboard-spawned) foreign row: even a matching binding is
    // ignored.
    let mut spawned = session(1, "/home/test/a", SessionStatus::Idle);
    spawned.launch_id = Some("L-1".into());
    spawned.window_id = None;
    spawned.terminal = Some("zellij:other".into());
    d.app
        .record_window_binding(HostId::local(), "L-1".into(), WindowId::from(900u64));

    // Token-less (hand-launched) foreign row: its self-reported window_id is a
    // foreign id and must not be driven.
    let mut hand = session(2, "/home/test/b", SessionStatus::Idle);
    hand.launch_id = None;
    hand.window_id = Some(WindowId::from(555u64));
    hand.terminal = Some("zellij:other".into());

    // Same-terminal row: still resolves via its binding.
    let mut mine = session(3, "/home/test/c", SessionStatus::Idle);
    mine.launch_id = Some("L-3".into());
    mine.window_id = None;
    mine.terminal = Some("kitty:me".into());

    // Terminal-less row: keeps today's self-report behavior.
    let mut legacy = session(4, "/home/test/d", SessionStatus::Idle);
    legacy.launch_id = None;
    legacy.window_id = Some(WindowId::from(444u64));
    legacy.terminal = None;

    assert_eq!(
        d.app.foreign_terminal(&spawned).as_deref(),
        Some("zellij:other")
    );
    assert_eq!(d.app.window_id_for_session(&spawned), None);
    assert_eq!(d.app.window_id_for_session(&hand), None);

    d.app
        .record_window_binding(HostId::local(), "L-3".into(), WindowId::from(300u64));
    assert!(d.app.foreign_terminal(&mine).is_none());
    assert_eq!(
        d.app.window_id_for_session(&mine),
        Some(WindowId::from(300u64))
    );
    assert!(d.app.foreign_terminal(&legacy).is_none());
    assert_eq!(
        d.app.window_id_for_session(&legacy),
        Some(WindowId::from(444u64))
    );
}

#[test]
fn seed_preserves_foreign_binding_through_rewrite() {
    let _guard = bindings_file_guard();
    use crate::state::{HostId, WindowBinding};
    use crate::terminal::WindowId;
    // A persisted binding from another terminal instance is held inert (never in
    // the resolved map) yet carried through every rewrite verbatim, so switching
    // back to that terminal still finds the session's window.
    let live_pid = std::process::id();
    let mut d = TestDashboard::new(120, 10);
    let _ = std::fs::create_dir_all(crate::state::state_dir());
    d.app.terminal_identity = Some("kitty:me".into());

    let entries = vec![
        WindowBinding {
            window_id: WindowId::from(100u64),
            host: HostId::local().0,
            launcher_pid: live_pid,
            token: "L-mine".into(),
            terminal: Some("kitty:me".into()),
        },
        WindowBinding {
            window_id: WindowId::from(200u64),
            host: HostId::local().0,
            launcher_pid: live_pid,
            token: "L-foreign".into(),
            terminal: Some("zellij:other".into()),
        },
    ];
    d.app.seed_window_bindings(entries);
    // The foreign entry stays out of the resolved map; only the mine one is in it.
    assert_eq!(d.app.window_bindings.len(), 1);
    assert_eq!(d.app.foreign_bindings.len(), 1);

    // With no live rows, the rewrite still re-emits the foreign binding verbatim.
    d.app.sessions = Vec::new();
    d.app.write_window_bindings_file();
    let on_disk: Vec<WindowBinding> =
        crate::state::read_json(&crate::state::window_bindings_path()).unwrap();
    assert!(
        on_disk
            .iter()
            .any(|b| b.token == "L-foreign" && b.terminal.as_deref() == Some("zellij:other")),
        "foreign binding must survive the rewrite: {on_disk:?}"
    );
}

#[test]
fn prune_leaves_foreign_binding_intact() {
    let _guard = bindings_file_guard();
    use crate::state::{HostId, WindowBinding};
    use crate::terminal::WindowId;
    use std::collections::HashSet;
    // Pruning against a live snapshot only validates same-terminal bindings; a
    // foreign one is never looked at, so it survives a snapshot that lacks it.
    let mut d = TestDashboard::new(120, 10);
    let _ = std::fs::create_dir_all(crate::state::state_dir());
    d.app.terminal_identity = Some("kitty:me".into());

    let entries = vec![
        // A same-terminal remote attach binding (prune-eligible).
        WindowBinding {
            window_id: WindowId::from(700u64),
            host: "box".into(),
            launcher_pid: 2_000_000_000,
            token: "pool-1".into(),
            terminal: Some("kitty:me".into()),
        },
        // A foreign local binding carried for persistence.
        WindowBinding {
            window_id: WindowId::from(200u64),
            host: HostId::local().0,
            launcher_pid: std::process::id(),
            token: "L-foreign".into(),
            terminal: Some("zellij:other".into()),
        },
    ];
    d.app.seed_window_bindings(entries);
    assert_eq!(d.app.window_bindings.len(), 1);
    assert_eq!(d.app.foreign_bindings.len(), 1);

    // Empty live set: the remote mine binding detaches; the foreign one is inert.
    let dropped = d.app.prune_detached_sessions(&HashSet::new());
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].token, "pool-1");
    assert!(d.app.window_bindings.is_empty());
    assert_eq!(d.app.foreign_bindings.len(), 1);

    // The rewrite still carries the foreign binding.
    d.app.sessions = Vec::new();
    d.app.write_window_bindings_file();
    let on_disk: Vec<WindowBinding> =
        crate::state::read_json(&crate::state::window_bindings_path()).unwrap();
    assert_eq!(on_disk.len(), 1);
    assert_eq!(on_disk[0].token, "L-foreign");
}

#[test]
fn reap_skips_foreign_terminal_row() {
    use crate::state::HostId;
    use crate::terminal::WindowId;
    // A departed local row that lives in another terminal instance is never
    // reaped: closing its overlapping id through this backend would mis-target.
    let mut d = TestDashboard::new(120, 10);
    d.app.capabilities.floating_sessions = true;
    d.app.terminal_identity = Some("kitty:me".into());

    let mut s = session(1, "/home/test/a", SessionStatus::Idle);
    s.launch_id = Some("L-1".into());
    s.window_id = None;
    s.terminal = Some("zellij:other".into());
    d.app
        .record_window_binding(HostId::local(), "L-1".into(), WindowId::from(900u64));
    // `s` is absent from the (empty) live session set, so it counts as departed.
    assert!(d.app.sessions.is_empty());
    assert!(
        d.app
            .reap_departed_windows(std::slice::from_ref(&s))
            .is_empty()
    );
}

#[test]
fn enter_on_foreign_terminal_row_reports_it() {
    // Enter (focus) on a foreign row sets an explanatory status instead of a
    // generic no-op; `x` (kill) stays available since it signals by pid.
    let mut d = TestDashboard::new(120, 10);
    d.app.terminal_identity = Some("kitty:me".into());
    let mut s = session(1, "/home/test/a", SessionStatus::Idle);
    s.terminal = Some("zellij:other".into());
    d.set_sessions(vec![s]);

    assert!(d.press(KeyCode::Enter).is_none());
    assert!(d.app.status_is_error);
    assert!(
        d.app
            .status_msg
            .as_deref()
            .unwrap_or_default()
            .contains("zellij:other"),
        "status should name the foreign terminal: {:?}",
        d.app.status_msg
    );
}

#[test]
fn window_bindings_file_round_trips_through_seed() {
    let _guard = bindings_file_guard();
    use crate::state::HostId;
    use crate::terminal::WindowId;
    // The reload writes window-bindings.json; a restarted dashboard seeds from it
    // and re-resolves a live local session by its launch_id (§15.7 recovery), plus
    // a remote session by its pool_session. It is no longer the only test to
    // touch the shared window-bindings.json file, hence `bindings_file_guard`.
    // The state dir is only created below, after the first TestDashboard::new
    // redirects `state_dir()` into a per-process tempdir — creating it before
    // the redirect would make the wrong dir and the atomic write would silently
    // fail.
    let pid = std::process::id(); // a live pid so the seed keeps the local entry
    let mut s = session(pid, "/home/test/a", SessionStatus::Idle);
    s.launch_id = Some("L-rt".into());
    s.window_id = None;

    // A remote session whose launcher pid lives on another host — represented
    // here by a pid that is never alive locally, so the local-process liveness
    // gate would (wrongly) drop it. Its window must still resolve after a seed,
    // else a restarted dashboard re-attaches a second terminal to a pool session
    // that already has one ("already has a terminal attached").
    let remote_dead_pid = 2_000_000_000u32; // above any real pid_max on either OS
    assert!(!crate::state::is_process_alive(remote_dead_pid));
    let mut sr = session(remote_dead_pid, "/srv/proj", SessionStatus::Idle);
    sr.host = HostId("box".into());
    sr.pool_session = Some("cm-claude-999-2".into());
    sr.window_id = None;

    // First dashboard: record both spawn bindings and write the projection. The
    // redirect fires inside `new`, so create the (redirected) state dir after it.
    let mut d1 = TestDashboard::new(120, 10);
    let _ = std::fs::create_dir_all(crate::state::state_dir());
    d1.app.sessions = vec![s.clone(), sr.clone()];
    d1.app
        .record_window_binding(HostId::local(), "L-rt".into(), WindowId::from(424u64));
    d1.app.record_window_binding(
        HostId("box".into()),
        "cm-claude-999-2".into(),
        WindowId::from(777u64),
    );
    d1.app.write_window_bindings_file();

    // Second dashboard starts with empty bindings, seeds from disk, and resolves
    // each row through its own binding (order-independent — two rows would make
    // a selection-index assertion ambiguous).
    let mut d2 = TestDashboard::new(120, 10);
    d2.app.sessions = vec![s.clone(), sr.clone()];
    assert!(d2.app.window_bindings.is_empty());
    d2.app.seed_window_bindings_from_disk();
    // Local binding resolves (live pid).
    assert_eq!(
        d2.app.window_id_for_session(&s),
        Some(WindowId::from(424u64))
    );
    // The remote binding survives the seed despite its pid being dead locally.
    assert_eq!(
        d2.app.window_id_for_session(&sr),
        Some(WindowId::from(777u64))
    );
}

/// A remote row with no pool session is one this dashboard can neither attach
/// nor act on, so it never reaches the list at all (§9). The hosts panel's
/// session count keeps it countable; the host's own dashboard is its surface.
#[test]
fn remote_session_without_pool_name_is_hidden() {
    use crate::state::HostId;
    let d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/proj", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = None; // spawned into the server's own zellij, not the pool

    // A session on *this* machine is always actionable, whatever it carries.
    let local = session(2, "/home/test/here", SessionStatus::Idle);
    assert!(d.app.is_actionable_row(&local));
    assert!(!d.app.is_actionable_row(&s));
}

/// A pooled session with no window on this screen sinks below plain idle — it's
/// running somewhere else, so it shouldn't compete for the eye with what's in
/// front of you (§9).
#[test]
fn detached_rows_sink_below_plain_idle() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut detached = session(1, "/srv/away", SessionStatus::Idle);
    detached.host = HostId("box".into());
    detached.pool_session = Some("cm-away".into()); // pooled, but unbound
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![detached, here]);

    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![2, 1], "the detached row should sort last");
}

/// …and it *reads* as background too. The sort tier alone only helps once you
/// know the ordering rule; dimming the whole row says "running elsewhere"
/// where the eye already is. The override glyph is a couple of cells at the far
/// left and was carrying that on its own.
#[test]
fn a_detached_row_draws_dim() {
    use crate::state::HostId;
    use ratatui::style::Modifier;
    let mut d = TestDashboard::new(120, 12);
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = HostId("box".into());
    away.pool_session = Some("cm-away".into()); // pooled, but unbound
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![away, here]);
    d.render();

    let buf = d.terminal.backend().buffer();
    let dim_at = |needle: &str| {
        let (x, y) = find_cell(buf, needle).unwrap_or_else(|| panic!("{needle} not drawn"));
        buf[(x, y)].style().add_modifier.contains(Modifier::DIM)
    };
    assert!(dim_at("session-1"), "the detached row must draw dim");
    assert!(
        !dim_at("session-2"),
        "a row with a window here must keep full brightness"
    );
}

/// Two rows can both be "no window here" for opposite reasons: free to take, or
/// The two refusals the server upgrade owes the user, and the one case it lets
/// through.
///
/// Both come from the same fact: the upgrade ends every session on the host and
/// brings each one back as a window *here*. A session that isn't resting would
/// lose work to that, and one another terminal holds would be taken from
/// whoever is using it rather than handed back.
#[test]
fn an_upgrade_refuses_a_busy_host_and_one_held_by_another_client() {
    use crate::state::HostId;
    let host = HostId("box".into());
    let pooled = |pid: u32, status: SessionStatus, attached: Option<bool>| {
        let mut s = session(pid, "/srv/work", status);
        s.host = HostId("box".into());
        s.pool_session = Some(format!("cm-{pid}"));
        s.attached = attached;
        // A pooled row's window binding is keyed on the pool name, not the
        // launch id a local row carries; leaving that behind is what makes
        // `detached_kind` see these as detached at all.
        s.launch_id = None;
        s.window_id = None;
        s
    };

    // Everything idle and nobody else attached: the upgrade may proceed.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![
        pooled(1, SessionStatus::Idle, Some(false)),
        pooled(2, SessionStatus::Compacted, None),
    ]);
    assert_eq!(d.app.upgrade_blocker(&host), None);
    // …and both are on the restore list, detached or not — every session comes
    // back as a window, which is exactly why a held one is refused below.
    assert_eq!(d.app.upgrade_restore_list(&host).len(), 2);

    // A session that isn't resting blocks it. Note `WaitingForApproval` is at
    // rest by `SessionStatus::is_busy`'s narrower test and must still block:
    // restarting a session sitting on a permission prompt loses the prompt.
    for status in [
        SessionStatus::Active,
        SessionStatus::Starting,
        SessionStatus::WaitingForApproval,
    ] {
        let mut d = TestDashboard::new(120, 12);
        d.set_sessions(vec![
            pooled(1, SessionStatus::Idle, Some(false)),
            pooled(2, status.clone(), Some(false)),
        ]);
        let why = d
            .app
            .upgrade_blocker(&host)
            .unwrap_or_else(|| panic!("{status:?} must block"));
        assert!(why.contains("not idle"), "{status:?} said: {why}");
    }

    // A parked server and a session blocked on a human review do *not*: the
    // agent's turn ended in both, so there is no work in flight to lose, and
    // refusing left a host un-upgradeable over a shell the user had already
    // walked away from. Same whitelist the restart commands use
    // (`SessionStatus::is_restartable`).
    for status in [
        SessionStatus::BackgroundServer,
        SessionStatus::ReviewPending,
    ] {
        let mut d = TestDashboard::new(120, 12);
        d.set_sessions(vec![
            pooled(1, SessionStatus::Idle, Some(false)),
            pooled(2, status.clone(), Some(false)),
        ]);
        assert_eq!(
            d.app.upgrade_blocker(&host),
            None,
            "{status:?} must not block an upgrade"
        );
        // …and both still come back afterwards.
        assert_eq!(d.app.upgrade_restore_list(&host).len(), 2);
    }

    // A session another terminal is attached to blocks it too, even idle.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![
        pooled(1, SessionStatus::Idle, Some(false)),
        pooled(2, SessionStatus::Idle, Some(true)),
    ]);
    let why = d.app.upgrade_blocker(&host).expect("held row blocks");
    assert!(why.contains("another terminal"), "{why}");

    // An unreadable attached bit is not evidence of a second client — the same
    // rule the detached glyph follows.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![pooled(1, SessionStatus::Idle, None)]);
    assert_eq!(d.app.upgrade_blocker(&host), None);

    // A host with nothing on it upgrades freely, and owes nothing back.
    let d = TestDashboard::new(120, 12);
    assert_eq!(d.app.upgrade_blocker(&host), None);
    assert!(d.app.upgrade_restore_list(&host).is_empty());
}

/// held by somebody else's terminal. `Enter` behaves differently on each (the
/// second needs a steal), so they must not wear the same glyph. The host's
/// attached-bit overlay is what separates them — and an *unknown* bit (an
/// unreadable pool) must read as free, never as held.
#[test]
fn a_row_held_by_another_client_reads_apart_from_a_free_one() {
    use super::format::Detached;
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut free = session(1, "/srv/free", SessionStatus::Idle);
    free.host = HostId("box".into());
    free.pool_session = Some("cm-free".into());
    free.attached = Some(false);
    let mut held = session(2, "/srv/held", SessionStatus::Idle);
    held.host = HostId("box".into());
    held.pool_session = Some("cm-held".into());
    held.attached = Some(true);
    let mut unknown = session(3, "/srv/unknown", SessionStatus::Idle);
    unknown.host = HostId("box".into());
    unknown.pool_session = Some("cm-unknown".into());
    unknown.attached = None; // the pool couldn't be read
    let here = session(4, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![free, held, unknown, here]);

    let kind = |d: &TestDashboard, pid: u32| {
        let s = d
            .app
            .sessions
            .iter()
            .find(|s| s.launcher_pid == pid)
            .expect("row exists")
            .clone();
        d.app.detached_kind(&s)
    };
    assert_eq!(kind(&d, 1), Some(Detached::Free));
    assert_eq!(kind(&d, 2), Some(Detached::HeldElsewhere));
    assert_eq!(kind(&d, 3), Some(Detached::Free), "unknown is not held");
    assert_eq!(kind(&d, 4), None, "a row with a window here isn't detached");

    // Both glyphs actually reach the override indicator, and they differ.
    let out = d.render();
    assert!(out.contains('\u{1F648}'), "the free row keeps 🙈");
    assert!(out.contains('\u{1F440}'), "the held row draws 👀");

    // The preview panel says which one you're looking at, and names the steal by
    // its live binding rather than a hardcoded key.
    d.app.table_state.select(Some(
        d.app
            .visible_sessions()
            .iter()
            .position(|s| s.launcher_pid == 2)
            .expect("held row is visible"),
    ));
    let placeholder = d.app.preview_placeholder();
    assert!(
        placeholder.contains("another terminal") && placeholder.contains("Space s"),
        "unexpected placeholder: {placeholder}"
    );
}

/// …and no status lifts it back out — not even a live blocking prompt. A
/// parked approval or decision is urgent, but it's urgent *elsewhere*: it
/// can't be answered until the row is attached, so seating it above the
/// sessions on this screen buries the work that can be done now.
#[test]
fn an_attention_state_does_not_lift_a_detached_row() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut waiting = session(1, "/srv/away", SessionStatus::WaitingForDecision);
    waiting.host = HostId("box".into());
    waiting.pool_session = Some("cm-away".into());
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    let working = session(3, "/home/test/work", SessionStatus::Active);
    d.set_sessions(vec![waiting, here, working]);

    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(
        order,
        vec![2, 3, 1],
        "a detached decision prompt still sorts below every live row"
    );
}

/// …and `s` doesn't take you there either. The jump key means "take me to the
/// work waiting on me", which a detached row can't offer: its prompt is
/// unanswerable until the row is attached, so landing the cursor there costs a
/// keypress and gives nothing back. When the detached row is the *only* thing
/// wanting attention the key says so, rather than claiming nothing does while
/// an attention icon sits on screen.
#[test]
fn the_attention_jump_skips_a_detached_row() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut away = session(1, "/srv/away", SessionStatus::WaitingForDecision);
    away.host = HostId("box".into());
    away.pool_session = Some("cm-away".into()); // pooled, but unbound
    let here = session(2, "/home/test/here", SessionStatus::WaitingForDecision);
    let working = session(3, "/home/test/work", SessionStatus::Active);
    d.set_sessions(vec![away, here, working]);
    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![2, 3, 1]);

    // Sitting on the one live attention row, `s` has nowhere else to go — the
    // detached decision prompt at index 2 is not a candidate.
    d.app.table_state.select(Some(0));
    d.press(KeyCode::Char('s'));
    assert_eq!(d.selected(), Some(0));
    assert_eq!(
        d.app.status_msg.as_deref(),
        Some("Only one session needs attention")
    );

    // With the live row at rest, only the detached one still wants something.
    // The cursor stays put and the status names why.
    d.app.sessions[1].status = SessionStatus::Idle;
    d.app.mark_dirty(Cursor::HoldIndex);
    d.press(KeyCode::Char('s'));
    assert_eq!(d.selected(), Some(0));
    assert_eq!(
        d.app.status_msg.as_deref(),
        Some("Only detached sessions need attention")
    );
}

/// "No sessions need attention" is a claim about every visible row, and one
/// backend cannot back it: Pi has no per-tool approval prompt at all
/// (`AgentCapabilities::approval_gate`), so a Pi session blocked on the user
/// looks exactly like one that is working. The sweep therefore names the backend
/// instead of reading as exhaustive — the treatment `RefreshPreview` already
/// gives a terminal that cannot read a window.
#[test]
fn the_attention_sweep_says_when_a_backend_could_not_have_told_it() {
    let mut d = TestDashboard::new(120, 12);
    let claude = session(1, "/home/test/one", SessionStatus::Idle);
    d.set_sessions(vec![claude.clone()]);
    d.app.table_state.select(Some(0));
    d.press(KeyCode::Char('s'));
    assert_eq!(
        d.app.status_msg.as_deref(),
        Some("No sessions need attention"),
        "every visible backend can report an approval, so the sweep is exhaustive"
    );

    // Add a Pi row and the same keypress stops promising as much.
    let mut pi = session(2, "/home/test/two", SessionStatus::Active);
    pi.agent = crate::agent::AgentControl::Pi;
    d.set_sessions(vec![claude, pi]);
    d.app.table_state.select(Some(0));
    d.press(KeyCode::Char('s'));
    let msg = d.app.status_msg.clone().expect("a status message");
    assert!(
        msg.starts_with("No sessions need attention — Pi has no approval prompt"),
        "the sweep must name the backend that could not answer: {msg}"
    );
}

/// The one thing that does outrank detachment is an explicit pin: `p` is the
/// user saying "keep this in front of me" about that exact row, which is the
/// whole job of the flag — unlike a status, which the dashboard infers.
#[test]
fn a_pin_lifts_a_detached_row() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = HostId("box".into());
    away.pool_session = Some("cm-away".into()); // pooled, but unbound
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![away, here]);
    d.app
        .update_flags((HostId("box".into()), 1), Cursor::HoldIndex, |f| {
            f.pinned = true
        });

    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![1, 2], "the pinned detached row leads");
}

/// A *follow-up bell* is the same call for a softer signal. The bell is
/// auto-armed on every Active→Idle, so a detached session that merely finished
/// a turn would otherwise float into the attention block and stay there — the
/// exact opposite of what the detached tier is for.
#[test]
fn a_follow_up_bell_does_not_lift_a_detached_row() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = HostId("box".into());
    away.pool_session = Some("cm-away".into()); // pooled, but unbound
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![away, here]);
    d.app
        .update_flags((HostId("box".into()), 1), Cursor::HoldIndex, |f| {
            f.follow_up = true
        });

    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(
        order,
        vec![2, 1],
        "the flagged detached row still sorts last"
    );
}

/// Detachment is a sort key, so retiring a binding has to invalidate the cached
/// visible order. It used to not: the row picked up the unplugged icon (computed
/// live at draw time) but kept its old slot until some unrelated reload happened
/// to bump the version — and nothing reloads when an attach window closes, which
/// is exactly when this fires.
#[test]
fn pruning_a_binding_resorts_the_list() {
    use crate::state::HostId;
    use crate::terminal::WindowId;
    use std::collections::HashSet;
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    let mut here = session(2, "/home/test/here", SessionStatus::Idle);
    here.updated_at -= 60; // older, so recency alone would sort it second
    d.set_sessions(vec![away, here]);
    // Attached: the row is an ordinary idle row, and the newer one leads.
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));
    let order = |d: &TestDashboard| -> Vec<u32> {
        d.app
            .visible_sessions()
            .iter()
            .map(|s| s.launcher_pid)
            .collect()
    };
    assert_eq!(order(&d), vec![1, 2]);

    // Its window died: the prune must move it, not just re-icon it. (The local
    // row's own `launch_id` binding goes too — `set_sessions` seeds one — but a
    // row with no pool session can't be detached, so it doesn't move.)
    let dropped = d.app.prune_detached_sessions(&HashSet::new());
    assert!(dropped.iter().any(|k| k.token == "cm-away"));
    assert_eq!(order(&d), vec![2, 1]);
}

/// Closing a window ends its session (`[remote] on_window_close`, default
/// `close`) — but only when it was the **user** who closed it. Every other way
/// an attach can end must leave the session running, and the dangerous
/// direction is the one this pins: a laptop waking to a dead link reports one
/// per window, and closing sessions there would turn a flaky network into lost
/// work.
#[test]
fn only_a_user_closed_window_ends_its_session() {
    use super::{ReportOrigin, closed_by_the_user};

    // 129 = 128 + SIGHUP: the terminal tore the pty down under a live attach,
    // which is what closing a window (or its tab) does.
    assert!(closed_by_the_user(Some(129)));
    // ssh's 255 — a dropped link *and* a failure to connect. The session is
    // what survived; ending it is the opposite of what's wanted.
    assert!(!closed_by_the_user(Some(255)));
    // An in-session shpool detach returned cleanly: already "leave it running".
    // A session *stolen* from us lands here too — libshpool's steal sends the
    // sitting client a `Detach` and its attach simply exits (`guard_plain_
    // reattach`), so the window we lose to someone else must not take the
    // session they just took over with it.
    assert!(!closed_by_the_user(Some(0)));
    // A reporter that couldn't tell must never be read as intent.
    assert!(!closed_by_the_user(None));
    // Spent, so their windows close — but they reach the wrapper by routes that
    // aren't a window closing, and the default here ends a session.
    assert!(!closed_by_the_user(Some(130)));
    assert!(!closed_by_the_user(Some(143)));

    // And the origin gate: a quitting terminal SIGHUPs every attach window on
    // its way out, taking the dashboard with it, so those 129s are waiting at
    // the next startup. Acting on them would end every session on the host.
    assert_ne!(ReportOrigin::Backlog, ReportOrigin::Live);
}

/// `D` closes its window exactly the way the user's own close does, so it
/// produces an identical 129 — which under `on_window_close = "close"` would end
/// the very session `D` exists to keep running. Nothing in the report tells them
/// apart. What separates them is **ordering**: `D` retires the binding before
/// closing the window, so the report finds nothing to retire and stops at the
/// `continue` above the policy.
///
/// That ordering was merely tidy when the report only retired a binding; it is
/// load-bearing now, and this is what pins it. The same shape protects `x` and
/// restart, which don't retire first but have already ended the session, so
/// their late 129 finds no row and resolves no key.
#[test]
fn detaching_before_the_close_is_what_keeps_d_from_killing() {
    use super::ReportOrigin;
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    d.set_sessions(vec![away]);
    d.app.on_window_close = crate::config::OnWindowClose::Close;
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));

    // `D`: retire, *then* close the window (`Action::DetachRemote`).
    d.app.retire_window_binding(&host, "cm-away");
    // The close the user never sees, arriving as the same 129 a hand-close makes.
    assert!(
        !d.app.apply_detach_reports(
            vec![DetachReport {
                host: "box".into(),
                token: "cm-away".into(),
                status: Some(129),
                held_secs: Some(600),
            }],
            ReportOrigin::Live
        ),
        "a report for an already-retired binding must be a no-op"
    );
    assert!(
        d.app.pending_session_close.is_empty(),
        "`D` must leave the session running — that is the whole point of it"
    );
}

/// The whole-batch version of the above, through `apply_detach_reports`: a
/// user-closed window queues its session for closing, a dropped link does not,
/// and a backlogged report never does whatever its status says.
#[test]
fn a_reported_window_close_queues_only_its_own_session() {
    use super::ReportOrigin;
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let pooled = |pid: u32, token: &str| {
        let mut s = session(pid, "/srv/work", SessionStatus::Idle);
        s.host = host.clone();
        s.pool_session = Some(token.to_string());
        s.window_id = None;
        s
    };
    d.set_sessions(vec![
        pooled(1, "cm-one"),
        pooled(2, "cm-two"),
        pooled(3, "cm-three"),
    ]);
    for (token, wid) in [("cm-one", 901u64), ("cm-two", 902), ("cm-three", 903)] {
        d.app
            .record_window_binding(host.clone(), token.into(), WindowId::from(wid));
    }

    // Pinned rather than inherited: the default is `Close`, but a developer
    // whose own config says otherwise must not see this test fail.
    d.app.on_window_close = crate::config::OnWindowClose::Close;

    let report = |token: &str, status: i32| DetachReport {
        host: "box".into(),
        token: token.into(),
        status: Some(status),
        held_secs: Some(600),
    };
    // A live 129 — the user closed this one.
    assert!(
        d.app
            .apply_detach_reports(vec![report("cm-one", 129)], ReportOrigin::Live)
    );
    // A live 255 — the link died under it.
    assert!(
        d.app
            .apply_detach_reports(vec![report("cm-two", 255)], ReportOrigin::Live)
    );
    // A backlogged 129 — indistinguishable from a terminal quit, so no.
    assert!(
        d.app
            .apply_detach_reports(vec![report("cm-three", 129)], ReportOrigin::Backlog)
    );

    let queued: Vec<String> = d
        .app
        .pending_session_close
        .iter()
        .map(|p| p.key.to_string())
        .collect();
    assert_eq!(
        queued,
        vec![crate::state::SessionKey::from_launcher_pid(1).to_string()],
        "only the user-closed window's session is queued"
    );
    // All three bindings are retired either way — the policy decides the
    // session's fate, never whether the window is still bound.
    assert!(d.app.window_bindings.window_for(&host, "cm-two").is_none());
    assert!(
        d.app
            .window_bindings
            .window_for(&host, "cm-three")
            .is_none()
    );

    // `detach` opts out entirely: the same user-closed window queues nothing.
    d.app.pending_session_close.clear();
    d.app.on_window_close = crate::config::OnWindowClose::Detach;
    d.app
        .record_window_binding(host.clone(), "cm-two".into(), WindowId::from(902u64));
    assert!(
        d.app
            .apply_detach_reports(vec![report("cm-two", 129)], ReportOrigin::Live)
    );
    assert!(d.app.pending_session_close.is_empty());
}

/// The two refusals captain-miao mints itself say what happened, in the
/// dashboard, rather than pointing at a window the user is not looking at.
///
/// They earn that because an attach is the only operation that actually takes
/// the pty's lock: its refusal is a transaction's answer, authoritative for the
/// instant it happened, where any query about the same session is a sample. A
/// refusal we did *not* mint keeps the old text — its reason exists only as
/// output held in that window.
#[test]
fn a_refused_attach_names_its_reason() {
    use super::ReportOrigin;
    use crate::state::{ATTACH_EXIT_BUSY, ATTACH_EXIT_STALE, DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let pooled = |pid: u32, token: &str| {
        let mut s = session(pid, "/srv/work", SessionStatus::Idle);
        s.host = host.clone();
        s.pool_session = Some(token.to_string());
        s.window_id = None;
        s
    };
    d.set_sessions(vec![pooled(1, "cm-one"), pooled(2, "cm-two")]);

    // A refusal arrives the instant the wrapper exits, so the window it was
    // holding is not "spent" — that is the branch these statuses land in.
    let refused = |token: &str, status: i32| DetachReport {
        host: "box".into(),
        token: token.into(),
        status: Some(status),
        held_secs: Some(0),
    };
    let apply = |d: &mut TestDashboard, report: DetachReport, wid: u64| {
        let token = report.token.clone();
        d.app
            .record_window_binding(host.clone(), token, WindowId::from(wid));
        d.app.apply_detach_reports(vec![report], ReportOrigin::Live);
        d.app.status_msg.clone().expect("a status was set")
    };

    let msg = apply(&mut d, refused("cm-one", ATTACH_EXIT_BUSY), 901);
    assert!(
        msg.contains("cm-one") && msg.contains("attached in another terminal"),
        "busy should name the session and the reason, got {msg:?}"
    );
    assert!(
        msg.contains(
            &d.app
                .keymap
                .primary_key(super::keymap::Command::StealAttach)
                .expect("a steal binding")
        ),
        "and offer the steal by its live binding, got {msg:?}"
    );

    let msg = apply(&mut d, refused("cm-two", ATTACH_EXIT_STALE), 902);
    assert!(
        msg.contains("cm-two") && msg.contains("no longer a live session"),
        "stale should say the session is gone, got {msg:?}"
    );

    // Anything else — ssh auth, a missing server — reads the same as before.
    let msg = apply(&mut d, refused("cm-one", 255), 903);
    assert!(
        msg.contains("see its window"),
        "an unminted status keeps pointing at the window, got {msg:?}"
    );
}

/// A queued close waits before it goes out, and that wait is the guard against
/// the one event that looks exactly like a user closing every window at once: a
/// terminal quitting. The dashboard dies with it in milliseconds, so anything
/// still waiting is never sent — which only works if the queue really is
/// time-gated rather than drained on sight.
#[test]
fn a_queued_close_waits_out_its_delay_before_going_anywhere() {
    use super::{CLOSE_ON_WINDOW_CLOSE_DELAY, ReportOrigin};
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    use std::time::Instant;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    d.set_sessions(vec![away]);
    d.app.on_window_close = crate::config::OnWindowClose::Close;
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));

    let queued_at = Instant::now();
    assert!(d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            status: Some(129),
            held_secs: Some(600),
        }],
        ReportOrigin::Live
    ));
    let queued_after = Instant::now();

    // Nothing goes out on the tick that queued it — a dashboard dying right here
    // (its terminal quitting) takes the kill with it.
    assert!(
        d.app.take_due_session_closes(queued_at).is_empty(),
        "a close must not fire on the tick that queued it"
    );
    assert!(
        d.app
            .take_due_session_closes(queued_at + CLOSE_ON_WINDOW_CLOSE_DELAY / 2)
            .is_empty(),
        "nor part way through the delay"
    );
    // Still queued, and the loop knows when to come back for it.
    assert!(d.app.next_session_close_due().is_some());

    // Measured from *after* the call, since the queue stamps its own `now`
    // inside it — `queued_at + DELAY` would miss the deadline by that sliver.
    let due = d
        .app
        .take_due_session_closes(queued_after + CLOSE_ON_WINDOW_CLOSE_DELAY);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].0, host);
    assert_eq!(due[0].1, crate::state::SessionKey::from_launcher_pid(1));
    // Taken once: the loop drains every tick, and a second pass must not
    // re-signal a session already asked to end.
    assert!(
        d.app
            .take_due_session_closes(queued_after + CLOSE_ON_WINDOW_CLOSE_DELAY * 10)
            .is_empty()
    );
    assert!(d.app.next_session_close_due().is_none());
}

/// …and none of that wait is on screen. The row goes when the close is *queued*,
/// a second before the kill it is waiting for.
///
/// Left visible, that second was the whole of what closing a remote window
/// looked like: the binding is retired, so the row drops to the detached tier
/// still carrying the pool's `attached = true` — the bit *our own* attach set —
/// and reads as "another terminal has this", offering a steal for a session
/// nobody holds and this dashboard has already decided to end. Hiding it here
/// costs nothing the queue doesn't already cost: a dashboard that dies inside
/// the delay drops the close and the presumption together.
#[test]
fn a_queued_close_takes_its_row_with_it_immediately() {
    use super::ReportOrigin;
    use crate::backend::{Backend, RemoteBackend};
    use crate::state::{DetachReport, HostId, SessionKey};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    // The host mirrors the session as attached — which it is, to the window
    // about to be closed. Nothing will arrive to correct that: this backend has
    // no connection, so what the row does next is the dashboard's own doing.
    away.attached = Some(true);
    d.set_sessions(vec![away.clone()]);
    d.app
        .backends
        .push(Backend::Remote(RemoteBackend::unconnected_for_tests(
            host.clone(),
            vec![away],
        )));
    d.app.on_window_close = crate::config::OnWindowClose::Close;
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));

    let rows = |d: &TestDashboard| {
        d.app
            .backend_for(&host)
            .expect("the host's backend")
            .list_sessions()
            .len()
    };
    assert_eq!(rows(&d), 1);

    assert!(d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            status: Some(129),
            held_secs: Some(600),
        }],
        ReportOrigin::Live
    ));

    assert_eq!(
        rows(&d),
        0,
        "the row must go with the window, not with the kill a second later"
    );
    // And the kill is still owed: hiding the row is a presumption about a
    // request that has yet to go out, never a substitute for making it.
    assert_eq!(
        d.app
            .pending_session_close
            .iter()
            .map(|p| p.key.clone())
            .collect::<Vec<_>>(),
        vec![SessionKey::from_launcher_pid(1)]
    );
}

/// Where the session *stays* — `D`, or a closed window under
/// `on_window_close = "detach"` — the row has to lose the attached bit with its
/// window, because the bit is one our own attach set and the host won't know
/// otherwise until its pool hook has crossed the link.
///
/// Left alone for that round trip, the row is detached *and* attached, which is
/// the dashboard's spelling of "another terminal has this": 👀 instead of 🙈,
/// and `Enter` offering to steal a session the user has just put down.
#[test]
fn a_detach_lowers_the_bit_its_own_attach_raised() {
    use super::ReportOrigin;
    use super::format::Detached;
    use crate::backend::{Backend, RemoteBackend};
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let pooled = |pid: u32, token: &str| {
        let mut s = session(pid, "/srv/work", SessionStatus::Idle);
        s.host = host.clone();
        s.pool_session = Some(token.to_string());
        s.window_id = None;
        // Attached — to the window this dashboard is about to give up.
        s.attached = Some(true);
        s
    };
    let (detached_by_key, closed_by_hand) = (pooled(1, "cm-one"), pooled(2, "cm-two"));
    d.set_sessions(vec![detached_by_key.clone(), closed_by_hand.clone()]);
    d.app
        .backends
        .push(Backend::Remote(RemoteBackend::unconnected_for_tests(
            host.clone(),
            vec![detached_by_key, closed_by_hand],
        )));
    for (token, wid) in [("cm-one", 901u64), ("cm-two", 902)] {
        d.app
            .record_window_binding(host.clone(), token.into(), WindowId::from(wid));
    }

    // `D` on the first row.
    d.app.retire_window_binding(&host, "cm-one");
    // A hand-closed window on the second, with the policy that keeps the
    // session: no kill, so the row stays and its bit is all it has.
    d.app.on_window_close = crate::config::OnWindowClose::Detach;
    assert!(d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-two".into(),
            status: Some(129),
            held_secs: Some(600),
        }],
        ReportOrigin::Live
    ));

    // What the next reload will carry — both rows still there, neither attached.
    let mut rows = d
        .app
        .backend_for(&host)
        .expect("the host's backend")
        .list_sessions();
    rows.sort_by_key(|s| s.launcher_pid);
    assert_eq!(rows.len(), 2, "a detach ends no session");
    assert_eq!(
        rows.iter().map(|s| s.attached).collect::<Vec<_>>(),
        vec![Some(false), Some(false)]
    );
    // …and that is what turns 👀 back into 🙈: free to take, no steal implied.
    d.set_sessions(rows);
    for s in d.app.sessions.clone() {
        assert_eq!(d.app.detached_kind(&s), Some(Detached::Free));
    }
}

/// A detach report is the *event* that replaces polling the window tree: the
/// attach window tells us its session ended, and the row must go detached (and
/// re-sort) without waiting for a snapshot. Crucially it behaves like
/// `prune_dead`, not like `D` — the expected-attached memory survives, because a
/// closed window and a dropped ssh are indistinguishable from here and both
/// should come back when the host reconnects.
#[test]
fn a_detach_report_retires_the_binding_but_not_the_expectation() {
    use super::ReportOrigin;
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![away, here]);
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));
    assert!(!d.app.is_detached_row(&d.app.sessions[0].clone()));

    let changed = d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            status: Some(0),
            held_secs: Some(600),
        }],
        ReportOrigin::Live,
    );
    assert!(changed);
    assert!(d.app.is_detached_row(&d.app.sessions[0].clone()));
    // The row sank on the strength of the report alone — no snapshot involved.
    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![2, 1]);
    // …and auto-reattach still knows to bring it back on a reconnect.
    assert_eq!(
        d.app.window_bindings.expected_without_window(&host),
        vec!["cm-away".to_string()]
    );
    // The window it was bound to is queued for closing — the backstop for the
    // window the wrapper didn't close itself, which the next `Enter` would
    // otherwise spawn a sibling for.
    assert_eq!(d.app.reap_window_queue, vec![WindowId::from(900u64)]);

    // A report for a binding we no longer hold is a no-op, not a wobble: the
    // snapshot prune or a `D` may well have got there first.
    assert!(!d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            status: Some(0),
            held_secs: Some(600),
        }],
        ReportOrigin::Live
    ));
}

/// Attaching and detaching flip `is_detached_row`, which is a *sort key* — so a
/// binding change slides the selected row to the other end of the list while the
/// cursor, being a bare index, stays put and silently re-points at whichever
/// session took its slot. The cursor must follow the session instead.
#[test]
fn the_cursor_follows_a_session_across_an_attach_detach_resort() {
    use super::ReportOrigin;
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![away, here]);
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));

    let index_of = |d: &TestDashboard, pid: u32| {
        d.app
            .visible_sessions()
            .iter()
            .position(|s| s.launcher_pid == pid)
            .expect("row is visible")
    };
    // Park the cursor on the attached pooled row.
    let attached_at = index_of(&d, 1);
    d.app.table_state.select(Some(attached_at));

    // Its attach window reports its end: the row sinks into the detached tier
    // and the other row rises into the index it vacated.
    assert!(d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            status: Some(0),
            held_secs: Some(600),
        }],
        ReportOrigin::Live
    ));
    let detached_at = index_of(&d, 1);
    assert_ne!(
        detached_at, attached_at,
        "the detach must actually re-sort the row, or this test proves nothing"
    );
    assert_eq!(
        d.app.selected_pid(),
        Some(1),
        "the cursor follows the session that detached, not the index it left"
    );
    assert_eq!(d.app.table_state.selected(), Some(detached_at));

    // Re-attaching floats it back out of the tier; the cursor rides along again.
    d.app
        .record_window_binding(host, "cm-away".into(), WindowId::from(901u64));
    assert_eq!(d.app.selected_pid(), Some(1));
    assert_eq!(d.app.table_state.selected(), Some(index_of(&d, 1)));

    // And an explicit `D` (the same retire, minus the report) keeps it too.
    d.app
        .retire_window_binding(&HostId("box".into()), "cm-away");
    assert_eq!(
        d.app.selected_pid(),
        Some(1),
        "`D` sinks the row it acts on — the cursor stays on that session"
    );
}

/// A window left behind by a dropped ssh shows a dead session's last frame while
/// the row says detached — and `Enter` then opens a *second* window beside it.
/// So a spent attach's window is closed. But a *refused* attach's window is the
/// only place its error exists, so that one stays: the two are told apart by
/// exit status plus how long it ran.
#[test]
fn a_spent_attach_window_is_closed_but_a_refused_one_is_kept() {
    use super::attach_window_is_spent;
    use std::time::Duration;
    let grace = super::ATTACH_STARTUP_GRACE;

    // Ran and ended: a clean detach, and a link that died mid-session (ssh's
    // 255, indistinguishable by status alone from a failure to connect — which
    // is exactly why the duration is consulted).
    assert!(attach_window_is_spent(Duration::from_secs(1), Some(0)));
    assert!(attach_window_is_spent(grace, Some(255)));
    // The window was torn down under the wrapper (128 + SIGHUP). Never a
    // refusal, whatever the duration — otherwise closing a window seconds after
    // opening it would be announced as a failed attach, pointing at a window
    // that is already gone.
    assert!(attach_window_is_spent(Duration::from_secs(1), Some(129)));
    // A reporter that couldn't determine a status reads as clean.
    assert!(attach_window_is_spent(Duration::from_secs(1), None));

    // Refused on arrival — busy, stale name, ssh auth. The window holds the only
    // copy of that message.
    assert!(!attach_window_is_spent(Duration::from_millis(200), Some(1)));
    assert!(!attach_window_is_spent(Duration::from_secs(2), Some(255)));
}

/// The duration that decides the above comes from the *wrapper*, which measured
/// it in wall clock, not from the binding, whose age is an `Instant` — and
/// CLOCK_MONOTONIC does not advance while the machine is suspended. A laptop
/// that slept through an attach and woke to a dead ssh is the case: hours of
/// attach, seconds of monotonic age, and reading the binding would file the
/// overnight drop as a refused attach and strand its window on screen.
#[test]
fn a_report_is_judged_by_the_wrappers_clock_not_the_bindings() {
    use super::ReportOrigin;
    use crate::state::{DetachReport, HostId};
    use crate::terminal::WindowId;
    let _guard = bindings_file_guard();
    let mut d = TestDashboard::new(120, 12);
    let host = HostId("box".into());
    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = host.clone();
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    d.set_sessions(vec![away]);
    // Bound just now, so the binding's own age is milliseconds — the shape a
    // suspend leaves behind, and inside `ATTACH_STARTUP_GRACE`.
    d.app
        .record_window_binding(host.clone(), "cm-away".into(), WindowId::from(900u64));

    assert!(d.app.apply_detach_reports(
        vec![DetachReport {
            host: "box".into(),
            token: "cm-away".into(),
            // ssh's mid-session drop — the status that can't decide alone.
            status: Some(255),
            held_secs: Some(8 * 60 * 60),
        }],
        ReportOrigin::Live
    ));
    assert_eq!(
        d.app.reap_window_queue,
        vec![WindowId::from(900u64)],
        "an eight-hour attach that dropped is spent, whatever the binding's \
         monotonic age says"
    );
    assert!(
        !d.app.status_is_error,
        "and it is not announced as a failed attach"
    );
}

/// The preview panel is a capture of the row's *local* window, so a detached row
/// has nothing to show — ever, not "yet". It used to claim "(no session
/// selected)" in the one case it fires most often.
#[test]
fn preview_placeholder_names_the_detached_case() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    assert_eq!(d.app.preview_placeholder(), "(no session selected)");

    let mut away = session(1, "/srv/away", SessionStatus::Idle);
    away.host = HostId("box".into());
    away.pool_session = Some("cm-away".into());
    away.window_id = None;
    d.set_sessions(vec![away]);
    assert_eq!(
        d.app.preview_placeholder(),
        "(detached — attach with Enter to preview)"
    );

    // A local row with a live binding is genuinely mid-fetch.
    d.set_sessions(vec![session(2, "/home/test/here", SessionStatus::Idle)]);
    assert_eq!(d.app.preview_placeholder(), "(loading…)");
}

/// The attach overlay is the only feedback an `Enter` on a detached row gets:
/// the attach runs inline in the run loop, so nothing repaints until the window
/// is up. It draws over whatever else is on screen, independent of `input_mode`
/// (the `Space s` steal reaches it straight out of a confirm).
#[test]
fn the_attaching_overlay_draws_over_everything() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    assert!(!d.render().contains("Attaching"));

    d.app.attaching = Some("cm-away".to_string());
    let out = d.render();
    assert!(out.contains("Attaching to cm-away…"), "{out}");
}

/// `backend_for` must never silently fall back to localhost (§9's one
/// correctness-grade leak): a row carrying a host that's no longer configured
/// would otherwise aim its kill or its open at the wrong machine.
#[test]
fn backend_for_reports_an_unknown_host_instead_of_guessing() {
    use crate::state::HostId;
    let d = TestDashboard::new(120, 10);
    assert!(d.app.backend_for(&HostId::local()).is_some());
    assert!(d.app.backend_for(&HostId("ghost".into())).is_none());
}

/// The reconnect sweep fires on a `Disconnected → Connected` edge only, and only
/// for sessions the dashboard *expects* to be attached to (§7). A first sighting
/// is the initial connect, not a reconnect, so it must not queue anything.
#[test]
fn reconnect_sweep_only_reattaches_expected_sessions() {
    use crate::state::HostId;
    let host = HostId("box".into());
    let mut d = TestDashboard::new(120, 10);

    let mut s = session(1, "/srv/p", SessionStatus::Idle);
    s.host = host.clone();
    s.pool_session = Some("cm-1".into());
    let mut other = session(2, "/srv/q", SessionStatus::Idle);
    other.host = host.clone();
    other.pool_session = Some("cm-2".into());
    d.app.sessions = vec![s, other];

    // `cm-1` was attached and its window died with the link; `cm-2` was never
    // attached at all.
    d.app
        .window_bindings
        .record(host.clone(), "cm-1".into(), WindowId::from(9u64));
    d.app.window_bindings.prune_dead(&Default::default());

    // First sighting of the host is the *initial* connect, not a reconnect:
    // startup recovery is the binding re-seed's job, so nothing is queued.
    assert!(d.app.reattach_targets(&host, None, 0).is_empty());
    // An unchanged epoch means the host never dropped.
    assert!(d.app.reattach_targets(&host, Some(3), 3).is_empty());

    // A real reconnect brings back exactly what was open — `cm-1`, not `cm-2`,
    // which the user never attached to.
    assert_eq!(
        d.app.reattach_targets(&host, Some(0), 1),
        vec!["cm-1".to_string()]
    );

    // A deliberate `D` retires the expectation for good, so a later reconnect
    // leaves that session detached — the whole point of tracking intent.
    d.app.window_bindings.remove(&host, "cm-1");
    assert!(d.app.reattach_targets(&host, Some(1), 2).is_empty());
}

/// A host's flags are its own: when the host serves them, the dashboard adopts
/// what it reports rather than keeping a divergent local copy (§9).
#[test]
fn host_served_flags_are_adopted_onto_rows() {
    use crate::state::{HostId, SessionFlags as HostFlags};
    let mut d = TestDashboard::new(120, 10);
    let mut s = session(1, "/srv/p", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-1".into());
    s.flags = Some(HostFlags {
        pinned: true,
        follow_up: false,
    });
    d.app.sessions = vec![s];
    d.app.adopt_host_flags();

    let key = super::flag_key(&d.app.sessions[0]);
    assert!(d.app.flags_of(&key).pinned);
    // A locally-issued pin sequence is assigned so it sorts among our own pins.
    assert!(d.app.flags_of(&key).pin_seq > 0);
}

/// The tab label is what the dashboard says about itself while you are looking
/// at some *other* tab, so the number behind it has to be the whole picture —
/// and has to disappear when there is nothing to say.
#[test]
fn the_tab_label_carries_the_unfiltered_attention_count() {
    use super::dashboard_tab_title;
    let mut d = TestDashboard::new(120, 10);
    let waiting = session(1, "/home/test/a", SessionStatus::WaitingForApproval);
    let deciding = session(2, "/home/test/b", SessionStatus::WaitingForDecision);
    d.set_sessions(vec![
        waiting.clone(),
        deciding.clone(),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);
    assert_eq!(d.app.attention_count(), 2);
    assert_eq!(dashboard_tab_title(d.app.attention_count()), "miao (2)");

    // A search narrows what you're looking at, never what wants you — and the
    // label is read from a tab where the filter isn't even visible.
    d.app.search_filter = Some("zzz".into());
    d.app.mark_dirty(Cursor::Top); // what the search handler does; the order is cached
    assert_eq!(d.app.visible_sessions().len(), 0);
    assert_eq!(d.app.attention_count(), 2);

    // Answering one of them takes it off the count.
    let mut answered = waiting.clone();
    answered.status = SessionStatus::Idle;
    d.set_sessions(vec![
        answered,
        deciding.clone(),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);
    assert_eq!(d.app.attention_count(), 1);
    assert_eq!(dashboard_tab_title(d.app.attention_count()), "miao (1)");

    // Nothing waiting reads as a bare name: a parenthesised zero that is always
    // on screen is exactly the thing the count exists to *not* be.
    let mut decided = deciding.clone();
    decided.status = SessionStatus::Idle;
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        decided,
        session(3, "/home/test/c", SessionStatus::Active),
    ]);
    assert_eq!(d.app.attention_count(), 0);
    assert_eq!(dashboard_tab_title(0), "miao");
}

#[test]
fn attention_sessions_sort_first() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(3, "/home/test/a", SessionStatus::WaitingForApproval),
    ]);

    // Attention sessions sort first, so pid 3 (WaitingForApproval) is row 0
    // and pid 1 (Active) is row 1.
    assert_eq!(d.selected(), Some(0));
    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::FocusWindow(w)) if w == WindowId::from(300)));

    d.press(KeyCode::Char('j'));
    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::FocusWindow(w)) if w == WindowId::from(100)));
}

#[test]
fn failed_to_start_floats_to_attention_rank() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::FailedToStart),
    ]);
    // A failed launch is needs-attention, so it sorts above both the working
    // and idle rows — row 0 is the failed pid 3 (window 300).
    assert_eq!(d.selected(), Some(0));
    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::FocusWindow(w)) if w == WindowId::from(300)));
}

#[test]
fn review_pending_ranks_below_follow_up_above_idle() {
    let mut d = TestDashboard::new(120, 15);
    // updated_at descending by pid so a pure recency sort would order the rows
    // 4,3,2,1 — the assertion below proves the rank tiers dominate recency.
    let mut approval = session(1, "/home/test/a", SessionStatus::WaitingForApproval);
    approval.updated_at = 700;
    let mut follow = session(2, "/home/test/b", SessionStatus::Idle);
    follow.updated_at = 800;
    let mut review = session(3, "/home/test/c", SessionStatus::ReviewPending);
    review.updated_at = 900;
    let mut idle = session(4, "/home/test/d", SessionStatus::Idle);
    idle.updated_at = 1000;
    d.set_sessions(vec![approval, follow, review, idle]);

    // Mark pid 2 needs-input (follow_up) so it occupies the follow-up tier.
    d.app.update_flags(
        super::flag_key(&d.app.sessions[1]),
        Cursor::HoldIndex,
        |f| {
            f.follow_up = true;
        },
    );

    // Tiers, top to bottom: attention (approval) > follow-up-flagged >
    // ReviewPending > plain idle. So ReviewPending (pid 3) ranks *below* the
    // follow-up row (pid 2) but *above* the plain idle row (pid 4).
    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![1, 2, 3, 4]);
}

#[test]
fn clearing_follow_up_on_focus_keeps_cursor_on_the_session() {
    let mut d = TestDashboard::new(120, 15);
    // pid 1 is older; once it drops out of the follow-up tier it sorts *below*
    // the more-recent pid 2 (plain idle sorts newest-first).
    let mut older = session(1, "/home/test/a", SessionStatus::Idle);
    older.updated_at = 100;
    let mut newer = session(2, "/home/test/b", SessionStatus::Idle);
    newer.updated_at = 200;
    d.set_sessions(vec![older, newer]);

    // Mark pid 1 needs-input so it floats to the follow-up tier at the top.
    let key = super::flag_key(&d.app.sessions[0]);
    d.app
        .update_flags(key.clone(), Cursor::HoldIndex, |f| f.follow_up = true);
    assert_eq!(
        d.app
            .visible_sessions()
            .iter()
            .map(|s| s.launcher_pid)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "follow-up row should sit at the top"
    );

    // Cursor on the follow-up row (index 0 == pid 1), as Enter would find it.
    d.app.table_state.select(Some(0));

    // Clearing the follow-up (the Enter/focus path) drops pid 1 to plain idle,
    // so it re-sorts below pid 2 — but the cursor must follow pid 1 there, not
    // stay at index 0 and land on pid 2.
    d.app.clear_follow_up(key);

    assert_eq!(d.app.selected_pid(), Some(1), "cursor should follow pid 1");
    assert_eq!(
        d.selected(),
        Some(1),
        "pid 1 re-sorted to index 1, and the cursor moved with it"
    );
}

#[test]
fn tab_ids_resolved_from_cache_local_only() {
    use crate::state::HostId;
    use crate::terminal::TabId;

    let mut d = TestDashboard::new(120, 15);
    let mut local = session(1, "/home/test/a", SessionStatus::Idle);
    local.tab_id = None; // launcher no longer resolves it
    let mut remote = session(2, "/home/test/b", SessionStatus::Idle);
    remote.host = HostId("box".into());
    remote.tab_id = None;
    // set_sessions seeds the local row's (launch_id → window) binding, as a live
    // spawn would; the remote row has no pool_session, so it resolves to no local
    // window and is never queued for a snapshot.
    d.set_sessions(vec![local, remote]);

    // Window 100 (local pid 1) is unresolved; the remote row's window is never
    // resolved locally, so it isn't queued for a snapshot.
    assert_eq!(
        d.app.unresolved_local_tab_windows(),
        vec![WindowId::from(100)]
    );

    // A warm cache fills the local row's tab id and leaves the remote one None.
    d.app
        .window_tab_cache
        .insert(WindowId::from(100), TabId::from(9));
    d.app.fill_tab_ids_from_cache();
    assert_eq!(d.app.sessions[0].tab_id, Some(TabId::from(9)));
    assert_eq!(d.app.sessions[1].tab_id, None);
    // Now resolved → no snapshot needed next reload.
    assert!(d.app.unresolved_local_tab_windows().is_empty());
}

#[test]
fn newly_failed_windows_fires_once_on_transition() {
    use super::FlagKey;
    use std::collections::HashMap;

    let mut d = TestDashboard::new(120, 10);
    let failed = session(1, "/home/test/a", SessionStatus::FailedToStart);
    // Seed the held window's binding (a dashboard-spawned launch that then failed
    // still recorded its window), so it resolves through `window_id_for_session`.
    d.set_sessions(vec![failed.clone()]);

    // Fresh row (no prior status) → queued.
    let prev: HashMap<FlagKey, SessionStatus> = HashMap::new();
    let got = d
        .app
        .newly_failed_windows(&prev, std::slice::from_ref(&failed));
    assert_eq!(got, vec![WindowId::from(100)]);

    // Starting → FailedToStart transition → queued.
    let prev: HashMap<FlagKey, SessionStatus> =
        [(super::flag_key(&failed), SessionStatus::Starting)].into();
    let got = d
        .app
        .newly_failed_windows(&prev, std::slice::from_ref(&failed));
    assert_eq!(got, vec![WindowId::from(100)]);

    // Already FailedToStart last reload → not re-queued (focus fires once).
    let prev: HashMap<FlagKey, SessionStatus> =
        [(super::flag_key(&failed), SessionStatus::FailedToStart)].into();
    assert!(
        d.app
            .newly_failed_windows(&prev, std::slice::from_ref(&failed))
            .is_empty()
    );
}

#[test]
fn reap_departed_windows_reaps_only_on_floating_backend() {
    // A row that departs without a clean kill (its state file vanished) leaves a
    // held exited pane. On a floating-sessions backend (zellij) it's an invisible
    // leak, so its window is queued for `close_window`; on kitty the held window
    // stays as forensics and nothing is reaped. A still-live row is never touched.
    let mut d = TestDashboard::new(120, 10);
    let gone = session(1, "/home/test/a", SessionStatus::Idle);
    let alive = session(2, "/home/test/b", SessionStatus::Active);
    // `set_sessions` seeds each local row's (launch_id → window_id) binding.
    d.set_sessions(vec![gone.clone(), alive.clone()]);
    // `gone` departs; `alive` stays in the fresh session set.
    d.app.sessions.retain(|s| s.launcher_pid == 2);

    // Kitty (floating_sessions = false): nothing reaped, bindings intact.
    d.app.capabilities.floating_sessions = false;
    assert!(
        d.app
            .reap_departed_windows(&[gone.clone(), alive.clone()])
            .is_empty()
    );
    assert_eq!(d.app.window_bindings.len(), 2);

    // zellij (floating_sessions = true): the departed row's pane (pid 1 * 100) is
    // reaped and its binding dropped; the live row is untouched.
    d.app.capabilities.floating_sessions = true;
    assert_eq!(
        d.app.reap_departed_windows(&[gone.clone(), alive.clone()]),
        vec![WindowId::from(100u64)]
    );
    assert_eq!(
        d.app.window_id_for_session(&alive),
        Some(WindowId::from(200u64))
    );
    // Binding dropped → the token-bearing departed row resolves to nothing.
    assert!(d.app.window_id_for_session(&gone).is_none());
    assert_eq!(d.app.window_bindings.len(), 1);
}

#[test]
fn reap_never_touches_a_hand_launched_rows_own_pane() {
    // A token-less (hand-launched) row has no binding; its window id is the
    // launcher's self-report — the USER'S own pane, not one the dashboard
    // created. Even on a floating-sessions backend its departure must reap
    // nothing: closing it would destroy the user's pane and scrollback.
    let mut d = TestDashboard::new(120, 10);
    let mut hand = session(3, "/home/test/c", SessionStatus::Idle);
    hand.launch_id = None;
    hand.window_id = Some(WindowId::from(300u64));
    d.app.sessions.clear();
    d.app.capabilities.floating_sessions = true;
    assert!(d.app.reap_departed_windows(&[hand]).is_empty());
}

#[test]
fn reap_departed_windows_covers_remote_attach_pane() {
    use crate::state::HostId;
    // A remote row whose pool session died (the mirror dropped it) leaves the
    // local ssh-attach pane held; reap closes it and drops the attach binding. No
    // backend is configured for "box" here, so `backend_for` falls back to the
    // always-Connected local backend — the genuine-death path (a live but
    // *disconnected* host is skipped instead, so a transient blip keeps the row).
    let mut d = TestDashboard::new(120, 10);
    d.app.capabilities.floating_sessions = true;
    let mut s = session(1, "/srv/proj", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-1".into());
    s.window_id = None; // a pooled launcher has no local window of its own
    d.app
        .record_window_binding(HostId("box".into()), "cm-1".into(), WindowId::from(777u64));
    // The row has departed: it's absent from the fresh session set.
    assert!(d.app.sessions.is_empty());
    assert_eq!(
        d.app.reap_departed_windows(std::slice::from_ref(&s)),
        vec![WindowId::from(777u64)]
    );
    assert!(d.app.window_bindings.is_empty());
}

#[test]
fn seed_queues_dead_local_binding_pane_for_reap() {
    use crate::state::{HostId, WindowBinding};
    // At startup the binding seed drops a local binding whose launcher pid is dead
    // (a previous dashboard's crashed launcher). On a floating-sessions backend
    // that dropped window is a leaked held pane and must be queued for the run
    // loop to close; on kitty it's dropped without reaping. Live-local and remote
    // bindings are kept and never reaped at seed time.
    let dead_pid = 2_000_000_000u32; // above any real pid_max on either OS
    assert!(!crate::state::is_process_alive(dead_pid));
    let entries = vec![
        WindowBinding {
            window_id: WindowId::from(321u64),
            host: HostId::local().0,
            launcher_pid: dead_pid,
            token: "L-dead".into(),
            terminal: None,
        },
        WindowBinding {
            window_id: WindowId::from(322u64),
            host: HostId::local().0,
            launcher_pid: std::process::id(), // live → kept
            token: "L-live".into(),
            terminal: None,
        },
        WindowBinding {
            window_id: WindowId::from(777u64),
            host: "box".into(),
            launcher_pid: dead_pid, // remote pid, dead locally, but kept unconditionally
            token: "cm-1".into(),
            terminal: None,
        },
    ];

    // Floating backend: the dead local pane is queued; live + remote survive.
    let mut d = TestDashboard::new(120, 10);
    d.app.capabilities.floating_sessions = true;
    d.app.seed_window_bindings(entries.clone());
    assert_eq!(d.app.reap_window_queue, vec![WindowId::from(321u64)]);
    assert_eq!(d.app.window_bindings.len(), 2);

    // Kitty backend: the dead local binding is still dropped, but not reaped.
    let mut d2 = TestDashboard::new(120, 10);
    d2.app.capabilities.floating_sessions = false;
    d2.app.seed_window_bindings(entries);
    assert!(d2.app.reap_window_queue.is_empty());
    assert_eq!(d2.app.window_bindings.len(), 2);
}

#[test]
fn follow_up_transitions_mark_and_clear() {
    use super::{FlagKey, flag_key};
    use std::collections::HashMap;

    let mut d = TestDashboard::new(120, 15);

    // Each rest-entering transition marks follow_up; a row already carrying the
    // bell is skipped even though it entered rest too. Entering `BackgroundServer` (parking a
    // long-running dev server) also arms the bell — but only on a real transition
    // into it, not for a Server row seen for the first time (prev is None at
    // startup). Entering the *busy* `BackgroundActive` (a short-term task) does
    // NOT arm — it's work in progress, and arms on its exit to Idle like Active.
    let active_to_idle = session(1, "/home/test/a", SessionStatus::Idle);
    let bg_to_idle = session(2, "/home/test/b", SessionStatus::Idle);
    let review_to_idle = session(3, "/home/test/c", SessionStatus::Idle);
    let compacting_to_compacted = session(4, "/home/test/d", SessionStatus::Compacted);
    let flagged_to_idle = session(5, "/home/test/e", SessionStatus::Idle);
    let active_to_server = session(6, "/home/test/f", SessionStatus::BackgroundServer);
    let fresh_server = session(7, "/home/test/g", SessionStatus::BackgroundServer);
    let active_to_bg = session(8, "/home/test/h", SessionStatus::BackgroundActive);
    let server_to_idle = session(9, "/home/test/i", SessionStatus::Idle);
    let sessions = vec![
        active_to_idle.clone(),
        bg_to_idle.clone(),
        review_to_idle.clone(),
        compacting_to_compacted.clone(),
        flagged_to_idle.clone(),
        active_to_server.clone(),
        fresh_server.clone(),
        active_to_bg.clone(),
        server_to_idle.clone(),
    ];
    d.app
        .update_flags(flag_key(&flagged_to_idle), Cursor::HoldIndex, |f| {
            f.follow_up = true
        });

    let prev: HashMap<FlagKey, SessionStatus> = [
        (flag_key(&active_to_idle), SessionStatus::Active),
        (flag_key(&bg_to_idle), SessionStatus::BackgroundActive),
        (flag_key(&review_to_idle), SessionStatus::ReviewPending),
        (
            flag_key(&compacting_to_compacted),
            SessionStatus::Compacting,
        ),
        (flag_key(&flagged_to_idle), SessionStatus::Active),
        (flag_key(&active_to_server), SessionStatus::Active),
        // fresh_server has no prev entry → prev is None (dashboard just started).
        (flag_key(&active_to_bg), SessionStatus::Active),
        (flag_key(&server_to_idle), SessionStatus::BackgroundServer),
    ]
    .into();

    let got = d.app.follow_up_transitions(&prev, &sessions);
    assert_eq!(
        got,
        vec![
            (flag_key(&active_to_idle), true),
            (flag_key(&bg_to_idle), true),
            (flag_key(&review_to_idle), true),
            (flag_key(&compacting_to_compacted), true),
            // Active → BackgroundServer arms; a first-seen Server row and the
            // busy Active → BackgroundActive transition do not.
            (flag_key(&active_to_server), true),
            (flag_key(&server_to_idle), true),
        ]
    );

    // A flagged session that goes back to Active clears the flag.
    let resumed = session(6, "/home/test/f", SessionStatus::Active);
    d.app
        .update_flags(flag_key(&resumed), Cursor::HoldIndex, |f| {
            f.follow_up = true
        });
    let prev: HashMap<FlagKey, SessionStatus> =
        [(flag_key(&resumed), SessionStatus::ReviewPending)].into();
    let got = d
        .app
        .follow_up_transitions(&prev, std::slice::from_ref(&resumed));
    assert_eq!(got, vec![(flag_key(&resumed), false)]);
}

#[test]
fn marking_needs_input_keeps_cursor_on_the_session() {
    let mut d = TestDashboard::new(120, 15);
    // Four idle sessions keep insertion order (equal updated_at, stable sort).
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::Idle),
        session(4, "/home/test/d", SessionStatus::Idle),
    ]);

    // Select the 2nd session and mark it needs-input.
    d.press(KeyCode::Char('j'));
    assert_eq!(d.app.selected_pid(), Some(2));
    d.press(KeyCode::Char('i'));

    // End state is needs-attention: pid 2 floats up to the attention tier and the
    // cursor rides up with it, so the user stays on the session they just flagged.
    assert!(d.app.is_follow_up(&(crate::state::HostId::local(), 2)));
    assert_eq!(d.app.selected_pid(), Some(2));
    assert_eq!(
        d.selected(),
        Some(0),
        "pid 2 floated to the top attention tier and the cursor followed it"
    );
}

#[test]
fn clearing_needs_input_advances_cursor_to_next_session() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::Idle),
        session(4, "/home/test/d", SessionStatus::Idle),
    ]);

    // Flag pid 2; it floats to the top attention tier and the cursor follows.
    d.press(KeyCode::Char('j'));
    d.press(KeyCode::Char('i'));
    assert_eq!(d.app.selected_pid(), Some(2));
    // Order is now [2, 1, 3, 4]; pid 1 sits just below the flagged row.

    // Clearing needs-input (end state not attention) drops pid 2 back to idle.
    // The cursor doesn't follow it down — it lands on what was the *next* row.
    d.press(KeyCode::Char('i'));
    assert!(!d.app.is_follow_up(&(crate::state::HostId::local(), 2)));
    assert_eq!(d.app.selected_pid(), Some(1));
}

#[test]
fn clearing_needs_input_on_last_row_moves_to_previous() {
    let mut d = TestDashboard::new(120, 15);
    // An approval outranks a follow-up, so the flagged row sits *below* it as the
    // last visible row — exercising the next→prev fallback on clear.
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::WaitingForApproval),
        session(2, "/home/test/b", SessionStatus::Idle),
    ]);
    d.app.update_flags(
        super::flag_key(&d.app.sessions[1]),
        Cursor::HoldIndex,
        |f| f.follow_up = true,
    );

    // Select the flagged last row (order is [1, 2]).
    d.app.table_state.select(Some(1));
    assert_eq!(d.app.selected_pid(), Some(2));

    // Clear it: no session below, so the cursor falls back to the previous one.
    d.press(KeyCode::Char('i'));
    assert!(!d.app.is_follow_up(&(crate::state::HostId::local(), 2)));
    assert_eq!(d.app.selected_pid(), Some(1));
}

// -- New feature tests --

#[test]
fn gg_jumps_to_top() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);

    // Move to bottom
    d.press(KeyCode::Char('j'));
    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(2));

    // gg: jump to top
    d.press(KeyCode::Char('g'));
    d.press(KeyCode::Char('g'));
    assert_eq!(d.selected(), Some(0));
}

#[test]
fn shift_g_jumps_to_bottom() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);

    assert_eq!(d.selected(), Some(0));
    d.press(KeyCode::Char('G'));
    assert_eq!(d.selected(), Some(2));
}

#[test]
fn search_mode_filters() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session_with_prompt(1, "/home/test/alpha", SessionStatus::Active, "fix the auth"),
        session_with_prompt(2, "/home/test/beta", SessionStatus::Active, "add tests"),
        session(3, "/home/test/gamma", SessionStatus::Idle),
    ]);

    // Enter search mode
    d.press(KeyCode::Char('/'));
    assert_eq!(d.app.input_mode, InputMode::Search);

    // Type "auth" - should live-filter
    d.press(KeyCode::Char('a'));
    d.press(KeyCode::Char('u'));
    d.press(KeyCode::Char('t'));
    d.press(KeyCode::Char('h'));
    assert_eq!(d.app.visible_sessions().len(), 1);

    // Enter to lock filter
    d.press(KeyCode::Enter);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert_eq!(d.app.search_filter, Some("auth".to_string()));
    assert_eq!(d.app.visible_sessions().len(), 1);

    // Esc clears the filter
    d.press(KeyCode::Esc);
    assert!(d.app.search_filter.is_none());
    assert_eq!(d.app.visible_sessions().len(), 3);
}

/// Clearing the filter makes the list a different list, so the cursor goes back
/// to the top. `ClearSearch` used to be the one search path that forgot — its
/// three siblings each called `reset_selection()` by hand — leaving the cursor
/// on an index that now named an unrelated session. `set_search_filter` owns the
/// policy now (`Cursor::Top`), so all four get it.
#[test]
fn clearing_the_search_filter_returns_the_cursor_to_the_top() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session_with_prompt(1, "/home/test/alpha", SessionStatus::Active, "aaa one"),
        // The unmatched row sits *between* the two matches, so the filtered
        // list is not a prefix of the full one and index 1 genuinely changes
        // meaning when the filter drops.
        session_with_prompt(2, "/home/test/beta", SessionStatus::Active, "zzz two"),
        session_with_prompt(3, "/home/test/gamma", SessionStatus::Active, "aab three"),
    ]);

    // Filter to the two `aa` rows and lock it in.
    d.press(KeyCode::Char('/'));
    for c in "aa".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Enter);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert_eq!(d.app.visible_sessions().len(), 2);

    // Put the cursor on the second match.
    d.app.table_state.select(Some(1));
    let filtered_pid = d.app.selected_pid();

    // Esc clears the filter: the list grows back to three, so index 1 now names
    // a different session than it did a moment ago.
    d.press(KeyCode::Esc);
    assert!(d.app.search_filter.is_none());
    assert_eq!(d.app.visible_sessions().len(), 3);
    assert_eq!(
        d.app.table_state.selected(),
        Some(0),
        "clearing the filter resets the cursor rather than leaving a stale index"
    );
    assert_ne!(
        d.app.nth_visible(1).map(|s| s.launcher_pid),
        filtered_pid,
        "index 1 means a different session now — which is why holding it was a bug"
    );
}

/// `mark_dirty` may only run once a mutation has landed, because every cursor
/// policy *resolves* the visible order and so re-caches it under the freshly
/// bumped version. `reload_sessions` used to invalidate before swapping
/// `sessions`, which re-cached the pre-reload index list as current; the next
/// read then indexed the new, shorter Vec with an old index and panicked. This
/// replays that exact sequence.
#[test]
fn invalidating_before_the_rows_change_would_cache_the_stale_order() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::Idle),
    ]);
    let _ = d.app.visible_sessions(); // a frame drew the list: the cache is warm

    // `reload_sessions`' shape: collect, swap, *then* invalidate. A session that
    // exited leaves one fewer row and — on a plain reload — bumps nothing else.
    let fresh = vec![session(1, "/home/test/a", SessionStatus::Idle)];
    let _prev = std::mem::replace(&mut d.app.sessions, fresh);
    d.app.mark_dirty(Cursor::HoldIndex);

    let visible = d.app.visible_sessions();
    assert_eq!(
        visible.len(),
        1,
        "the visible order must describe the rows that exist now"
    );
    assert_eq!(visible[0].launcher_pid, 1);
    assert_eq!(
        d.app.table_state.selected(),
        Some(0),
        "clamped to the new len"
    );
}

/// The `mark_dirty` contract itself: the cursor argument is what decides where
/// the selection lands across a re-sort, and the two answers are genuinely
/// different. Driven by mutating `flags` directly, so the only thing under test
/// is the cursor policy — `update_flags` would apply one of its own.
#[test]
fn mark_dirty_cursor_decides_where_the_selection_lands() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::Idle),
    ]);
    let pid_at = |d: &TestDashboard, i: usize| d.app.nth_visible(i).map(|s| s.launcher_pid);
    assert_eq!(pid_at(&d, 2), Some(3), "pid 3 starts at the bottom");
    d.app.table_state.select(Some(2));

    // Pin the bottom row so it floats to the top — a re-sort under the cursor.
    // Straight into `flags`, so the version is untouched until `mark_dirty`.
    let pin = |d: &mut TestDashboard| {
        d.app
            .flags
            .entry((crate::state::HostId::local(), 3))
            .or_default()
            .pinned = true;
    };
    let unpin = |d: &mut TestDashboard| {
        d.app.flags.remove(&(crate::state::HostId::local(), 3));
    };

    pin(&mut d);
    d.app.mark_dirty(Cursor::FollowSession);
    assert_eq!(
        d.app.selected_pid(),
        Some(3),
        "FollowSession rides the row to its new rank"
    );
    assert_eq!(d.app.table_state.selected(), Some(0));

    // Same mutation, the other policy: the index stays and whatever re-sorted
    // into it comes to the cursor.
    unpin(&mut d);
    d.app.mark_dirty(Cursor::HoldIndex);
    d.app.table_state.select(Some(2));
    let _ = d.app.visible_sessions(); // warm the order cache, as a frame would
    pin(&mut d);
    d.app.mark_dirty(Cursor::HoldIndex);
    assert_eq!(d.app.table_state.selected(), Some(2), "HoldIndex holds it");
    assert_eq!(
        d.app.selected_pid(),
        pid_at(&d, 2),
        "…so the cursor now names whichever session slid into that slot"
    );
    assert_ne!(d.app.selected_pid(), Some(3), "which is not the pinned one");

    // And Top ignores both.
    d.app.mark_dirty(Cursor::Top);
    assert_eq!(d.app.table_state.selected(), Some(0));
}

fn picker_input_text(app: &super::App) -> &str {
    app.picker.as_ref().expect("picker").picker.input.text()
}

#[test]
fn shift_o_enters_workdir_picker() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/myproj", SessionStatus::Active)]);

    let action = d.press(KeyCode::Char('O'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);
    // Filter input is empty so the full recent-cwds list stays visible.
    assert!(picker_input_text(&d.app).is_empty());

    for c in "~/myproj".chars() {
        d.press(KeyCode::Char(c));
    }
    let action = d.press(KeyCode::Enter);
    match action {
        // The path travels in the host-canonical `~` form (§3) — the *host*
        // expands it, so the dashboard never needs to know any machine's home.
        Some(Action::NewSessionSplit { cwd, .. }) => {
            assert_eq!(cwd, "~/myproj");
        }
        _ => panic!("expected NewSessionSplit action, got {:?}", action),
    }
    assert_eq!(d.app.input_mode, InputMode::Normal);
}

#[test]
fn workdir_picker_can_be_edited() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    d.press(KeyCode::Char('O'));
    // Filter input starts empty; type "~/b".
    for c in "~/b".chars() {
        d.press(KeyCode::Char(c));
    }
    assert_eq!(picker_input_text(&d.app), "~/b");

    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "~/b"),
        _ => panic!("expected NewSessionSplit"),
    }
}

/// The picker submits the host-canonical form, but its *validation* runs against
/// the real filesystem — and `Path::is_dir` doesn't expand a `~` (nothing here
/// is a shell). So a typed `~/foo` has to be expanded before the check, or every
/// tilde path would be rejected as "not a directory".
#[test]
fn workdir_picker_validates_the_expanded_path() {
    let mut d = TestDashboard::new(120, 15);
    // Only the real, expanded path "exists".
    d.app.dir_exists = |p| p == "/home/test/foo";

    d.press(KeyCode::Char('O'));
    for c in "~/foo".chars() {
        d.press(KeyCode::Char(c));
    }
    match d.press(KeyCode::Enter) {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "~/foo"),
        other => panic!("tilde path should validate and launch, got {other:?}"),
    }

    // And a path that genuinely doesn't exist is still rejected, with the
    // picker left open so the user can correct it.
    let mut d = TestDashboard::new(120, 15);
    d.app.dir_exists = |p| p == "/home/test/foo";
    d.press(KeyCode::Char('O'));
    for c in "~/nope".chars() {
        d.press(KeyCode::Char(c));
    }
    assert!(d.press(KeyCode::Enter).is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);
}

#[test]
fn workdir_picker_esc_cancels() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.press(KeyCode::Char('O'));
    // Input starts empty, so a single Esc cancels the picker immediately.
    let action = d.press(KeyCode::Esc);
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.picker.is_none());
}

#[test]
fn workdir_picker_submits_the_canonical_form() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    // Clear pre-seeded text with Ctrl-U, then type "~/foo"
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "~/foo".chars() {
        d.press(KeyCode::Char(c));
    }

    let action = d.press(KeyCode::Enter);
    match action {
        // Unchanged, not expanded: the host owns the expansion (§3).
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "~/foo"),
        _ => panic!("expected NewSessionSplit"),
    }
}

#[test]
fn workdir_picker_lists_recent_cwds() {
    let mut d = TestDashboard::new(120, 15);
    // The list arrives host-canonical from the backend, so what's stored, what's
    // shown, and what's submitted are all one string (§3).
    d.app.recent_cwds = vec!["~/alpha".to_string(), "/tmp/work".to_string()];
    d.press(KeyCode::Char('O'));
    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.items.len(), 2);
    assert_eq!(picker.items[0].primary, "~/alpha");
    assert_eq!(picker.items[1].primary, "/tmp/work");
    assert_eq!(picker.items[0].payload.as_deref(), Some("~/alpha"));
}

#[test]
fn workdir_picker_selects_recent_over_free_input() {
    let mut d = TestDashboard::new(120, 15);
    d.app.recent_cwds = vec![
        "/home/test/alpha".to_string(),
        "/home/test/beta".to_string(),
    ];
    d.press(KeyCode::Char('O'));
    // Clear the pre-seed so both recents match.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    // Highlight the second recent via Down arrow, then Enter.
    d.press(KeyCode::Down);
    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "/home/test/beta"),
        _ => panic!("expected NewSessionSplit"),
    }
}

#[test]
fn workdir_picker_free_input_when_no_match() {
    let mut d = TestDashboard::new(120, 15);
    d.app.recent_cwds = vec!["/home/test/alpha".to_string()];
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    // Type a path that matches no recent.
    for c in "/var/log".chars() {
        d.press(KeyCode::Char(c));
    }
    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.filtered().len(), 0);

    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "/var/log"),
        _ => panic!("expected NewSessionSplit, got {:?}", action),
    }
}

#[test]
fn workdir_picker_filter_match_beats_raw_typed_text() {
    // Typing a filter fragment ("sys") that doesn't itself name a directory
    // must launch the highlighted recent (~/.system-config), not the raw text.
    let mut d = TestDashboard::new(120, 15);
    d.app.dir_exists = |p| p == "/home/test/.system-config";
    d.app.recent_cwds = vec!["/home/test/.system-config".to_string()];
    d.press(KeyCode::Char('O'));
    for c in "sys".chars() {
        d.press(KeyCode::Char(c));
    }
    // The recent matches the filter and is highlighted (cursor 0).
    assert_eq!(d.app.picker.as_ref().unwrap().picker.filtered().len(), 1);

    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "/home/test/.system-config"),
        _ => panic!("expected NewSessionSplit, got {:?}", action),
    }
    assert_eq!(d.app.input_mode, InputMode::Normal);
}

#[test]
fn workdir_picker_literal_path_beats_substring_recent() {
    // A typed string that already names a directory is taken literally even
    // when it substrings a recent cwd (the old precedence bug, in reverse).
    let mut d = TestDashboard::new(120, 15);
    d.app.dir_exists = |p| p == "/var/log";
    d.app.recent_cwds = vec!["/var/log/archive".to_string()];
    d.press(KeyCode::Char('O'));
    for c in "/var/log".chars() {
        d.press(KeyCode::Char(c));
    }
    // The recent substrings the typed path, so it's highlighted...
    assert_eq!(d.app.picker.as_ref().unwrap().picker.filtered().len(), 1);

    // ...but the typed path is itself a directory, so it wins.
    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "/var/log"),
        _ => panic!("expected NewSessionSplit, got {:?}", action),
    }
}

#[test]
fn workdir_picker_rejects_nonexistent_directory() {
    let mut d = TestDashboard::new(120, 15);
    d.app.dir_exists = |_| false;
    d.press(KeyCode::Char('O'));
    for c in "/does/not/exist".chars() {
        d.press(KeyCode::Char(c));
    }
    let action = d.press(KeyCode::Enter);
    // No launch; the picker stays open and shows an inline error.
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);
    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert!(picker.error.is_some());
    assert!(picker.error.as_deref().unwrap().contains("/does/not/exist"));

    // Editing the input clears the error.
    d.press(KeyCode::Backspace);
    assert!(d.app.picker.as_ref().unwrap().picker.error.is_none());
}

#[test]
fn workdir_picker_navigation_overrides_literal_typed_path() {
    // Even when the typed text is a valid directory, explicit Up/Down to a
    // recent honors the highlight.
    let mut d = TestDashboard::new(120, 15);
    d.app.dir_exists = |_| true;
    d.app.recent_cwds = vec![
        "/home/test/alpha".to_string(),
        "/home/test/beta".to_string(),
    ];
    d.press(KeyCode::Char('O'));
    for c in "/home".chars() {
        d.press(KeyCode::Char(c));
    }
    // Both recents match "/home"; arrow down to the second, then Enter.
    d.press(KeyCode::Down);
    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::NewSessionSplit { cwd, .. }) => assert_eq!(cwd, "/home/test/beta"),
        _ => panic!("expected NewSessionSplit, got {:?}", action),
    }
}

#[test]
fn tab_completion_cycles_through_matches() {
    let test_root = std::env::temp_dir().join("captain-miao-test-completion");
    let _ = std::fs::remove_dir_all(&test_root);
    std::fs::create_dir_all(test_root.join("alpha")).unwrap();
    std::fs::create_dir_all(test_root.join("apple")).unwrap();
    std::fs::create_dir_all(test_root.join("apricot")).unwrap();
    std::fs::create_dir_all(test_root.join("banana")).unwrap();

    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    // Clear pre-seed and type "<test_root>/a".
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    let prefix = format!("{}/a", test_root.display());
    for c in prefix.chars() {
        d.press(KeyCode::Char(c));
    }

    // First Tab: completes to first match (alpha)
    d.press(KeyCode::Tab);
    assert!(
        picker_input_text(&d.app).ends_with("alpha/"),
        "got {:?}",
        picker_input_text(&d.app)
    );

    // Second Tab: cycles to apple
    d.press(KeyCode::Tab);
    assert!(picker_input_text(&d.app).ends_with("apple/"));

    // Third Tab: cycles to apricot
    d.press(KeyCode::Tab);
    assert!(picker_input_text(&d.app).ends_with("apricot/"));

    // Fourth Tab: wraps around to alpha (all three share the "a" prefix, and
    // since the current text no longer matches any candidate, we snap back to
    // the first).
    d.press(KeyCode::Tab);
    assert!(picker_input_text(&d.app).ends_with("alpha/"));

    let _ = std::fs::remove_dir_all(&test_root);
}

#[test]
fn session_name_displayed() {
    // Width chosen so the last-prompt column (elastic, after the fixed 45-cell
    // name column + 36-cell detail panel) still has room for the full prompt.
    let mut d = TestDashboard::new(160, 12);
    d.set_sessions(vec![
        session_with_prompt(
            1,
            "/home/test/proj",
            SessionStatus::Active,
            "fix the auth bug in login",
        ),
        session(2, "/home/test/other", SessionStatus::Active),
    ]);
    let out = d.render();
    assert!(
        out.contains("fix the auth bug"),
        "should show prompt as auto-title"
    );
    // Sessions without a prompt and no random_names entry (set_sessions
    // bypasses reload_sessions) fall through to the `session-{pid}` literal.
    assert!(
        out.contains("session-2"),
        "should show pid-keyed fallback name"
    );
}

#[test]
fn space_i_opens_dir_edit_popup() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('i'));
    assert_eq!(d.app.input_mode, InputMode::DirEdit);
    let state = d
        .app
        .dir_edit
        .as_ref()
        .expect("dir-edit popup should be open");
    assert_eq!(state.cwd, "/home/test/proj");
}

#[test]
fn dir_edit_opens_focused_on_custom_text() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
}

#[test]
fn dir_edit_enter_persists_override_and_closes() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    // Type a label so the override is observably distinct from the path's
    // default. (Saving with empty text is also a valid override — meaning
    // "use the default emoji" — but is harder to assert against.)
    d.press(KeyCode::Char('p'));
    d.press(KeyCode::Char('y'));
    d.press(KeyCode::Enter);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.dir_edit.is_none());
    let mark = d
        .app
        .directory_marks
        .get("/home/test/proj")
        .expect("an override should have been written");
    assert_eq!(mark.icon, "py");
    assert!(!mark.color.is_empty());
}

#[test]
fn dir_edit_esc_cancels_without_saving() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    d.press(KeyCode::Char('x'));
    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.dir_edit.is_none());
    assert!(
        d.app.directory_marks.is_empty(),
        "esc should not have persisted anything"
    );
}

#[test]
fn dir_edit_tab_cycles_custom_color() {
    let mut d = TestDashboard::new(120, 22);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    // Initial focus is the custom-text input (so the user can start typing
    // immediately on `Space c`).
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
    d.press(KeyCode::Tab);
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Color
    );
    d.press(KeyCode::Tab);
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
}

#[test]
fn dir_edit_ctrl_e_opens_emoji_picker_from_icon_field() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    // Ctrl-E from the icon field opens the searchable emoji picker. The editor
    // state stays alive underneath so the pick has somewhere to land.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(matches!(
        d.app.picker.as_ref().unwrap().kind,
        super::PickerKind::Emoji
    ));
    assert!(
        d.app.dir_edit.is_some(),
        "editor state should persist under the picker"
    );
}

#[test]
fn dir_edit_ctrl_e_ignored_when_color_focused() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    d.press(KeyCode::Tab); // move focus off the icon field onto Color
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Color
    );
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    // The picker is only reachable from the icon field.
    assert_eq!(d.app.input_mode, InputMode::DirEdit);
    assert!(d.app.picker.is_none());
}

#[test]
fn emoji_picker_filter_and_select_sets_icon_and_returns_to_editor() {
    let rocket = emojis::get("\u{1F680}").unwrap().as_str();
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    // Filter by name down to the rocket, then submit.
    for c in "rocket".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Enter);
    // Submitting drops the emoji into the icon field and returns to the editor
    // (not the normal view) so the user can still pick a color / save.
    assert_eq!(d.app.input_mode, InputMode::DirEdit);
    assert!(d.app.picker.is_none());
    assert_eq!(d.app.dir_edit.as_ref().unwrap().custom.text(), rocket);
    // And saving from the editor persists that emoji as the directory icon.
    d.press(KeyCode::Enter);
    let mark = d
        .app
        .directory_marks
        .get("/home/test/proj")
        .expect("an override should have been written");
    assert_eq!(mark.icon, rocket);
}

#[test]
fn emoji_picker_renders_search_and_named_rows() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    // Filter so a known emoji name is on-screen regardless of list length.
    for c in "rocket".chars() {
        d.press(KeyCode::Char(c));
    }
    let out = d.render();
    assert!(out.contains("Emoji"), "picker title should render");
    assert!(out.contains("rocket"), "filtered emoji name should render");
}

#[test]
fn emoji_picker_cancel_returns_to_editor_without_changing_icon() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    d.press(KeyCode::Char('x')); // a label the user typed before opening the picker
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(d.app.input_mode, InputMode::Picker);
    // Esc with an empty filter cancels the picker back into the editor.
    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::DirEdit);
    assert!(d.app.picker.is_none());
    // The pre-existing icon text is untouched.
    assert_eq!(d.app.dir_edit.as_ref().unwrap().custom.text(), "x");
}

#[test]
fn dir_edit_jk_in_custom_inserts_text_and_tab_leaves() {
    // j/k must stay typeable inside the custom-text row — otherwise users
    // can never leave via the same vim keys, and common labels like "jvm"
    // or "k8s" would be impossible to enter.
    let mut d = TestDashboard::new(120, 22);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
    d.press(KeyCode::Char('j'));
    d.press(KeyCode::Char('k'));
    assert_eq!(d.app.dir_edit.as_ref().unwrap().custom.text(), "jk");
    // Tab leaves the row even when text is present.
    d.press(KeyCode::Tab);
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Color
    );
}

#[test]
fn dir_edit_custom_text_capped_at_max_chars() {
    let mut d = TestDashboard::new(120, 22);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    // Push 6 ASCII chars; only the first ICON_SLOT_WIDTH (2 cells) survive — the
    // cap is the slot the icon column reserves, so what the editor accepts is
    // exactly what fits without clipping.
    for c in ['a', 'b', 'c', 'd', 'e', 'f'] {
        d.press(KeyCode::Char(c));
    }
    use unicode_width::UnicodeWidthStr;
    let custom = d.app.dir_edit.as_ref().unwrap().custom.text();
    assert_eq!(custom, "ab");
    assert_eq!(
        custom.width(),
        super::format::ICON_SLOT_WIDTH,
        "the editor's cap must be the icon slot, or a legal mark won't fit its \
         own fixed-width column"
    );
}

/// ^n/^p switch rows alongside Tab and the arrows. They are what the pickers
/// bind for the same motion, and `TextInput` deliberately leaves them alone so
/// the list around it can have them — including from *inside* the text field,
/// which is the case that would otherwise trap the user in it.
#[test]
fn dir_edit_ctrl_n_p_switch_rows() {
    let mut d = TestDashboard::new(120, 22);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
    d.press_ctrl(KeyCode::Char('n'));
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Color
    );
    d.press_ctrl(KeyCode::Char('p'));
    assert_eq!(
        d.app.dir_edit.as_ref().unwrap().focus,
        super::DirEditFocus::Custom
    );
    // The text field never sees them, so nothing was typed on the way.
    assert!(d.app.dir_edit.as_ref().unwrap().custom.is_empty());
}

/// The colour palette is inert while the mark is an emoji — a colour emoji font
/// paints its own hues — so the editor says so instead of leaving the user to
/// conclude the colour keys are broken. It has to say it in the **default**
/// case too: the derived mark is always an emoji, so an untouched directory
/// opens straight into it.
#[test]
fn dir_edit_says_the_color_is_inert_for_an_emoji_icon() {
    let mut d = TestDashboard::new(120, 22);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);
    d.app.open_dir_edit();
    let out = d.render();
    assert!(out.contains("no effect on emoji"), "{out}");

    // A text icon is what the palette actually reaches, so the caveat goes away
    // as soon as one is typed — it tracks the icon, it isn't a fixture.
    d.press(KeyCode::Char('a'));
    d.press(KeyCode::Char('b'));
    let out = d.render();
    assert!(!out.contains("no effect on emoji"), "{out}");
}

#[test]
fn t_returns_fetch_tabs_action() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    let action = d.press(KeyCode::Char('t'));
    assert!(matches!(action, Some(Action::FetchTabsForMove(w)) if w == WindowId::from(100)));
}

#[test]
fn t_disabled_when_move_to_tab_unsupported() {
    let mut d = TestDashboard::new(120, 15);
    // Simulate a backend (zellij) that can't reparent a pane across tabs.
    d.app.capabilities.move_to_tab = false;
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    let action = d.press(KeyCode::Char('t'));
    assert!(
        action.is_none(),
        "t should produce no action when unsupported"
    );
    assert!(d.app.status_is_error);
    assert!(
        d.app
            .status_msg
            .as_deref()
            .unwrap_or("")
            .contains("not supported by this terminal backend"),
        "expected an unsupported-backend status, got {:?}",
        d.app.status_msg
    );
}

/// A backend that can't read a window (Ghostty) must say so in the preview
/// panel and must not arm the auto-refresh timer. Both matter: the fetch loop
/// treats a *failed* capture as evidence the binding is stale, so leaving the
/// call to fail would feed that inference on every tick for every live session.
#[test]
fn preview_reports_a_backend_that_cannot_capture() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    // Capable backend (kitty, the test default): the row is simply waiting.
    assert_eq!(d.app.preview_placeholder(), "(loading…)");
    d.app.preview_window_id = d.app.selected_window_id();
    d.app.preview_fetched_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
    assert!(
        d.app
            .wants_preview_auto_refresh(std::time::Duration::from_secs(3))
    );

    d.app.capabilities.capture = false;
    assert!(
        d.app.preview_placeholder().contains("no preview"),
        "expected the placeholder to name the limitation, got {:?}",
        d.app.preview_placeholder()
    );
    assert!(
        !d.app
            .wants_preview_auto_refresh(std::time::Duration::from_secs(3)),
        "auto-refresh must not fire on a backend that cannot capture"
    );
}

/// Every preview affordance has to disappear together with the capture it
/// depends on — otherwise `R` claims a refresh the fetch loop then declines, and
/// `?` advertises four scroll keys for a panel that never holds a scrollable
/// line.
#[test]
fn preview_keys_are_hidden_when_the_backend_cannot_capture() {
    // Supported (kitty, the test default): `R` refreshes and `?` lists the keys.
    let mut d = TestDashboard::new(120, 44);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.press(KeyCode::Char('R'));
    assert!(!d.app.status_is_error, "{:?}", d.app.status_msg);
    d.press(KeyCode::Char('?'));
    let help = d.render();
    assert!(help.contains("refresh preview now"), "{help}");
    assert!(help.contains("scroll preview up"), "{help}");

    // Ghostty: the row still exists and still focuses, but nothing about the
    // preview is offered.
    let mut d = TestDashboard::new(120, 44);
    d.app.capabilities.capture = false;
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let action = d.press(KeyCode::Char('R'));
    assert!(
        action.is_none(),
        "R should produce no action when unsupported"
    );
    assert!(d.app.status_is_error);
    assert!(
        d.app
            .status_msg
            .as_deref()
            .unwrap_or("")
            .contains("no way to read a window"),
        "expected an unsupported-backend status, got {:?}",
        d.app.status_msg
    );
    d.press(KeyCode::Char('?'));
    let help = d.render();
    assert!(!help.contains("refresh preview now"), "{help}");
    assert!(!help.contains("scroll preview up"), "{help}");
    // The panel toggle stays: the placeholder is worth reading once, and worth
    // reclaiming the rows from afterwards.
    assert!(help.contains("toggle preview panel"), "{help}");
}

/// The capture gate is the *last* resort: a foreign, detached or unbound row
/// has a more specific thing to say, and those must keep saying it.
#[test]
fn more_specific_preview_placeholders_beat_the_capture_gate() {
    let mut d = TestDashboard::new(120, 15);
    d.app.capabilities.capture = false;

    let mut s = session(1, "/home/test/a", SessionStatus::Active);
    s.window_id = None;
    s.launch_id = None;
    d.set_sessions(vec![s]);
    assert_eq!(d.app.preview_placeholder(), "(no window to preview)");

    d.set_sessions(vec![]);
    assert_eq!(d.app.preview_placeholder(), "(no session selected)");
}

#[test]
fn help_overlay_hides_move_tab_when_unsupported() {
    // Supported (kitty, the test default): the move-tab row is present.
    let mut d = TestDashboard::new(120, 44);
    d.press(KeyCode::Char('?'));
    assert!(
        d.render().contains("move window to another tab"),
        "supported backend should list the move-tab hint"
    );

    // Unsupported (zellij): the row is dropped entirely.
    let mut d = TestDashboard::new(120, 44);
    d.app.capabilities.move_to_tab = false;
    d.press(KeyCode::Char('?'));
    assert!(
        !d.render().contains("move window to another tab"),
        "unsupported backend should omit the move-tab hint"
    );
}

#[test]
fn tab_picker_navigation() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    d.app.open_move_tab_picker(
        WindowId::from(100),
        vec![
            TabInfo {
                id: TabId::from(1),
                title: "Tab 1".to_string(),
                window_count: 2,
                is_focused: true,
            },
            TabInfo {
                id: TabId::from(2),
                title: "Tab 2".to_string(),
                window_count: 1,
                is_focused: false,
            },
        ],
    );
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert_eq!(picker_cursor(&d.app), 0);

    // Navigate down through arrow keys (j would be a filter char).
    d.press(KeyCode::Down);
    assert_eq!(picker_cursor(&d.app), 1);

    // Navigate to [New Tab]
    d.press(KeyCode::Down);
    assert_eq!(picker_cursor(&d.app), 2);

    // Wrap around
    d.press(KeyCode::Down);
    assert_eq!(picker_cursor(&d.app), 0);

    // Select tab 1
    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::MoveWindow(ref w, ref t))
        if *w == WindowId::from(100) && *t == TabTarget::Existing(TabId::from(1))));
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.picker.is_none());
}

#[test]
fn tab_picker_new_tab() {
    let mut d = TestDashboard::new(120, 20);

    d.app.open_move_tab_picker(
        WindowId::from(100),
        vec![TabInfo {
            id: TabId::from(1),
            title: "Tab 1".to_string(),
            window_count: 2,
            is_focused: true,
        }],
    );
    // Navigate to [New Tab] (index 1)
    d.press(KeyCode::Down);

    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::MoveWindow(ref w, ref t))
        if *w == WindowId::from(100) && *t == TabTarget::New));
}

#[test]
fn tab_picker_cancel() {
    let mut d = TestDashboard::new(120, 20);

    d.app.open_move_tab_picker(
        WindowId::from(100),
        vec![TabInfo {
            id: TabId::from(1),
            title: "Tab 1".to_string(),
            window_count: 2,
            is_focused: true,
        }],
    );

    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.picker.is_none());
}

#[test]
fn tab_picker_filters() {
    let mut d = TestDashboard::new(120, 20);
    d.app.open_move_tab_picker(
        WindowId::from(100),
        vec![
            TabInfo {
                id: TabId::from(1),
                title: "Editor".to_string(),
                window_count: 1,
                is_focused: true,
            },
            TabInfo {
                id: TabId::from(2),
                title: "Shell".to_string(),
                window_count: 1,
                is_focused: false,
            },
            TabInfo {
                id: TabId::from(3),
                title: "Browser".to_string(),
                window_count: 1,
                is_focused: false,
            },
        ],
    );

    // Filter by typing — "she" matches only "Shell".
    for c in "she".chars() {
        d.press(KeyCode::Char(c));
    }
    let active = d.app.picker.as_ref().unwrap();
    assert_eq!(active.picker.filtered().len(), 1);

    // Selecting the single remaining match should move to tab 2.
    let action = d.press(KeyCode::Enter);
    assert!(matches!(action, Some(Action::MoveWindow(ref w, ref t))
        if *w == WindowId::from(100) && *t == TabTarget::Existing(TabId::from(2))));
}

#[test]
fn tab_picker_esc_clears_filter_before_closing() {
    let mut d = TestDashboard::new(120, 20);
    d.app.open_move_tab_picker(
        WindowId::from(100),
        vec![TabInfo {
            id: TabId::from(1),
            title: "Tab 1".to_string(),
            window_count: 1,
            is_focused: true,
        }],
    );

    d.press(KeyCode::Char('x'));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.text(), "x");

    // First Esc clears the filter, picker stays open.
    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(d.app.picker.as_ref().unwrap().picker.input.is_empty());

    // Second Esc closes the picker.
    d.press(KeyCode::Esc);
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.picker.is_none());
}

fn picker_cursor(app: &super::App) -> usize {
    app.picker.as_ref().expect("picker").picker.cursor
}

#[test]
fn search_footer_shown() {
    let mut d = TestDashboard::new(120, 10);
    d.press(KeyCode::Char('/'));
    d.press(KeyCode::Char('t'));
    d.press(KeyCode::Char('e'));
    let out = d.render();
    assert!(out.contains("/ te"), "should show search input in footer");
    assert!(out.contains("Esc"), "should show cancel hint");
}

#[test]
fn leader_which_key_footer_shown() {
    let mut d = TestDashboard::new(120, 10);
    // Pressing the leader surfaces the available continuations in the footer.
    d.press(KeyCode::Char(' '));
    assert!(d.app.pending_prefix.is_some());
    let out = d.render();
    assert!(
        out.contains("preview"),
        "which-key should list leader options"
    );
    assert!(
        out.contains("restart"),
        "which-key should list leader options"
    );
}

#[test]
fn g_then_other_key_does_not_jump() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);

    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(1));

    // Press g then j (not gg) — should just navigate down
    d.press(KeyCode::Char('g'));
    d.press(KeyCode::Char('j'));
    assert_eq!(d.selected(), Some(2));
}

#[test]
fn preview_panel_always_renders() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    // Preview is always shown when width >= 80
    let out = d.render();
    assert!(
        out.contains("Terminal Preview"),
        "should always show preview panel title"
    );
}

#[test]
fn preview_panel_shows_content() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.app.preview_text = Some("$ hello world\nsome output here".to_string());
    d.app.preview_window_id = Some(WindowId::from(100));

    let out = d.render();
    assert!(
        out.contains("Terminal Preview"),
        "should show preview panel title"
    );
    assert!(out.contains("hello world"), "should show preview content");
}

#[test]
fn preview_panel_hidden_on_short_terminal() {
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let out = d.render();
    assert!(
        !out.contains("Terminal Preview"),
        "should hide preview on short terminals"
    );
}

#[test]
fn wide_layout_name_column_is_fixed_max_width_with_aligned_truncation() {
    use super::format::truncate_str;
    // The Name column is a fixed max-width column — `name_truncate` (35) + 10
    // headroom = 45 by default — and the title is truncated to that *same* width,
    // so an over-long title ends in an ellipsis right at the column edge. Last
    // prompt is the elastic column that yields the remaining width.
    let mut d = TestDashboard::new(140, 12);
    let mut s = session(1, "/home/test/proj", SessionStatus::Active);
    let long = "a-really-long-session-title-that-exceeds-the-max-width-by-a-lot";
    s.name = Some(long.into());
    s.last_prompt = Some("do the thing and then wait for review".into());
    d.set_sessions(vec![s]);
    let out = d.render();

    assert!(!d.app.narrow_layout, "width 140 stays in the wide layout");
    // The title is truncated to the 45-cell column (name_truncate + 10) and ends
    // in an ellipsis exactly at the column edge.
    let truncated = truncate_str(long, 45);
    assert!(
        truncated.ends_with('…'),
        "sanity: the title exceeds the column"
    );
    assert!(
        out.contains(&truncated),
        "title truncates to the column's max width, ellipsis at the edge"
    );
    assert!(
        !out.contains(long),
        "the full over-long title is never shown"
    );
    // Last prompt is still present (elastic) but on a tighter viewport it yields.
    assert!(
        out.contains("do the thing"),
        "last prompt shares the leftover width"
    );
}

#[test]
fn narrow_layout_stacks_panels_and_trims_columns() {
    // A body at or below `narrow_max_width` (90) drops the side-by-side layout
    // for the vertical stack, trims the table to status / icon / name, and shows
    // the compact four-field detail panel.
    let mut d = TestDashboard::new(60, 30);
    let mut s = session(1, "/home/test/proj", SessionStatus::Active);
    s.name = Some("mytitle".into());
    s.model = Some("claude-opus-4-8".into());
    s.last_prompt = Some("do the thing".into());
    d.set_sessions(vec![s]);
    let out = d.render();

    assert!(
        d.app.narrow_layout,
        "60-col body should use the narrow layout"
    );
    // The session title is still shown in the trimmed table.
    assert!(out.contains("mytitle"), "name column survives the trim");
    // Wide-only table columns are dropped.
    assert!(
        !out.contains("Last prompt"),
        "narrow layout drops the last-prompt column"
    );
    assert!(
        !out.contains("Ctx"),
        "narrow layout drops the context column"
    );
    // The compact detail panel shows exactly agent / model / context / updated.
    assert!(out.contains("Agent"), "detail keeps the Agent field");
    assert!(out.contains("Model"), "detail keeps the Model field");
    assert!(out.contains("Context"), "detail keeps the Context field");
    // The wide detail's extra fields are gone.
    assert!(
        !out.contains("First prompt"),
        "compact detail drops first-prompt"
    );
    assert!(!out.contains("PID"), "compact detail drops the PID line");
}

/// An empty context reading means two different things, and both views that
/// show one have to say which. On a backend that reports a total, empty is "the
/// first turn hasn't landed" and a number is coming; on Reasonix or Grok — which
/// persist no context total at all (`AgentCapabilities::context_tokens`) —
/// nothing is ever coming, and a cell that never fills reads as a session
/// stalled before its first reply.
#[test]
fn a_backend_that_reports_no_context_total_says_so_instead_of_pending() {
    let render = |agent: crate::agent::AgentControl, cols: u16| {
        let mut d = TestDashboard::new(cols, 20);
        let mut s = session(1, "/home/test/proj", SessionStatus::Active);
        s.agent = agent;
        s.context_tokens = None;
        d.set_sessions(vec![s]);
        d.render()
    };
    // The wide table's `Ctx` column: blank while Claude has yet to report…
    let claude = render(crate::agent::AgentControl::Claude, 120);
    assert!(
        !claude.contains("n/a"),
        "a pending total leaves the column blank: {claude}"
    );
    // …and marked on the backend that will never report one.
    let grok = render(crate::agent::AgentControl::Grok, 120);
    assert!(
        grok.contains("n/a"),
        "a backend with no context total to give must not read as pending: {grok}"
    );

    // The narrow layout drops that column and shows a `Context` line instead,
    // which needs the same distinction: an em dash pends, `n/a` doesn't.
    let claude_narrow = render(crate::agent::AgentControl::Claude, 60);
    assert!(
        claude_narrow.contains("Context") && !claude_narrow.contains("n/a"),
        "the compact detail pends with an em dash: {claude_narrow}"
    );
    let grok_narrow = render(crate::agent::AgentControl::Grok, 60);
    assert!(
        grok_narrow.contains("Context") && grok_narrow.contains("n/a"),
        "…and says so when nothing is coming: {grok_narrow}"
    );
}

#[test]
fn wide_layout_keeps_full_columns_and_detail() {
    // Wide enough that the fixed 45-cell name column and the 36-cell detail
    // panel still leave the elastic last-prompt column room for its full header.
    let mut d = TestDashboard::new(160, 20);
    let mut s = session(1, "/home/test/proj", SessionStatus::Active);
    s.last_prompt = Some("do the thing".into());
    d.set_sessions(vec![s]);
    let out = d.render();

    assert!(
        !d.app.narrow_layout,
        "160-col body stays in the wide layout"
    );
    assert!(
        out.contains("Last prompt"),
        "wide table keeps the last-prompt column"
    );
    assert!(out.contains("PID"), "wide detail keeps the full field set");
}

/// A pooled session's `TERM` is whatever the client that *created* the pty
/// sent, frozen there for the session's whole life — so a row can be rendering
/// against a terminfo that has nothing to do with the window you are looking at
/// it through, and nothing said so. The detail panel says so. A value matching
/// this dashboard's own terminfo has nothing to report and draws dim.
/// A worktree row's cwd ends in `.claude/worktrees/<name>`, the one thing
/// telling it apart from its repo and the last thing the eye reaches in a
/// wrapped path. It gets a field of its own — beside the *whole* cwd, not
/// instead of it — and an ordinary row grows no empty `Worktree` field.
#[test]
fn the_detail_panel_names_the_worktree_a_session_sits_in() {
    // Tall enough that the preview doesn't clip the panel above `Dir`, and wide
    // enough that the cwd lands on one line — the assertions below read it whole.
    let mut d = TestDashboard::new(160, 40);
    d.app.panels_initialized = true;
    d.app.detail_visible = true;
    d.app.detail_width = 80;

    let plain = session(1, "/home/miao/code/proj", SessionStatus::Idle);
    // The agent allows a `/` in the name, so the whole remainder is the name.
    let tree = session(
        2,
        "/home/miao/code/proj/.claude/worktrees/feature/auth",
        SessionStatus::Idle,
    );
    d.set_sessions(vec![plain, tree]);

    let select = |d: &mut TestDashboard, pid: u32| {
        let at = d
            .app
            .visible_sessions()
            .iter()
            .position(|s| s.launcher_pid == pid)
            .expect("row is visible");
        d.app.table_state.select(Some(at));
    };

    select(&mut d, 1);
    let out = d.render();
    assert!(out.contains("/home/miao/code/proj"), "{out}");
    assert!(!out.contains("Worktree"), "no worktree, no field: {out}");

    select(&mut d, 2);
    let out = d.render();
    assert!(out.contains("Worktree feature/auth"), "{out}");
    // The new field is *additive*. `Dir` is what you copy into a `cd`, so it
    // stays the whole cwd — tail included — rather than the repo it splits to.
    assert!(
        out.contains("Dir      /home/miao/code/proj/.claude/worktrees/feature/auth"),
        "{out}"
    );
}

#[test]
fn the_detail_panel_names_the_terminfo_a_session_renders_against() {
    use crate::state::HostId;
    use ratatui::style::Modifier;
    let mut d = TestDashboard::new(160, 20);
    d.app.panels_initialized = true;
    d.app.detail_visible = true;
    d.app.terminfo = Some("xterm-kitty".into());

    let mut pooled = session(1, "/srv/away", SessionStatus::Idle);
    pooled.host = HostId("box".into());
    pooled.pool_session = Some("cm-away".into());
    // Created from Kitty onto a host with no kitty terminfo: the pool wrapper
    // rewrote it, and every window since has inherited the rewrite.
    pooled.terminfo = Some("xterm-256color".into());
    let mut here = session(2, "/home/test/here", SessionStatus::Idle);
    here.terminfo = Some("xterm-kitty".into());
    d.set_sessions(vec![pooled, here]);

    let select = |d: &mut TestDashboard, pid: u32| {
        let at = d
            .app
            .visible_sessions()
            .iter()
            .position(|s| s.launcher_pid == pid)
            .expect("row is visible");
        d.app.table_state.select(Some(at));
    };
    let style_of = |d: &TestDashboard, needle: &str| {
        let buf = d.terminal.backend().buffer();
        let (x, y) = find_cell(buf, needle).unwrap_or_else(|| panic!("{needle} not drawn"));
        buf[(x, y)].style()
    };

    select(&mut d, 1);
    let out = d.render();
    assert!(out.contains("Term"), "the panel carries a Term field");
    assert!(out.contains("xterm-256color"));
    assert!(
        !style_of(&d, "xterm-256color")
            .add_modifier
            .contains(Modifier::DIM),
        "a terminfo that isn't this terminal's stays bright"
    );

    // The same field on a row that renders the way this dashboard does has
    // nothing to report, so it recedes.
    select(&mut d, 2);
    d.render();
    assert!(
        style_of(&d, "xterm-kitty")
            .add_modifier
            .contains(Modifier::DIM),
        "a matching terminfo draws dim"
    );

    // A host too old to send the field says so rather than inventing one.
    let mut unknown = session(3, "/home/test/old", SessionStatus::Idle);
    unknown.terminfo = None;
    d.set_sessions(vec![unknown]);
    select(&mut d, 3);
    let out = d.render();
    assert!(out.contains("Terminfo"));
    assert!(!out.contains("xterm"), "no terminfo invented: {out}");
}

/// `Enter` on a row another client holds is a *steal*, so it asks. Left to the
/// attach wrapper this was a window that opened, printed libshpool's refusal and
/// closed — the answer arriving where the user isn't looking. The confirm keys
/// on the same bit as the held-elsewhere glyph, so what the row shows and what
/// `Enter` does cannot disagree.
#[test]
fn enter_on_a_row_another_client_holds_offers_the_steal() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut held = session(1, "/srv/held", SessionStatus::Idle);
    held.host = HostId("box".into());
    held.pool_session = Some("cm-held".into());
    held.attached = Some(true);
    d.set_sessions(vec![held.clone()]);

    assert!(
        d.app.focus_or_attach(&held).is_none(),
        "the action waits behind the confirm rather than firing"
    );
    let confirm = d.app.pending_confirm.as_ref().expect("a confirm is armed");
    assert!(confirm.prompt.contains("kick it"), "{}", confirm.prompt);
    assert!(
        matches!(
            confirm.action,
            Action::AttachRemoteRunning { force: true, .. }
        ),
        "answering yes must steal, not re-attempt a plain attach"
    );
    assert_eq!(d.app.input_mode, super::InputMode::Confirm);

    // Free, and unknown, both attach as before — the second matters most: a
    // pool we couldn't read must not put a confirm in front of every row.
    for bit in [Some(false), None] {
        let mut free = session(2, "/srv/free", SessionStatus::Idle);
        free.host = HostId("box".into());
        free.pool_session = Some("cm-free".into());
        free.attached = bit;
        let mut d = TestDashboard::new(120, 12);
        d.set_sessions(vec![free.clone()]);
        assert!(
            matches!(
                d.app.focus_or_attach(&free),
                Some(Action::AttachRemoteRunning { force: false, .. })
            ),
            "attached={bit:?} must attach directly"
        );
        assert!(d.app.pending_confirm.is_none(), "attached={bit:?}");
    }
}

/// `Space A` takes every detached session it *can* take: the free ones. A row
/// another client holds is skipped rather than stolen — one keypress must not
/// kick a roomful of terminals — and a row that already has a window here has
/// nothing to attach. The unknown-bit row rides with the free ones, matching how
/// `Enter` treats it: attach, and let the pool refuse if it must.
#[test]
fn attach_all_takes_the_free_detached_rows_and_leaves_the_held_ones() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);

    let mut free = session(1, "/srv/free", SessionStatus::Idle);
    free.host = HostId("box".into());
    free.pool_session = Some("cm-free".into());
    free.attached = Some(false);
    let mut unknown = session(2, "/srv/unknown", SessionStatus::Idle);
    unknown.host = HostId("box".into());
    unknown.pool_session = Some("cm-unknown".into());
    unknown.attached = None;
    let mut held = session(3, "/srv/held", SessionStatus::Idle);
    held.host = HostId("box".into());
    held.pool_session = Some("cm-held".into());
    held.attached = Some(true);
    // A local row with a window: nothing detached about it.
    let here = session(4, "/home/miao/here", SessionStatus::Idle);
    d.set_sessions(vec![free, unknown, held.clone(), here]);

    // Reached through the real binding, so the default key is under test too.
    assert!(d.press(KeyCode::Char(' ')).is_none(), "leader is pending");
    let action = d
        .press(KeyCode::Char('A'))
        .expect("Space A produces a batch");
    let Action::AttachAll { targets } = action else {
        panic!("expected AttachAll, got {action:?}");
    };
    let names: Vec<&str> = targets.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(names, vec!["cm-free", "cm-unknown"]);
    assert!(
        targets.iter().all(|(h, _)| h.0 == "box"),
        "each target carries its own host"
    );

    // Only held rows left: nothing to do, and the status points at the steal
    // instead of silently reporting success.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![held]);
    assert!(d.app.request_attach_all().is_none());
    let status = d.app.status_msg.clone().expect("a status is set");
    assert!(status.contains("another terminal"), "{status}");
    assert!(
        status.contains("Space s"),
        "names the live steal key: {status}"
    );

    // Nothing detached at all reads differently again.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![session(5, "/home/miao/here", SessionStatus::Idle)]);
    assert!(d.app.request_attach_all().is_none());
    let status = d.app.status_msg.clone().expect("a status is set");
    assert!(status.contains("Nothing to attach"), "{status}");
}

/// A terminfo that isn't this terminal's earns a warning under the value naming
/// what this terminal is — the name to install on the host, which is the remedy
/// however the difference arose. It must not fire when they agree, or on a row
/// whose terminfo we don't know.
#[test]
fn a_foreign_terminfo_warns_and_names_this_terminal() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(160, 20);
    d.app.panels_initialized = true;
    d.app.detail_visible = true;
    // The warning is a continuation line under the value, so the panel needs
    // its full height — with the preview on, the box ends at `Terminfo`.
    d.app.preview_visible = false;
    d.app.terminfo = Some("xterm-kitty".into());

    let mut downgraded = session(1, "/srv/away", SessionStatus::Idle);
    downgraded.host = HostId("box".into());
    downgraded.pool_session = Some("cm-away".into());
    downgraded.terminfo = Some("xterm-256color".into());
    d.set_sessions(vec![downgraded]);
    d.app.table_state.select(Some(0));
    let out = d.render();
    assert!(
        out.contains("not yours (xterm-kitty)"),
        "the warning must name this terminal: {out}"
    );

    // Same terminfo → nothing to warn about.
    let mut same = session(2, "/srv/other", SessionStatus::Idle);
    same.host = HostId("box".into());
    same.pool_session = Some("cm-other".into());
    same.terminfo = Some("xterm-kitty".into());
    d.set_sessions(vec![same]);
    d.app.table_state.select(Some(0));
    let out = d.render();
    assert!(!out.contains("not yours"), "no warning invented: {out}");

    // Unknown terminfo (a host too old to send it) is not a mismatch — there is
    // nothing to compare, and warning on absence would light up every row
    // served by an older daemon.
    let mut unknown = session(3, "/srv/old", SessionStatus::Idle);
    unknown.host = HostId("box".into());
    unknown.pool_session = Some("cm-old".into());
    unknown.terminfo = None;
    d.set_sessions(vec![unknown]);
    d.app.table_state.select(Some(0));
    let out = d.render();
    assert!(
        !out.contains("not yours"),
        "absence is not a mismatch: {out}"
    );
}

#[test]
fn narrow_layout_hides_preview_when_too_short() {
    // In the vertical stack the preview is dynamic-height and disappears when the
    // viewport can't spare room for both a usable table and a usable preview,
    // even with the preview toggle on.
    let mut d = TestDashboard::new(60, 14);
    // Skip the first-draw auto-hide so we exercise the height guard, not the
    // toggle default.
    d.app.panels_initialized = true;
    d.app.preview_visible = true;
    d.app.detail_visible = true;
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let out = d.render();

    assert!(d.app.narrow_layout);
    assert!(
        !out.contains("Terminal Preview"),
        "preview is dropped when the narrow viewport is too short"
    );
    // The compact detail panel still renders above where the preview would be.
    assert!(out.contains("Agent"), "detail stays visible");
}

#[test]
fn preview_auto_refresh_gates() {
    use std::time::{Duration, Instant};
    let mut d = TestDashboard::new(120, 24);
    let interval = Duration::from_secs(10);
    let stale = Instant::now().checked_sub(interval).unwrap();

    // Focused + visible + busy selection + live window + unscrolled + stale
    // fetch → refresh.
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.app.preview_window_id = Some(WindowId::from(100));
    d.app.preview_fetched_at = Some(stale);
    assert!(d.app.wants_preview_auto_refresh(interval));

    // A zero interval disables the timer outright.
    assert!(!d.app.wants_preview_auto_refresh(Duration::ZERO));

    // Each gate independently pauses the refresh.
    d.app.focused = false;
    assert!(!d.app.wants_preview_auto_refresh(interval), "unfocused");
    d.app.focused = true;

    d.app.preview_visible = false;
    assert!(!d.app.wants_preview_auto_refresh(interval), "panel hidden");
    d.app.preview_visible = true;

    d.app.preview_window_id = None;
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "no live window"
    );
    d.app.preview_window_id = Some(WindowId::from(100));

    d.app.preview_scroll = 5;
    assert!(!d.app.wants_preview_auto_refresh(interval), "scrolled up");
    d.app.preview_scroll = 0;

    d.app.preview_h_scroll = 8;
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "scrolled right"
    );
    d.app.preview_h_scroll = 0;

    // At-rest selection: no new output to fetch, so the timer pauses. Any
    // busy status (`is_busy`) resumes it.
    d.app.sessions[0].status = SessionStatus::Idle;
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "selection idle"
    );
    d.app.sessions[0].status = SessionStatus::WaitingForApproval;
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "selection at rest"
    );
    // A short-term background task (`BackgroundActive`) is busy — the agent is
    // waiting on it, output is still flowing — so the timer resumes.
    d.app.sessions[0].status = SessionStatus::BackgroundActive;
    assert!(
        d.app.wants_preview_auto_refresh(interval),
        "background task is busy"
    );
    // A parked long-running server (`BackgroundServer`) is at-rest — the agent
    // isn't working, so the timer stays paused like any other at-rest row.
    d.app.sessions[0].status = SessionStatus::BackgroundServer;
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "parked server is at rest, not busy"
    );
    d.app.sessions[0].status = SessionStatus::Active;

    d.app.preview_fetched_at = Some(Instant::now());
    assert!(
        !d.app.wants_preview_auto_refresh(interval),
        "fetch still fresh"
    );
}

#[test]
fn preview_h_scroll_clamps_to_content_width() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    // Lines far wider than the view, so there's real content to clip.
    d.app.preview_text = Some(
        (0..5)
            .map(|i| format!("{i}: {}", "x".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    d.app.preview_window_id = Some(WindowId::from(100));

    // `scroll_preview_right` saturating-adds unbounded; drive the field far past
    // the clip point, then render.
    d.app.preview_h_scroll = u16::MAX;
    let _ = d.render();

    // The field is clamped to the real clip point (content width minus the
    // preview view width), not left inflated — so scrolling back left moves the
    // view immediately instead of first draining the excess.
    let view_width = d.app.last_preview_rect.unwrap().width as usize;
    let expected = d.app.preview_max_width.saturating_sub(view_width) as u16;
    assert!(expected > 0, "test content must be wider than the view");
    assert_eq!(d.app.preview_h_scroll, expected);
}

#[test]
fn preview_h_scroll_writeback_rearms_auto_refresh() {
    use std::time::{Duration, Instant};
    let mut d = TestDashboard::new(120, 30);
    let interval = Duration::from_secs(10);
    let stale = Instant::now().checked_sub(interval).unwrap();

    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    // Narrow content: nothing to scroll horizontally, so the clip point is 0.
    d.app.preview_text = Some("$ short line\nanother short line".to_string());
    d.app.preview_window_id = Some(WindowId::from(100));
    d.app.preview_fetched_at = Some(stale);

    // An over-scroll right defeats the `preview_h_scroll == 0` auto-refresh
    // gate even though there's nothing past the edge to see...
    d.app.preview_h_scroll = 8;
    assert!(!d.app.wants_preview_auto_refresh(interval));

    // ...but a render clamps the field back to the real clip point (0), so the
    // gate re-arms rather than staying silently stuck off.
    let _ = d.render();
    assert_eq!(d.app.preview_h_scroll, 0);
    assert!(d.app.wants_preview_auto_refresh(interval));
}

#[test]
fn preview_title_shows_age_when_stale() {
    use std::time::{Duration, Instant};
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.app.preview_text = Some("$ hello world".to_string());
    d.app.preview_window_id = Some(WindowId::from(100));

    // No successful fetch recorded (or content cleared): no label.
    d.app.preview_updated_at = None;
    assert_eq!(d.app.preview_age_label(), None);

    // Fresh content: still no label.
    d.app.preview_updated_at = Some(Instant::now());
    assert_eq!(d.app.preview_age_label(), None);
    assert!(
        !d.render().contains("updated"),
        "fresh preview shows no age label"
    );

    // Older than thresholds.preview_stale_secs (default 20) but under a
    // minute: coarse `<1m`, same resolution as the table's Updated column.
    d.app.preview_updated_at = Instant::now().checked_sub(Duration::from_secs(25));
    assert_eq!(
        d.app.preview_age_label().as_deref(),
        Some("updated <1m ago")
    );
    assert!(
        d.render().contains("(updated <1m ago)"),
        "stale preview surfaces its age"
    );

    // Whole minutes once past the first one.
    d.app.preview_updated_at = Instant::now().checked_sub(Duration::from_secs(90));
    assert_eq!(d.app.preview_age_label().as_deref(), Some("updated 1m ago"));
}

#[test]
fn format_coarse_age_has_minute_resolution() {
    assert_eq!(format_coarse_age(0), "<1m");
    assert_eq!(format_coarse_age(59), "<1m");
    assert_eq!(format_coarse_age(60), "1m");
    assert_eq!(format_coarse_age(599), "9m");
    assert_eq!(format_coarse_age(3660), "1h01m");
}

#[test]
fn l_opens_the_hosts_panel_connection_log_and_esc_returns() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.app.open_host_edit();
    // With no configured hosts the cursor sits on "+ add host", where there is
    // no host to log — `l` must not open an empty view over nothing.
    d.press(KeyCode::Char('l'));
    assert!(d.app.host_edit.as_ref().unwrap().log_view.is_none());

    // Give it a row to stand on. (No backend for it, so the log is empty — the
    // view says so rather than showing a blank box.)
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("polaris"),
        target: super::picker::TextInput::with_text("polaris"),
        ..Default::default()
    });
    state.cursor = 0;
    d.press(KeyCode::Char('l'));
    let view = d.app.host_edit.as_ref().unwrap().log_view.as_ref();
    assert_eq!(view.map(|v| v.host.0.as_str()), Some("polaris"));

    let out = d.render();
    assert!(out.contains("connection log"), "{out}");

    // A stray key inside the log is swallowed, not re-read as a list command —
    // `d` here must never reach the removal confirm.
    d.press(KeyCode::Char('d'));
    assert!(d.app.host_edit.as_ref().unwrap().pending_remove.is_none());
    assert!(d.app.host_edit.as_ref().unwrap().log_view.is_some());

    // Esc backs out to the list, not out of the panel.
    d.press(KeyCode::Esc);
    assert!(d.app.host_edit.as_ref().unwrap().log_view.is_none());
    assert_eq!(d.app.input_mode, InputMode::HostEdit);
}

/// A host's ssh options are configured where the host is — the panel's third
/// field — and read back on that host's own row, beside its target.
///
/// The row line is the only place they are ever visible, and a port forward
/// among them has nothing else in the dashboard announcing it: nothing else says
/// a local port is being answered by another machine.
#[test]
fn the_hosts_panel_configures_a_hosts_ssh_options() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    // `a` adds a row on Label; Tab walks Label → Target → Options.
    d.press(KeyCode::Char('a'));
    for c in "box".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Tab);
    for c in "user@box".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Tab);
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().focus(),
        Some(super::HostField::Options)
    );
    for c in "-C -L 8080:localhost:3000".chars() {
        d.press(KeyCode::Char(c));
    }
    let row = &d.app.host_edit.as_ref().unwrap().rows[0];
    assert_eq!(row.target.text(), "user@box");
    assert_eq!(row.options.text(), "-C -L 8080:localhost:3000");

    // Shown as typed: this field has no grammar of its own to canonicalise to.
    let out = d.render();
    assert!(
        out.contains("ssh user@box -C -L 8080:localhost:3000"),
        "{out}"
    );

    // One more Tab reaches Icon, so the cycle still closes.
    d.press(KeyCode::Tab);
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().focus(),
        Some(super::HostField::Icon)
    );
}

/// A host offered the clipboard says so on its row.
///
/// The marker rides the *target* line rather than the status line because that is
/// where a host's forwards are shown, and the clipboard is one more of them — the
/// status line reports live connection state, which this is not. Without it the
/// setting is invisible from the list, which is where you look to see how a host
/// is configured.
#[test]
fn a_host_offered_the_clipboard_shows_it_on_its_row() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("box"),
        target: super::picker::TextInput::with_text("user@box"),
        ..Default::default()
    });
    state.cursor = 0;

    let out = d.render();
    assert!(out.contains("ssh user@box"), "{out}");
    assert!(!out.contains('\u{1f4cb}'), "off must show no marker: {out}");

    d.app.host_edit.as_mut().unwrap().rows[0].clipboard = true;
    let out = d.render();
    assert!(
        out.contains("ssh user@box \u{1f4cb}"),
        "the marker belongs beside the target: {out}"
    );
}

/// The clipboard is a **field**, not a panel key: it shows its own state in the
/// editor, `Space` flips it, and `Esc` puts it back like any other field.
///
/// The old `p` in the list was invisible until you read the footer and had no way
/// to say what it currently was. Being a field, `[off]` is on screen the moment
/// the editor opens — and it inherits the row snapshot, so abandoning an edit
/// abandons this too.
#[test]
fn the_clipboard_is_a_field_in_the_row_editor() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    // No target, so committing the row exercises the commit without standing up a
    // backend — the same trick `esc_abandons_a_hosts_row_edit_and_enter_keeps_it`
    // uses, and here it also keeps an ssh host out of the shared `hosts.json`.
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("box"),
        ..Default::default()
    });
    state.cursor = 0;
    let row = |d: &TestDashboard| d.app.host_edit.as_ref().unwrap().rows[0].clipboard;

    // The list has no key for it any more — `p` there must be inert.
    d.press(KeyCode::Char('p'));
    assert!(!row(&d), "`p` in the list must no longer toggle anything");

    // Opening the editor shows the field and its state, unasked.
    d.press(KeyCode::Char('e'));
    let out = d.render();
    assert!(
        out.contains("Clipboard"),
        "the field must be visible: {out}"
    );
    assert!(out.contains("[off]"), "and say what it currently is: {out}");

    // Walk to it and flip it. `Space` on a text field would type a space, which
    // is why the toggle is bound on this field alone.
    for _ in 0..4 {
        d.press(KeyCode::Tab);
    }
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().focus(),
        Some(super::HostField::Clipboard)
    );
    d.press(KeyCode::Char(' '));
    assert!(row(&d), "Space must toggle it on");
    let out = d.render();
    assert!(out.contains("[on]"), "{out}");
    assert!(
        out.contains("Space toggle"),
        "the hint names the key: {out}"
    );
    // The arrows have no cursor to move here, so they flip it too.
    d.press(KeyCode::Left);
    assert!(!row(&d), "← must toggle it back");
    d.press(KeyCode::Right);
    assert!(row(&d));

    // `Enter` commits rather than toggling — a key that means "save" on the other
    // four fields must not mean "change" on this one.
    d.press(KeyCode::Enter);
    assert!(row(&d), "Enter must not have flipped it");
    assert!(d.app.host_edit.as_ref().unwrap().edit.is_none());

    // And `Esc` abandons it with the rest of the row.
    d.press(KeyCode::Char('e'));
    for _ in 0..4 {
        d.press(KeyCode::Tab);
    }
    d.press(KeyCode::Char(' '));
    assert!(!row(&d));
    d.press(KeyCode::Esc);
    assert!(row(&d), "Esc must restore the pre-edit value");
}

/// The row editor is a **card over the list**, not a form pinned under it.
///
/// The form used to take eight of the popup's rows the moment you pressed `e`,
/// and this panel has no scrolling — so on a full list the hosts at the bottom
/// simply left the screen while you were editing one of them. As a card it costs
/// the list nothing, it insets inside the panel's frame so it reads as floating
/// over it, it dims what stopped listening, and it can say which of the two
/// things `Esc` will do.
#[test]
fn the_hosts_row_editor_draws_as_a_card_over_the_list() {
    use ratatui::style::Modifier;
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let state = d.app.host_edit.as_mut().unwrap();
    // Two lines each, so seven hosts fill the popup exactly — which is more than
    // the old layout had room for once the form took the bottom eight rows.
    for i in 1..=7 {
        state.rows.push(super::HostRow {
            label: super::picker::TextInput::with_text(format!("h{i}")),
            ..Default::default()
        });
    }
    state.cursor = 0;
    let out = d.render();
    assert!(out.contains("h7"), "the list starts full: {out}");

    d.press(KeyCode::Char('e'));
    let out = d.render();
    assert!(out.contains("Edit Host"), "the card names itself: {out}");
    assert!(
        out.contains("h7"),
        "and costs the list none of its rows: {out}"
    );
    let card = out
        .lines()
        .find(|l| l.contains("Edit Host"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        card.trim_start().starts_with('\u{2502}'),
        "the panel's own border survives beside the card: {card}"
    );

    // The list goes dim under it: every key belongs to the form while it is up,
    // including the `j`/`k`/`d` that move and delete rows out there.
    let buf = d.terminal.backend().buffer();
    let dim_at = |needle: &str| {
        let (x, y) = find_cell(buf, needle).unwrap_or_else(|| panic!("{needle} not drawn"));
        buf[(x, y)].style().add_modifier.contains(Modifier::DIM)
    };
    assert!(dim_at("h7"), "the list must read as no longer listening");
    assert!(!dim_at("Edit Host"), "the card itself must not");

    // The line the card holds for a per-field hint is held whether the focused
    // field has one or not — otherwise `Tab` shunts the fields up and down under
    // the cursor as it walks past the ones that do.
    let row_of = |out: &str, needle: &str| out.lines().position(|l| l.contains(needle));
    let quiet = d.render();
    assert!(
        !quiet.contains("Space toggle"),
        "Label has no hint: {quiet}"
    );
    for _ in 0..4 {
        d.press(KeyCode::Tab);
    }
    let hinted = d.render();
    assert!(hinted.contains("Space toggle"), "{hinted}");
    assert_eq!(
        row_of(&quiet, "Clipboard"),
        row_of(&hinted, "Clipboard"),
        "the fields must not move when a hint appears"
    );

    // A row the edit *created* says so, because there `Esc` drops it rather than
    // putting it back — the inline form had nowhere to tell you that.
    d.press(KeyCode::Esc);
    d.press(KeyCode::Char('a'));
    let out = d.render();
    assert!(out.contains("Add Host"), "{out}");
}

/// A value too long for the card **wraps**; the card grows to hold it.
///
/// Three `-L` forwards is an ordinary set and already outruns the card's width.
/// Truncating at the frame was worse here than in the panel's own read-only
/// rows: the hidden tail was still in the field, saved on `Enter` and editable by
/// a cursor nothing on screen could place — so the fix is more lines, not an `…`.
#[test]
fn the_hosts_editor_wraps_a_value_too_long_for_its_card() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let opts = "-L 8010:localhost:8010 -L 8089:localhost:8089 -L 7891:localhost:7891";
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("polaris"),
        target: super::picker::TextInput::with_text("polaris"),
        options: super::picker::TextInput::with_text(opts),
        ..Default::default()
    });
    state.cursor = 0;
    d.press(KeyCode::Char('e'));

    let out = d.render();
    let lines: Vec<&str> = out.lines().collect();
    let row_of = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not drawn: {out}"))
    };
    let opts_row = row_of("Options");
    let cont = lines[opts_row + 1];
    assert!(
        cont.contains("7891:localhost:7891"),
        "the tail wraps onto the next line instead of being cut off: {out}"
    );
    // Indented to the value column, so the second line reads as more of the same
    // field rather than as a nameless one of its own.
    assert_eq!(
        cont.find("7891:localhost:7891"),
        lines[opts_row].find("-L 8010"),
        "the continuation lines up under the value: {out}"
    );
    // And the fields below it move down rather than being written over: the card
    // is sized from the lines it actually draws.
    assert_eq!(row_of("Icon"), opts_row + 2, "{out}");
    assert!(out.contains("Clipboard"), "the card still closes: {out}");
}

/// Wrapping a field under a cursor is not the same problem as wrapping a log
/// line: the offsets have to survive it.
#[test]
fn wrapping_a_field_value_tiles_the_text_exactly() {
    use super::draw::wrap_ranges;
    use unicode_width::UnicodeWidthStr;

    let text = "-L 8010:localhost:8010 -L 8089:localhost:8089";
    let seg = |r: &std::ops::Range<usize>| text[r.start..r.end].to_string();
    let ranges = wrap_ranges(text, 25);
    // Nothing dropped and nothing collapsed — the cursor's byte offset has to
    // keep pointing at the character it did.
    assert_eq!(ranges.iter().map(seg).collect::<String>(), text);
    assert!(ranges.iter().all(|r| seg(r).width() <= 25), "{ranges:?}");
    // Broken past a space rather than mid-argument.
    assert_eq!(seg(&ranges[0]), "-L 8010:localhost:8010 ");

    // A token with no break in it is hard-broken rather than left to run off the
    // card — an ssh option can be one long word.
    let long = "8010:localhost:8010";
    assert_eq!(
        wrap_ranges(long, 8)
            .iter()
            .map(|r| &long[r.start..r.end])
            .collect::<Vec<_>>(),
        vec!["8010:loc", "alhost:8", "010"]
    );

    // A run of spaces is text the user typed and can delete, not layout.
    let spaced = "a  b";
    assert_eq!(wrap_ranges(spaced, 10).len(), 1);
    assert_eq!(
        wrap_ranges(spaced, 10)
            .iter()
            .map(|r| &spaced[r.start..r.end])
            .collect::<String>(),
        spaced
    );

    // An empty field still gets a line, so it still gets a row — and a cursor.
    assert_eq!(wrap_ranges("", 10), vec![0..0]);
}

/// The row editor's four fields walk by every idiom the dashboard binds
/// elsewhere, and — the point — they walk **backwards** too. `Tab`-only meant
/// overshooting Options cost three more presses.
#[test]
fn the_hosts_panel_walks_its_fields_in_both_directions() {
    use super::HostField::*;
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let focus = |d: &TestDashboard| d.app.host_edit.as_ref().unwrap().focus();

    d.press(KeyCode::Char('a'));
    assert_eq!(focus(&d), Some(Label));
    d.press(KeyCode::Tab);
    assert_eq!(focus(&d), Some(Target));
    d.press(KeyCode::Down);
    assert_eq!(focus(&d), Some(Options));
    d.press_ctrl(KeyCode::Char('n'));
    assert_eq!(focus(&d), Some(Icon));
    d.press(KeyCode::Tab);
    assert_eq!(focus(&d), Some(Clipboard));
    // The form is a ring, so the last field steps to the first.
    d.press(KeyCode::Tab);
    assert_eq!(focus(&d), Some(Label));

    d.press(KeyCode::BackTab);
    assert_eq!(focus(&d), Some(Clipboard));
    d.press(KeyCode::Up);
    assert_eq!(focus(&d), Some(Icon));
    d.press_ctrl(KeyCode::Char('p'));
    assert_eq!(focus(&d), Some(Options));

    // None of it typed anything: the field keys are intercepted ahead of the
    // text input, which would otherwise have eaten `n` and `p`.
    assert_eq!(d.app.host_edit.as_ref().unwrap().rows[0].label.text(), "");
}

/// `Esc` abandons a row edit where `Enter` keeps it. The two used to be the same
/// key, so there was no way to back out of a mistyped target — and the footer
/// said "back", which is what `Esc` means everywhere else in the app.
#[test]
fn esc_abandons_a_hosts_row_edit_and_enter_keeps_it() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let rows = |d: &TestDashboard| {
        d.app
            .host_edit
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.label.text().to_string())
            .collect::<Vec<_>>()
    };

    // A row with no target never reaches `hosts.json` (nothing to dial), so
    // committing it here exercises the commit without standing up a backend.
    d.press(KeyCode::Char('a'));
    for c in "box".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Enter);
    assert!(d.app.host_edit.as_ref().unwrap().edit.is_none());
    assert_eq!(rows(&d), ["box"]);

    // Editing it and pressing Esc puts back what was there before the edit…
    d.press(KeyCode::Char('e'));
    for c in "-typo".chars() {
        d.press(KeyCode::Char(c));
    }
    assert_eq!(rows(&d), ["box-typo"]);
    d.press(KeyCode::Esc);
    assert_eq!(rows(&d), ["box"]);
    // …and leaves the panel open: it cancelled the row, not the view.
    assert!(d.app.host_edit.as_ref().unwrap().edit.is_none());
    assert_eq!(d.app.input_mode, InputMode::HostEdit);

    // A row the edit *created* has nothing to restore, so cancelling removes it
    // again — an empty row was never a host on disk, and leaving it in the list
    // would be a lie about what is configured.
    d.press(KeyCode::Char('a'));
    for c in "half".chars() {
        d.press(KeyCode::Char(c));
    }
    assert_eq!(rows(&d), ["box", "half"]);
    d.press(KeyCode::Esc);
    assert_eq!(rows(&d), ["box"]);
    assert_eq!(d.app.host_edit.as_ref().unwrap().cursor, 1, "on '+ add'");

    // Esc from the list still closes the panel — the cancel is scoped to an
    // edit in progress, and there is none now.
    d.press(KeyCode::Esc);
    assert!(d.app.host_edit.is_none());
}

/// The fields are `TextInput`s, so a typo in the middle of a target is fixable
/// in place. They used to be push/pop `String`s: the only edit was at the end,
/// and the cursor drawn after the text was decorative.
#[test]
fn a_hosts_panel_field_edits_at_the_cursor() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    d.press(KeyCode::Char('a'));
    for c in "usr@box".chars() {
        d.press(KeyCode::Char(c));
    }
    // Back over "r@box" and repair the front of the string.
    for _ in 0..5 {
        d.press(KeyCode::Left);
    }
    d.press(KeyCode::Char('e'));
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().rows[0].label.text(),
        "user@box"
    );
    // Readline keys ride along with the arrows, and they are cursor-relative
    // too: ^u kills to the start *from where the cursor is*, not the whole line.
    d.press_ctrl(KeyCode::Char('u'));
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().rows[0].label.text(),
        "r@box"
    );
}

/// From the list, `^e`/`^t` open the editor *on* the field they name — `e`
/// always lands on Label, which put the emoji picker five keys away from a row
/// whose icon you wanted to change.
#[test]
fn ctrl_keys_open_the_hosts_editor_on_a_named_field() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("box"),
        target: super::picker::TextInput::with_text("user@box"),
        ..Default::default()
    });
    state.cursor = 0;

    d.press_ctrl(KeyCode::Char('t'));
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().focus(),
        Some(super::HostField::Target)
    );
    d.press(KeyCode::Esc);

    // `^e` goes one step further and opens the picker, since picking is what
    // the icon field is for. The panel stays alive underneath to receive it.
    d.press_ctrl(KeyCode::Char('e'));
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(matches!(
        d.app.picker.as_ref().unwrap().kind,
        super::PickerKind::HostEmoji
    ));
    d.app.apply_host_emoji_pick("\u{1F680}");
    assert_eq!(d.app.input_mode, InputMode::HostEdit);
    let state = d.app.host_edit.as_ref().unwrap();
    assert_eq!(state.focus(), Some(super::HostField::Icon));
    assert_eq!(state.rows[0].icon.text(), "\u{1F680}");

    // And it is an *edit*, so Esc undoes the pick like any other field change.
    d.press(KeyCode::Esc);
    let state = d.app.host_edit.as_ref().unwrap();
    assert_eq!(state.rows[0].icon.text(), "");
    assert!(state.edit.is_none());

    // Inside a *text* field the same key keeps its readline meaning: the picker
    // belongs to the Icon field, not to the form.
    d.press(KeyCode::Char('e'));
    d.press(KeyCode::Left);
    d.press_ctrl(KeyCode::Char('e'));
    d.press(KeyCode::Char('y'));
    assert_eq!(d.app.input_mode, InputMode::HostEdit);
    assert_eq!(
        d.app.host_edit.as_ref().unwrap().rows[0].label.text(),
        "boxy"
    );
}

/// A Ctrl-modified key never falls through to the list's plain-letter commands.
/// `^d` reaching `d` would put a removal confirm on screen for a keystroke the
/// user meant as "page down" or as nothing at all.
#[test]
fn a_ctrl_key_does_not_trigger_the_hosts_lists_plain_commands() {
    let mut d = TestDashboard::new(120, 30);
    d.app.open_host_edit();
    let state = d.app.host_edit.as_mut().unwrap();
    state.rows.push(super::HostRow {
        label: super::picker::TextInput::with_text("box"),
        target: super::picker::TextInput::with_text("user@box"),
        ..Default::default()
    });
    state.cursor = 0;

    d.press_ctrl(KeyCode::Char('d'));
    assert!(d.app.host_edit.as_ref().unwrap().pending_remove.is_none());
    d.press_ctrl(KeyCode::Char('a'));
    assert_eq!(d.app.host_edit.as_ref().unwrap().rows.len(), 1);
    d.press_ctrl(KeyCode::Char('l'));
    assert!(d.app.host_edit.as_ref().unwrap().log_view.is_none());

    // ^n/^p are the list's ↑↓ under the pickers' names, and are the exception.
    d.press_ctrl(KeyCode::Char('n'));
    assert_eq!(d.app.host_edit.as_ref().unwrap().cursor, 1, "on '+ add'");
    d.press_ctrl(KeyCode::Char('p'));
    assert_eq!(d.app.host_edit.as_ref().unwrap().cursor, 0);
}

#[test]
fn a_host_status_is_flattened_and_truncated_to_its_row() {
    use super::draw::one_line;
    // The case this exists for: a host quoting a multi-line refusal. A `\n`
    // inside a Span corrupts the row rather than wrapping, so it must not
    // survive; the `…` is what says the rest is elsewhere (`l`).
    let refusal = "could not deploy miao-server: Could not start dynamically linked executable:\n/home/u/.cache/captain-miao/bin/miao-server\nNixOS cannot run dynamically linked executables";
    let out = one_line(refusal, 40);
    assert!(!out.contains('\n'), "{out}");
    assert_eq!(out.chars().count(), 40, "{out}");
    assert!(out.ends_with('…'), "{out}");
    // Short text is left alone, newlines and all.
    assert_eq!(one_line("connected", 40), "connected");
    assert_eq!(one_line("a\nb", 40), "a b");
}

#[test]
fn the_host_tally_prints_only_the_non_empty_buckets() {
    let ui = crate::config::UiColors::default();
    let text = |good, error, down| {
        super::draw::host_tally_spans(
            &super::HostTally {
                good,
                error,
                down,
                connecting: 0,
            },
            &ui,
            true,
        )
        .iter()
        .map(|s| s.content.to_string())
        .collect::<String>()
    };
    // The cloud carries its emoji variation selector: the bare U+2601 is a
    // hairline text glyph that vanished against the header.
    // All healthy: one green number, nothing else — the quiet case stays quiet.
    // No separator before the first count: `unicode-width` measures the VS16
    // sequence as 2 cells and ratatui reserves both, so a terminal that paints
    // the glyph 1 cell wide leaves the second blank — an explicit space on top
    // of that read as a two-cell gulf between the cloud and its number.
    assert_eq!(text(3, 0, 0), "\u{2601}\u{fe0f}3");
    // A problem announces itself by a number appearing, not by a 0 changing.
    // The numbers still separate from *each other* — nothing else tells them
    // apart at this width.
    assert_eq!(text(2, 1, 0), "\u{2601}\u{fe0f}2 1");
    assert_eq!(text(0, 0, 2), "\u{2601}\u{fe0f}2");
    assert_eq!(text(1, 2, 3), "\u{2601}\u{fe0f}1 2 3");
}

/// A host that is still dialing is the one bucket with no number of its own: it
/// blinks the cloud, and forces the *connected* count on screen at zero so the
/// header can't be read as "one host, up" while the handshake is still running.
#[test]
fn a_dialing_host_blinks_the_cloud_and_holds_the_count_at_zero() {
    use super::draw::{connect_blink_lit, host_tally_spans};
    let ui = crate::config::UiColors::default();
    let text = |tally: &super::HostTally, lit| {
        host_tally_spans(tally, &ui, lit)
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    let dialing = super::HostTally {
        connecting: 1,
        ..Default::default()
    };
    assert_eq!(text(&dialing, true), "\u{2601}\u{fe0f}0");
    // The dark half swaps the cloud for exactly the width `unicode-width`
    // measures it as, so the right-aligned header cluster holds still.
    assert_eq!(text(&dialing, false), "  0");
    // The count is the truth as it stands, not a placeholder: a second host
    // already up shows as 1 while the first one dials.
    let mixed = super::HostTally {
        good: 1,
        connecting: 1,
        ..Default::default()
    };
    assert_eq!(text(&mixed, true), "\u{2601}\u{fe0f}1");
    // A dropped link still gets its own dim number — dialing is not "down".
    let dropped = super::HostTally {
        down: 1,
        connecting: 1,
        ..Default::default()
    };
    assert_eq!(text(&dropped, true), "\u{2601}\u{fe0f}0 1");
    // Settled hosts never blink: nothing dialing, so the phase is never dark.
    assert_eq!(
        text(
            &super::HostTally {
                good: 2,
                ..Default::default()
            },
            true
        ),
        "\u{2601}\u{fe0f}2"
    );

    // Lit for most of the cycle, and it does come back around — a "wait a
    // moment" hint, not a strobe.
    use std::time::Duration;
    assert!(connect_blink_lit(Duration::ZERO));
    assert!(connect_blink_lit(Duration::from_millis(899)));
    assert!(!connect_blink_lit(Duration::from_millis(900)));
    assert!(!connect_blink_lit(Duration::from_millis(1399)));
    assert!(connect_blink_lit(Duration::from_millis(1400)));
    // Nothing accumulates across the wrap: the same phase a period later.
    assert!(!connect_blink_lit(Duration::from_millis(900 + 1400)));
}

/// The hosts panel's utilisation spinner: one frame per step, wrapping, and off
/// the wall clock so nothing has to hold an animation cursor. Same shape as the
/// blink above, and pinned for the same reason — the run loop redraws on the
/// *change* of this number, so a phase that stuck or jumped would show up as a
/// frozen or stuttering spinner rather than as a failure anywhere.
#[test]
fn the_vitals_spinner_advances_one_frame_a_step_and_wraps() {
    use super::draw::{VITALS_SPINNER, VITALS_SPINNER_STEP, vitals_spinner_frame};
    use std::time::Duration;
    let step = VITALS_SPINNER_STEP;
    assert_eq!(vitals_spinner_frame(Duration::ZERO), 0);
    // Held for the whole step, then exactly one frame on.
    assert_eq!(vitals_spinner_frame(step - Duration::from_millis(1)), 0);
    assert_eq!(vitals_spinner_frame(step), 1);
    assert_eq!(vitals_spinner_frame(step * 2), 2);
    // And around: the clock never accumulates into an out-of-range index.
    let cycle = step * VITALS_SPINNER.len() as u32;
    assert_eq!(vitals_spinner_frame(cycle), 0);
    assert_eq!(vitals_spinner_frame(cycle * 1000 + step * 3), 3);
}

/// The table's trailing line while a host is still dialing. Its whole job is to
/// distinguish "no sessions there" from "not asked yet", so it names the host
/// when there is one to name and counts them when there isn't.
#[test]
fn the_loading_line_names_one_host_and_counts_several() {
    use super::draw::connecting_row_label;
    use crate::state::HostId;
    let host = |s: &str| HostId(s.into());
    assert_eq!(connecting_row_label(&[]), None);
    assert_eq!(
        connecting_row_label(&[host("box")]).as_deref(),
        Some("loading sessions from box…")
    );
    assert_eq!(
        connecting_row_label(&[host("box"), host("buildbox")]).as_deref(),
        Some("loading sessions from 2 remote hosts…")
    );
}

/// One host's connection says nothing about another's rows. A box that has
/// answered shows its sessions immediately, while the trailing line names only
/// the box still being waited on — the whole point of mirroring per host.
#[test]
fn a_connected_host_shows_its_sessions_while_another_still_loads() {
    use crate::backend::{Backend, ConnState, RemoteBackend};
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let quick = HostId("quick".into());
    let slow = HostId("slow".into());
    let mut there = session(1, "/srv/there", SessionStatus::Idle);
    there.host = quick.clone();
    d.set_sessions(vec![there.clone()]);
    d.app
        .backends
        .push(Backend::Remote(RemoteBackend::unconnected_for_tests(
            quick.clone(),
            vec![there],
        )));
    let dialing = RemoteBackend::unconnected_for_tests(slow.clone(), Vec::new());
    dialing.simulate_link_for_tests(ConnState::Connecting, false);
    d.app.backends.push(Backend::Remote(dialing));

    assert_eq!(d.app.connecting_hosts(), vec![slow]);
    let out = d.render();
    // The answered host's row is on screen…
    assert!(out.contains("session-1"), "{out}");
    // …under a line naming only the host still to answer.
    assert!(out.contains("loading sessions from slow…"), "{out}");
}

#[test]
fn leader_v_toggles_preview_visibility() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    // First render initializes panel defaults based on viewport.
    let out = d.render();
    assert!(out.contains("Terminal Preview"));
    assert!(d.app.preview_visible);

    // Space then v toggles off.
    d.press(KeyCode::Char(' '));
    assert!(d.app.pending_prefix.is_some());
    d.press(KeyCode::Char('v'));
    assert!(d.app.pending_prefix.is_none());
    assert!(!d.app.preview_visible);
    let out = d.render();
    assert!(!out.contains("Terminal Preview"));

    // And back on.
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('v'));
    assert!(d.app.preview_visible);
    let out = d.render();
    assert!(out.contains("Terminal Preview"));
}

#[test]
fn leader_d_toggles_detail_visibility() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let out = d.render();
    assert!(out.contains("Detail"));
    assert!(d.app.detail_visible);

    // Detail toggle lives on `Space d` (the icon editor took `Space i`).
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('d'));
    assert!(!d.app.detail_visible);
    let out = d.render();
    assert!(!out.contains("Detail"));
}

#[test]
fn manual_toggle_overrides_small_viewport() {
    // Even if the viewport would auto-hide the preview at startup,
    // manually toggling it on should still make it render.
    let mut d = TestDashboard::new(120, 12);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    let out = d.render();
    assert!(!out.contains("Terminal Preview"), "auto-hidden at startup");
    assert!(!d.app.preview_visible);

    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('v'));
    assert!(d.app.preview_visible);
    let out = d.render();
    assert!(
        out.contains("Terminal Preview"),
        "manual toggle wins over viewport"
    );
}

#[test]
fn leader_cancels_on_unknown_key() {
    let mut d = TestDashboard::new(120, 24);
    d.press(KeyCode::Char(' '));
    assert!(d.app.pending_prefix.is_some());
    // Unknown leader sequence: consume the key, clear pending, no action.
    // 'q' isn't a registered leader binding (top-level 'q' quits, but the
    // leader namespace ignores it).
    d.press(KeyCode::Char('q'));
    assert!(d.app.pending_prefix.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    // Falling through to top-level 'q' would quit; the leader handler
    // swallows the key, so the dashboard stays open.
    assert!(!d.app.should_quit);
}

#[test]
fn leader_then_unbound_key_does_not_fall_through_to_destructive_command() {
    // Safety: `Space` (a prefix) followed by an unbound key must be swallowed,
    // never reinterpreted as the single-key command (here `x` = kill).
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.press(KeyCode::Char(' '));
    assert!(d.app.pending_prefix.is_some());
    let action = d.press(KeyCode::Char('x'));
    assert!(action.is_none(), "Space x must not kill the session");
    assert!(d.app.pending_prefix.is_none());
}

#[test]
fn remapped_key_dispatches_and_frees_old_default() {
    use crate::config::KeyBinding;
    let mut cfg = std::collections::HashMap::new();
    cfg.insert("kill".to_string(), KeyBinding::One("X".to_string()));
    let (keymap, warnings) = super::keymap::Keymap::from_config(&cfg);
    assert!(warnings.is_empty(), "{warnings:?}");

    let mut d = TestDashboard::new(120, 15);
    d.app.keymap = keymap;
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);

    // The old default `x` no longer kills.
    assert!(d.press(KeyCode::Char('x')).is_none());
    // The remapped `X` does.
    match d.press(KeyCode::Char('X')) {
        Some(Action::KillSession { .. }) => {}
        other => panic!("expected KillSession from remapped key, got {other:?}"),
    }
}

#[test]
fn remapped_leader_completes_sequence() {
    use crate::config::KeyBinding;
    let mut cfg = std::collections::HashMap::new();
    // Move "restart selected" onto a custom `, r` prefix sequence.
    cfg.insert("restart".to_string(), KeyBinding::One(", r".to_string()));
    let (keymap, warnings) = super::keymap::Keymap::from_config(&cfg);
    assert!(warnings.is_empty(), "{warnings:?}");

    let mut d = TestDashboard::new(120, 15);
    d.app.keymap = keymap;
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);

    // `,` is now a prefix; `r` completes it. (No assertion on the restart side
    // effect — just that the sequence is recognized and consumed cleanly.)
    d.press(KeyCode::Char(','));
    assert!(d.app.pending_prefix.is_some());
    d.press(KeyCode::Char('r'));
    assert!(d.app.pending_prefix.is_none());
}

#[test]
fn space_z_toggles_prevent_sleep() {
    if !crate::sleep::supported() {
        return; // No backend on this system — see prevent_sleep_disabled_when_unsupported.
    }
    let mut d = TestDashboard::new(120, 10);
    // Default is enabled, and with no sessions there's no inhibitor running.
    assert!(d.app.prevent_sleep_enabled);
    assert!(!d.app.sleep_inhibitor.is_active());

    // Toggle off.
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('z'));
    assert!(d.app.pending_prefix.is_none());
    assert!(!d.app.prevent_sleep_enabled);
    assert!(!d.app.sleep_inhibitor.is_active());

    // Toggle back on.
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('z'));
    assert!(d.app.prevent_sleep_enabled);
}

#[test]
fn prevent_sleep_default_matches_support() {
    let d = TestDashboard::new(80, 10);
    assert_eq!(d.app.prevent_sleep_enabled, crate::sleep::supported());
}

/// Keep-awake is about *this* machine's power state. A remote session runs in
/// the far host's pty pool and neither notices nor cares whether the laptop
/// watching it sleeps, so caffeinating for one burns battery for nothing.
#[test]
fn a_busy_remote_session_does_not_keep_this_machine_awake() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 10);
    let mut remote = session(1, "/srv/proj", SessionStatus::Active);
    remote.host = HostId("box".into());
    remote.pool_session = Some("miao-box-1".into());
    d.set_sessions(vec![remote.clone()]);
    assert!(remote.status.is_busy(), "the row is busy, just not ours");
    assert!(
        !d.app.has_active_session(),
        "a remote session must not hold the inhibitor"
    );

    // The same status on this machine does keep it awake — the gate is locality,
    // not a narrower reading of "busy".
    d.set_sessions(vec![
        remote,
        session(2, "/home/test/here", SessionStatus::Active),
    ]);
    assert!(d.app.has_active_session());
}

#[test]
fn review_pending_does_not_inhibit_sleep() {
    // A session blocked on a human review (`ReviewPending`) is an *attention*
    // state, not a busy one: there's no point caffeinating through a multi-hour
    // `r3 watch` that's waiting on a person. The inhibitor gate
    // (`has_active_session` → `is_busy`) must ignore it, exactly as it ignores
    // plain Idle / WaitingForApproval. This pins the end-to-end link so adding
    // `ReviewPending` to `is_busy` (e.g. to re-group the row) can't silently
    // start keeping the machine awake.
    let mut d = TestDashboard::new(120, 10);
    d.set_sessions(vec![session(
        1,
        "/home/test/a",
        SessionStatus::ReviewPending,
    )]);
    assert!(
        !d.app.has_active_session(),
        "ReviewPending must not count as an active (busy) session"
    );

    if !crate::sleep::supported() {
        return; // Can't exercise the real inhibitor without a backend binary.
    }

    // With the toggle on, reconciling the inhibitor for a ReviewPending-only
    // set must leave caffeinate stopped.
    d.app.prevent_sleep_enabled = true;
    d.app.update_sleep_inhibitor();
    assert!(
        !d.app.sleep_inhibitor.is_active(),
        "ReviewPending session must not spin up the sleep inhibitor"
    );

    // Positive control: a genuinely busy (Active) session *does* inhibit sleep,
    // so the assertion above is testing the ReviewPending exclusion — not a
    // globally-disabled inhibitor.
    d.set_sessions(vec![session(2, "/home/test/b", SessionStatus::Active)]);
    d.app.update_sleep_inhibitor();
    assert!(
        d.app.sleep_inhibitor.is_active(),
        "an Active session should inhibit sleep (positive control)"
    );
    d.app.sleep_inhibitor.disable(); // reap the spawned caffeinate
}

#[test]
fn prevent_sleep_toggle_errors_when_unsupported() {
    if crate::sleep::supported() {
        return; // Only meaningful on systems missing the inhibitor binary.
    }
    let mut d = TestDashboard::new(80, 10);
    assert!(!d.app.prevent_sleep_enabled, "default off when unsupported");

    // Trying to enable should surface an error and leave the flag off.
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('z'));
    assert!(
        !d.app.prevent_sleep_enabled,
        "toggle blocked when no backend"
    );
    assert!(d.app.status_is_error, "an error message is shown");
    let msg = d.app.status_msg.as_deref().unwrap_or("");
    assert!(
        msg.contains("Cannot enable"),
        "error mentions failure: {msg}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn keep_awake_indicator_renders_in_header() {
    if !crate::sleep::supported() {
        return; // No binary on PATH — indicator is hidden, nothing to assert.
    }
    let mut d = TestDashboard::new(120, 10);
    // Enabled but no active session → inhibitor idle → no icon at all.
    let out = d.render();
    assert!(!out.contains('\u{2615}'), "no icon when idle: {out}");

    // A busy session with the feature on spins up caffeinate → bare "☕".
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    d.app.update_sleep_inhibitor();
    let out = d.render();
    assert!(out.contains('\u{2615}'), "coffee icon when active: {out}");

    // Toggle the feature off → inhibitor released → icon disappears again.
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('z'));
    let out = d.render();
    assert!(!out.contains('\u{2615}'), "no icon when disabled: {out}");

    d.app.sleep_inhibitor.disable(); // reap any spawned caffeinate
}

#[test]
fn ctrl_n_and_ctrl_p_navigate_rows() {
    let mut d = TestDashboard::new(120, 15);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Active),
        session(2, "/home/test/b", SessionStatus::Active),
        session(3, "/home/test/c", SessionStatus::Active),
    ]);
    assert_eq!(d.selected(), Some(0));

    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(d.selected(), Some(1));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(d.selected(), Some(2));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(d.selected(), Some(1));
}

#[test]
fn ctrl_c_quits_from_any_mode() {
    let mut d = TestDashboard::new(120, 10);
    d.app.input_mode = InputMode::Picker;
    assert!(!d.app.should_quit);
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(d.app.should_quit);
}

#[test]
fn help_overlay_opens_and_dismisses() {
    let mut d = TestDashboard::new(120, 30);
    d.press(KeyCode::Char('?'));
    assert_eq!(d.app.input_mode, InputMode::Help);
    let out = d.render();
    assert!(out.contains("Keybindings"));

    // Any key dismisses the help overlay.
    d.press(KeyCode::Char('x'));
    assert_eq!(d.app.input_mode, InputMode::Normal);
}

#[test]
fn ctrl_u_scrolls_preview() {
    let mut d = TestDashboard::new(120, 30);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Active)]);
    // Fake enough preview content to allow scrolling
    d.app.preview_text = Some(
        (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    d.app.preview_window_id = Some(WindowId::from(100));

    assert_eq!(d.app.preview_scroll, 0);

    // Ctrl-u scrolls up
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(d.app.preview_scroll, 8);

    // Ctrl-d scrolls back down
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(d.app.preview_scroll, 0);

    // Ctrl-d at 0 stays at 0
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(d.app.preview_scroll, 0);
}

#[test]
fn ansi_to_lines_basic() {
    use ratatui::style::{Color, Modifier};

    let lines = ansi_to_lines("\x1b[31mhello\x1b[0m world");
    assert_eq!(lines.len(), 1);
    let spans: Vec<_> = lines[0].spans.iter().collect();
    assert_eq!(spans[0].content, "hello");
    assert_eq!(spans[0].style.fg, Some(Color::Red));
    assert_eq!(spans[1].content, " world");
    assert_eq!(spans[1].style.fg, None);

    let lines = ansi_to_lines("\x1b[1;94mtitle\x1b[0m");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::LightBlue));
    assert!(
        lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn ansi_to_lines_handles_multiple_lines() {
    let lines = ansi_to_lines("a\nb\nc");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].spans[0].content, "a");
    assert_eq!(lines[1].spans[0].content, "b");
    assert_eq!(lines[2].spans[0].content, "c");
}

#[test]
fn ansi_to_lines_extended_colors() {
    use ratatui::style::Color;

    let lines = ansi_to_lines("\x1b[38;5;202mx\x1b[0m");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(202)));

    let lines = ansi_to_lines("\x1b[38;2;10;20;30mx\x1b[0m");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));

    let lines = ansi_to_lines("\x1b[48;5;7my\x1b[0m");
    assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(7)));

    // Modern ':' sub-parameter form — what `kitten @ get-text --ansi`
    // actually emits. Must produce the same color as the ';' form.
    let lines = ansi_to_lines("\x1b[38:2:215:119:87mz\x1b[0m");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(215, 119, 87)));

    let lines = ansi_to_lines("\x1b[48:5:202mw\x1b[0m");
    assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(202)));
}

#[test]
fn ansi_to_lines_strips_osc_and_charset() {
    let lines = ansi_to_lines("\x1b]0;title\x07hello");
    assert_eq!(lines[0].spans[0].content, "hello");

    // Charset designator: ESC ( B selects ASCII; 'B' must not leak as text.
    let lines = ansi_to_lines("\x1b(Bhello");
    assert_eq!(lines[0].spans[0].content, "hello");
}

// -- Readline keybinds in pickers --

#[test]
fn picker_readline_ctrl_a_and_e() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    // Start with "~/a", cursor at end.
    let cursor_end = d.app.picker.as_ref().unwrap().picker.input.cursor();
    assert_eq!(cursor_end, picker_input_text(&d.app).len());

    // Ctrl-A → beginning of line.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 0);

    // Ctrl-E → end of line.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(
        d.app.picker.as_ref().unwrap().picker.input.cursor(),
        cursor_end
    );
}

#[test]
fn picker_readline_ctrl_w_deletes_prev_word() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "/foo/bar".chars() {
        d.press(KeyCode::Char(c));
    }
    // Ctrl-W deletes "bar".
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(picker_input_text(&d.app), "/foo/");
}

#[test]
fn picker_readline_ctrl_b_and_f_move_cursor() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "abc".chars() {
        d.press(KeyCode::Char(c));
    }
    // Cursor at end = 3. Ctrl-B twice → cursor 1.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 1);
    // Insert 'X' in the middle.
    d.press(KeyCode::Char('X'));
    assert_eq!(picker_input_text(&d.app), "aXbc");
    // Ctrl-F → cursor moves right past 'b'.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 3);
}

#[test]
fn picker_readline_ctrl_k_and_u_kill_lines() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "hello-world".chars() {
        d.press(KeyCode::Char(c));
    }
    // Move to position 5 (after "hello").
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    for _ in 0..5 {
        d.app
            .handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    }
    // Ctrl-K kills to end → "hello".
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(picker_input_text(&d.app), "hello");
    // Ctrl-U kills to start → "".
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(picker_input_text(&d.app), "");
}

#[test]
fn workdir_picker_ctrl_d_deletes_highlighted_recent() {
    let mut d = TestDashboard::new(120, 15);
    d.app.recent_cwds = vec![
        "/home/test/alpha".to_string(),
        "/home/test/beta".to_string(),
        "/tmp/work".to_string(),
    ];
    d.press(KeyCode::Char('O'));
    // Drop the picker's pre-seeded text so the full recent list shows.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    // Highlight the middle entry, then Ctrl-D drops it from the list and the picker.
    d.press(KeyCode::Down);
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(
        d.app.recent_cwds,
        vec!["/home/test/alpha".to_string(), "/tmp/work".to_string()]
    );
    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.items.len(), 2);
    assert_eq!(picker.items[0].payload.as_deref(), Some("/home/test/alpha"));
    assert_eq!(picker.items[1].payload.as_deref(), Some("/tmp/work"));
    // Cursor stays at index 1 (now pointing at "/tmp/work").
    assert_eq!(picker.cursor, 1);
}

/// `Ctrl-d` follows the host the picker is targeting, not the machine the
/// dashboard runs on. The list belongs to whoever launched into those dirs, so
/// a remote (or pooled-localhost) host is told to forget the entry and the
/// cached copy the picker draws from is edited to match — the row goes on the
/// keystroke, never on the round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workdir_picker_ctrl_d_forgets_on_the_targeted_host() {
    use crate::backend::{Backend, ConnState, RemoteBackend};
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 15);
    let host = HostId("box".into());
    let remote = RemoteBackend::unconnected_for_tests(host.clone(), Vec::new());
    // Connected, so the delete has somewhere to land. The backend has no live
    // connection task, so the request itself is dropped on the floor — what is
    // under test is that the dashboard *sends* it and stops drawing the row.
    remote.simulate_link_for_tests(ConnState::Connected, true);
    d.app.backends.push(Backend::Remote(remote));
    d.app.default_host = host.clone();
    // Seed the per-host cache the picker renders from, standing in for a
    // `ListRecentDirs` that has already come back.
    d.app.recent_dirs_cache.insert(
        host.clone(),
        vec!["~/alpha".to_string(), "~/beta".to_string()],
    );
    // The local list is untouched throughout: it is a different host's.
    d.app.recent_cwds = vec!["/home/test/alpha".to_string()];

    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));

    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.items.len(), 1);
    assert_eq!(picker.items[0].payload.as_deref(), Some("~/beta"));
    // And the cache agrees, so reopening the picker doesn't resurrect it.
    assert_eq!(d.app.recent_dirs_cache[&host], vec!["~/beta".to_string()]);
    assert_eq!(d.app.recent_cwds, vec!["/home/test/alpha".to_string()]);
}

/// A host we can't reach can't be told, and dropping the row anyway would be a
/// promise the next re-seed takes straight back. So the row stays.
#[test]
fn workdir_picker_ctrl_d_holds_when_the_host_is_unreachable() {
    use crate::backend::{Backend, RemoteBackend};
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 15);
    let host = HostId("box".into());
    d.app
        .backends
        .push(Backend::Remote(RemoteBackend::unconnected_for_tests(
            host.clone(),
            Vec::new(),
        )));
    d.app.default_host = host.clone();
    d.app.recent_dirs_cache.insert(
        host.clone(),
        vec!["~/alpha".to_string(), "~/beta".to_string()],
    );

    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));

    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.items.len(), 2);
    assert_eq!(
        d.app.recent_dirs_cache[&host],
        vec!["~/alpha".to_string(), "~/beta".to_string()]
    );
}

#[test]
fn workdir_picker_ctrl_d_with_empty_list_is_noop() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "abc".chars() {
        d.press(KeyCode::Char(c));
    }
    // No recents → Ctrl-D shouldn't crash, shouldn't touch text input.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(picker_input_text(&d.app), "abc");
}

#[test]
fn picker_readline_alt_b_and_f_word_motion() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "foo/bar/baz".chars() {
        d.press(KeyCode::Char(c));
    }
    // Alt-B jumps to start of last word ("baz"), index 8.
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 8);
    // Alt-B again → start of "bar" (index 4).
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 4);
    // Alt-F jumps forward over "bar" (ends at 7, before '/').
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 7);
}

// -- Restart feature --

/// `Space e` on an idle session opens a confirmation dialog without firing
/// the action. `y` then yields a RestartSession action carrying the session
/// metadata; the dashboard returns to Normal mode.
#[test]
fn space_e_on_idle_session_confirms_then_restarts() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Idle)]);

    // Space e — should open confirm dialog, no action yet.
    d.press(KeyCode::Char(' '));
    let action = d.press(KeyCode::Char('e'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Confirm);
    assert!(d.app.pending_confirm.is_some());

    // y confirms — returns RestartSession action and exits Confirm mode.
    let action = d.press(KeyCode::Char('y'));
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
    match action {
        Some(Action::RestartSession(spec)) => {
            assert_eq!(spec.window_id, Some(WindowId::from(100)));
            assert_eq!(spec.cwd, "/home/test/proj");
            assert_eq!(spec.session_id, "sess-1");
        }
        _ => panic!("expected RestartSession action, got {:?}", action),
    }
}

#[test]
fn space_e_on_active_session_is_rejected() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Active)]);

    d.press(KeyCode::Char(' '));
    let action = d.press(KeyCode::Char('e'));
    assert!(action.is_none());
    assert_eq!(
        d.app.input_mode,
        InputMode::Normal,
        "should not enter Confirm when session is busy"
    );
    assert!(d.app.pending_confirm.is_none());
    assert!(d.app.status_is_error);
    let msg = d.app.status_msg.as_deref().unwrap_or("");
    assert!(
        msg.contains("idle"),
        "status should mention idle requirement: {msg}"
    );
}

/// Approval/Decision states count as "busy" too — the user is being asked
/// something and a restart would silently throw that prompt away.
#[test]
fn space_e_on_waiting_session_is_rejected() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(
        1,
        "/home/test/proj",
        SessionStatus::WaitingForApproval,
    )]);

    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('e'));
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
    assert!(d.app.status_is_error);
}

#[test]
fn confirm_dialog_cancels_on_n() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Idle)]);
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('e'));
    assert_eq!(d.app.input_mode, InputMode::Confirm);

    let action = d.press(KeyCode::Char('n'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
}

#[test]
fn confirm_dialog_cancels_on_esc() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Idle)]);
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('e'));
    let action = d.press(KeyCode::Esc);
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
}

#[test]
fn confirm_dialog_renders_prompt() {
    let mut d = TestDashboard::new(120, 24);
    d.set_sessions(vec![session(1, "/home/test/proj", SessionStatus::Idle)]);
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('e'));
    let out = d.render();
    assert!(out.contains("Confirm"), "should show confirm dialog title");
    assert!(
        out.contains("Restart session"),
        "should show restart prompt"
    );
    assert!(out.contains("y/Y/Enter"), "should show confirm hint");
}

/// `Space E` with all idle sessions queues a RestartAll action whose specs
/// cover every session.
#[test]
fn space_shift_e_restarts_all_when_all_idle() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Idle),
        session(3, "/home/test/c", SessionStatus::Idle),
    ]);
    d.press(KeyCode::Char(' '));
    let action = d.press(KeyCode::Char('E'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Confirm);

    let action = d.press(KeyCode::Enter);
    match action {
        Some(Action::RestartAll { sessions }) => {
            assert_eq!(sessions.len(), 3);
            let mut wids: Vec<WindowId> = sessions
                .iter()
                .filter_map(|s| s.window_id.clone())
                .collect();
            wids.sort();
            assert_eq!(
                wids,
                vec![
                    WindowId::from(100),
                    WindowId::from(200),
                    WindowId::from(300)
                ]
            );
        }
        _ => panic!("expected RestartAll, got {:?}", action),
    }
}

#[test]
fn space_shift_e_rejects_when_any_session_busy() {
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::Active),
    ]);
    d.press(KeyCode::Char(' '));
    let action = d.press(KeyCode::Char('E'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
    assert!(d.app.status_is_error);
}

/// A parked server (`Server`) and a session blocked on a human review
/// (`Review`) restart like an idle one, singly and in bulk.
///
/// Both are at rest — the agent's turn ended and what's left running is a dev
/// server / watcher it walked away from, or an r3 review-watch waiting on a
/// human. There is no in-flight work to lose, so the gate
/// (`SessionStatus::is_restartable`, shared with `upgrade_blocker`) lets them
/// through; refusing only stranded the row until the user hunted the shell down.
#[test]
fn space_e_and_space_shift_e_treat_server_and_review_as_idle() {
    for status in [
        SessionStatus::BackgroundServer,
        SessionStatus::ReviewPending,
    ] {
        let mut d = TestDashboard::new(120, 20);
        d.set_sessions(vec![session(1, "/home/test/proj", status.clone())]);
        d.press(KeyCode::Char(' '));
        d.press(KeyCode::Char('e'));
        assert_eq!(
            d.app.input_mode,
            InputMode::Confirm,
            "{status:?} should confirm, not refuse"
        );
        assert!(matches!(
            d.press(KeyCode::Char('y')),
            Some(Action::RestartSession(_))
        ));
    }

    // …and neither holds up a restart-all, which refuses on *any* non-resting
    // row.
    let mut d = TestDashboard::new(120, 20);
    d.set_sessions(vec![
        session(1, "/home/test/a", SessionStatus::Idle),
        session(2, "/home/test/b", SessionStatus::BackgroundServer),
        session(3, "/home/test/c", SessionStatus::ReviewPending),
    ]);
    d.press(KeyCode::Char(' '));
    d.press(KeyCode::Char('E'));
    assert_eq!(d.app.input_mode, InputMode::Confirm);
    match d.press(KeyCode::Enter) {
        Some(Action::RestartAll { sessions }) => assert_eq!(sessions.len(), 3),
        other => panic!("expected RestartAll over all three, got {other:?}"),
    }
}

#[test]
fn space_shift_e_with_no_sessions_is_a_noop() {
    let mut d = TestDashboard::new(120, 20);
    d.press(KeyCode::Char(' '));
    let action = d.press(KeyCode::Char('E'));
    assert!(action.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
    assert!(d.app.pending_confirm.is_none());
}

#[test]
fn default_dir_emoji_and_color_is_deterministic() {
    // The whole point of FNV-1a over DefaultHasher: a given path must map to
    // the same (emoji, color) every call, every process, every machine.
    // Otherwise the dir's "look" flips on dashboard restart.
    let p = "/Users/alice/projects/web";
    let first = default_dir_emoji_and_color(p);
    for _ in 0..3 {
        assert_eq!(default_dir_emoji_and_color(p), first);
    }
    // Trailing slashes are normalised — same key.
    assert_eq!(
        default_dir_emoji_and_color(p),
        default_dir_emoji_and_color(&format!("{p}/"))
    );
    // Different paths should reach into the icon and color sets independently
    // (i.e. we read both halves of the hash, not the same low bits twice).
    let a = default_dir_emoji_and_color("/a");
    let b = default_dir_emoji_and_color("/b");
    assert_ne!(
        a, b,
        "distinct paths shouldn't collide on this small sample"
    );
}

#[test]
fn picker_arrow_keys_move_cursor() {
    let mut d = TestDashboard::new(120, 15);
    d.press(KeyCode::Char('O'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for c in "abc".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Left);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 2);
    d.press(KeyCode::Home);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 0);
    d.press(KeyCode::End);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 3);
    d.press(KeyCode::Left);
    d.press(KeyCode::Right);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.input.cursor(), 3);
}

// -- Restart flag restoration --

#[test]
fn restart_restores_flags_to_new_pid_by_window() {
    let mut d = TestDashboard::new(120, 18);
    // The relaunched session reappears under a brand-new launcher pid but
    // reuses the window id `restart_one` captured at launch time.
    let s = session(7, "/home/test/proj", SessionStatus::Idle);
    let new_wid = s.window_id.clone().unwrap();
    d.set_sessions(vec![s]);
    // The session that died was waiting on a follow-up.
    d.app.pending_flag_restores.insert(
        new_wid,
        super::SessionFlags {
            follow_up: true,
            ..Default::default()
        },
    );

    assert!(d.app.apply_pending_flag_restores());
    let f = d.app.flags_of(&(crate::state::HostId::local(), 7));
    assert!(f.follow_up);
    assert!(!f.pinned);
    // Consumed: a later reload must not re-apply it.
    assert!(d.app.pending_flag_restores.is_empty());
    assert!(!d.app.apply_pending_flag_restores());
}

#[test]
fn restart_restores_pin_with_fresh_seq_above_existing() {
    let mut d = TestDashboard::new(120, 18);
    let live = session(1, "/home/test/a", SessionStatus::Idle);
    let restarted = session(2, "/home/test/b", SessionStatus::Idle);
    let restarted_wid = restarted.window_id.clone().unwrap();
    d.set_sessions(vec![live, restarted]);
    // A live pinned session already holds the lowest seq.
    d.app
        .update_flags((crate::state::HostId::local(), 1), Cursor::HoldIndex, |f| {
            f.pinned = true;
            f.pin_seq = 1;
        });
    d.app.next_pin_seq = 1;
    // Restore a pin onto pid 2 carrying a stale seq from the previous run.
    d.app.pending_flag_restores.insert(
        restarted_wid,
        super::SessionFlags {
            pinned: true,
            pin_seq: 999,
            ..Default::default()
        },
    );

    assert!(d.app.apply_pending_flag_restores());
    let restored = d.app.flags_of(&(crate::state::HostId::local(), 2));
    assert!(restored.pinned);
    // The stale 999 is discarded; a fresh seq is issued above the live pin so
    // the restored session sorts to the top and the counter stays consistent.
    assert_eq!(restored.pin_seq, 2);
    assert!(restored.pin_seq > d.app.flags_of(&(crate::state::HostId::local(), 1)).pin_seq);
    assert_eq!(d.app.next_pin_seq, 2);
}

#[test]
fn restart_restore_waits_until_session_reappears() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    // The replacement window hasn't shown up yet (no session with this id).
    d.app.pending_flag_restores.insert(
        WindowId::from(9999),
        super::SessionFlags {
            follow_up: true,
            ..Default::default()
        },
    );
    assert!(!d.app.apply_pending_flag_restores());
    // Nothing applied, entry retained for a later reload.
    assert!(
        !d.app
            .flags_of(&(crate::state::HostId::local(), 1))
            .follow_up
    );
    assert_eq!(d.app.pending_flag_restores.len(), 1);
}

#[test]
fn snapshot_entry_flags_roundtrip_and_back_compat() {
    use super::{SessionFlags, SessionSnapshotEntry};
    let entry = SessionSnapshotEntry {
        agent: crate::agent::AgentControl::Claude,
        launcher_pid: 1,
        child_pid: 2,
        window_id: WindowId::from(100),
        cwd: "/home/test/proj".to_string(),
        session_id: "sess-1".to_string(),
        flags: SessionFlags {
            pinned: true,
            pin_seq: 3,
            follow_up: false,
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    let back: SessionSnapshotEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.flags, entry.flags);

    // A snapshot written before the flags field existed must still parse, with
    // all-default (no) flags. window_id is here an integer (the pre-abstraction
    // format, before WindowId became a string newtype) — the tolerant
    // deserializer must still accept it, coerced to its decimal string.
    let legacy = r#"{"launcher_pid":1,"child_pid":2,"window_id":100,
        "cwd":"/x","session_id":"s"}"#;
    let parsed: SessionSnapshotEntry = serde_json::from_str(legacy).unwrap();
    assert!(parsed.flags.is_default());
    assert_eq!(parsed.window_id, WindowId::from(100));
}

#[test]
fn override_glyphs_measure_what_they_paint() {
    // The override indicator's tight two-slot layout rests on every glyph
    // measuring exactly what the terminal paints — the invariant is that
    // agreement, not "emoji only". The secondaries are emoji-presentation, so
    // `unicode-width` says 2 and ratatui parks the glyph in one buffer cell,
    // blanks the cell behind it, and its diff then skips that cell — which is
    // what keeps a neighbouring column's update from clipping the glyph's right
    // half. The follow-up dot is the other legal class: East-Asian-Width `N`, so
    // it measures 1 and paints 1 with no cell behind it to blank. What is
    // *illegal* is a glyph that measures 1 and paints 2 — the Nerd Font PUA
    // glyphs this column used to carry did, and needed a post-render buffer
    // fix-up to fake the agreement. An East-Asian-Width *Ambiguous* dot (`●`,
    // `•`) is the same bug on a terminal configured ambiguous-as-wide.
    use unicode_width::UnicodeWidthStr;
    let pin = "\u{1F4CC}";
    let dot = "\u{2022}";
    // Every secondary, not just the one rendered below: the slot is sized for
    // the class, so a 1-cell replacement would pass the render assertions (the
    // pad simply absorbs it) while breaking measure==paint.
    for (name, glyph) in [
        ("pin", pin),
        ("out-of-sight", "\u{1F648}"),
        ("held-elsewhere", "\u{1F440}"),
    ] {
        assert_eq!(glyph.width(), 2, "{name} secondary must measure two cells");
    }
    assert_eq!(dot.width(), 1, "follow-up dot must measure one cell");
    // ...and `•` is East-Asian-Width Ambiguous, so on a terminal configured
    // ambiguous-as-wide it paints two. That is a chosen tradeoff, documented on
    // `override_indicator_spans`: such a terminal slides a flagged row's status
    // label one column right. Asserted rather than dropped so the exception stays
    // on the record — a swap to a *different* ambiguous glyph trips nothing, but
    // one to an unambiguous glyph (`◉` U+25C9) fails here and sends whoever did it
    // to the note saying that is the fix, not the regression.
    assert_eq!(
        dot.width_cjk(),
        2,
        "known: the follow-up dot is ambiguous-width — see the layout note on \
         `override_indicator_spans` before changing this"
    );
    // The indent is sized for the worst case: both slots occupied at once.
    assert_eq!(
        dot.width() + pin.width(),
        super::format::OVERRIDE_COL_WIDTH as usize,
        "the indent must be exactly wide enough for both slots — narrower \
         silently clips the dot in the cell before the status label"
    );

    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.app
        .update_flags((crate::state::HostId::local(), 1), Cursor::HoldIndex, |f| {
            f.pinned = true;
            f.follow_up = true;
        });
    d.render();

    let buf = d.terminal.backend().buffer();
    let cells_of = |needle: &str| -> Vec<(u16, u16)> {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].symbol().contains(needle))
            .collect()
    };
    let pin_hits = cells_of(pin);
    assert_eq!(
        pin_hits.len(),
        1,
        "expected exactly one pin glyph cell, got {pin_hits:?}"
    );
    let (x, y) = pin_hits[0];
    assert_eq!(
        buf[(x, y)].symbol(),
        pin,
        "pin glyph must sit in its cell bare — no padding grapheme appended"
    );
    assert_eq!(
        buf[(x + 1, y)].symbol(),
        " ",
        "the cell the glyph paints over must stay blank"
    );

    // Both slots drew, packed tight and in order: the dot sits in the cell
    // *after* the secondary's two — the one that is blank when nothing is
    // flagged, so the status label's neighbour never moves. A clipped dot, or a
    // dot back on the far side of the emoji, is what this pins.
    let dot_hits = cells_of(dot);
    assert_eq!(
        dot_hits.len(),
        1,
        "expected exactly one follow-up dot cell, got {dot_hits:?}"
    );
    assert_eq!(
        dot_hits[0],
        (x + 2, y),
        "the dot must follow the secondary's slot with no separator"
    );
}

#[test]
fn the_override_indent_is_the_width_the_status_column_reserves() {
    // The indicator has no column of its own — it is the leading spans of the
    // status cell, and `status_width` adds `OVERRIDE_COL_WIDTH` on the promise
    // that those spans come out to exactly that. Nothing in the type system
    // holds the two together, so an added glyph combination that overruns would
    // simply push the status label right on that one row (or clip the last
    // column). Every combination, including the empty one, must measure the
    // same: the label starts at a fixed offset down the whole table.
    use crate::app::format::{Detached, override_indicator_spans};
    use unicode_width::UnicodeWidthStr;

    let secondaries = [None, Some(Detached::Free), Some(Detached::HeldElsewhere)];
    for follow_up in [false, true] {
        for pinned in [false, true] {
            for detached in secondaries {
                let w: usize = override_indicator_spans(follow_up, pinned, detached)
                    .iter()
                    .map(|s| s.content.width())
                    .sum();
                assert_eq!(
                    w,
                    super::format::OVERRIDE_COL_WIDTH as usize,
                    "follow_up={follow_up} pinned={pinned} detached={detached:?} \
                     measured {w} cells"
                );
            }
        }
    }
}

#[test]
fn base64_encode_matches_known_vectors() {
    // RFC 4648 §10 test vectors plus the padding boundaries.
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    // A realistic session id (uuid) round-trips to the expected encoding.
    assert_eq!(
        base64_encode(b"1b9d6bcd-bbfd-4b2d-9b5d-ab8dfbbd4bed"),
        "MWI5ZDZiY2QtYmJmZC00YjJkLTliNWQtYWI4ZGZiYmQ0YmVk",
    );
}

#[test]
fn y_copies_selected_session_id() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(7, "/home/test/a", SessionStatus::Idle)]);
    d.app.table_state.select(Some(0));
    match d.press(KeyCode::Char('y')) {
        Some(Action::CopySessionId(sid)) => assert_eq!(sid, "sess-7"),
        other => panic!("expected CopySessionId, got {other:?}"),
    }
}

#[test]
fn y_without_session_id_reports_instead_of_copying() {
    let mut d = TestDashboard::new(120, 18);
    let mut s = session(7, "/home/test/a", SessionStatus::Idle);
    s.session_id = None;
    s.child_pid = None;
    d.set_sessions(vec![s]);
    d.app.table_state.select(Some(0));
    assert!(
        d.press(KeyCode::Char('y')).is_none(),
        "no id → no copy action"
    );
    assert!(d.app.status_is_error, "should surface an error status");
}

#[test]
fn w_dispatches_work_tab_action_and_map_validates_against_snapshot() {
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(7, "/home/test/a", SessionStatus::Idle)]);
    d.app.table_state.select(Some(0));

    // `w` produces the work-tab action for the selected session's (host, cwd).
    match d.press(KeyCode::Char('w')) {
        Some(Action::OpenShellTab { host, cwd }) => {
            assert!(host.is_local());
            assert_eq!(cwd, "/home/test/a");
        }
        other => panic!("expected OpenShellTab, got {other:?}"),
    }

    // The map returns a recorded tab only while it exists in the snapshot,
    // and prunes the entry once the tab is gone.
    let key = (crate::state::HostId::local(), "/home/test/a".to_string());
    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(3),
            window_id: Some(WindowId::from(9)),
        },
    );
    let live = vec![crate::terminal::Tab {
        id: TabId::from(3),
        title: "a".into(),
        is_focused: false,
        windows: vec![WindowId::from(9)],
    }];
    assert_eq!(d.app.live_work_tab(&key, &live), Some(TabId::from(3)));

    // zellij recycles a closed highest tab's id, so an id match with the wrong
    // title (an unrelated tab inherited the number) must not count as live.
    let recycled = vec![crate::terminal::Tab {
        id: TabId::from(3),
        title: "something-else".into(),
        is_focused: false,
        windows: vec![WindowId::from(9)],
    }];
    assert_eq!(
        d.app.live_work_tab(&key, &recycled),
        None,
        "recycled id with a different title → no reuse"
    );

    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(3),
            window_id: Some(WindowId::from(9)),
        },
    );
    assert_eq!(
        d.app.live_work_tab(&key, &[]),
        None,
        "closed tab → no reuse"
    );
    assert!(
        !d.app.work_tabs.contains_key(&key),
        "stale entry should be pruned"
    );
}

#[test]
fn live_work_tab_validates_recorded_window_in_tab() {
    let mut d = TestDashboard::new(120, 18);
    let key = (crate::state::HostId::local(), "/home/test/a".to_string());

    // A recorded window that's still inside the (id, title)-matching tab proves
    // it's the same tab captain-miao spawned — accepted (zellij pane ids never
    // recycle, so this can't be an impostor).
    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(5),
            window_id: Some(WindowId::from(42)),
        },
    );
    let live = vec![crate::terminal::Tab {
        id: TabId::from(5),
        title: "a".into(),
        is_focused: false,
        windows: vec![WindowId::from(7), WindowId::from(42)],
    }];
    assert_eq!(d.app.live_work_tab(&key, &live), Some(TabId::from(5)));

    // Same id + title but the recorded window is gone → a recycled tab id wearing
    // the same basename (an impostor). Reject and prune.
    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(5),
            window_id: Some(WindowId::from(42)),
        },
    );
    let impostor = vec![crate::terminal::Tab {
        id: TabId::from(5),
        title: "a".into(),
        is_focused: false,
        windows: vec![WindowId::from(7)],
    }];
    assert_eq!(
        d.app.live_work_tab(&key, &impostor),
        None,
        "recycled id, matching title, but recorded window absent → no reuse"
    );
    assert!(
        !d.app.work_tabs.contains_key(&key),
        "impostor entry should be pruned"
    );

    // A legacy entry (no window id, from a pre-window-id work-tabs.json) falls
    // back to (id, title) validation and is accepted without a window check.
    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(5),
            window_id: None,
        },
    );
    let no_matching_window = vec![crate::terminal::Tab {
        id: TabId::from(5),
        title: "a".into(),
        is_focused: false,
        windows: vec![WindowId::from(7)],
    }];
    assert_eq!(
        d.app.live_work_tab(&key, &no_matching_window),
        Some(TabId::from(5)),
        "legacy entry (window_id None) → id + title acceptance"
    );
}

#[test]
fn work_tab_title_names_the_host_for_a_remote_tab() {
    let mut d = TestDashboard::new(120, 18);
    let box_host = crate::state::HostId("box".into());
    d.app.host_icons.insert(box_host.clone(), "🚀".into());
    // This machine's work tabs stay bare — the host adds nothing.
    assert_eq!(
        d.app
            .work_tab_title(&crate::state::HostId::local(), "/home/test/proj"),
        "proj"
    );
    // A remote tab is an ssh session whose own `[hostname]` title can only ever
    // reach the *window* title, never the tab label (the spawn sets an explicit
    // tab title, which overrides the follow-the-window default on both
    // backends). So the host rides in the title we stamp — as its icon, which
    // costs the basename two cells instead of the label's six, bracketed so the
    // glyph doesn't run into the basename.
    assert_eq!(d.app.work_tab_title(&box_host, "~/proj"), "[🚀] proj");
    // An unconfigured host still gets a glyph, so the prefix is never empty.
    let unconfigured = d
        .app
        .work_tab_title(&crate::state::HostId("other".into()), "~/proj");
    assert!(
        unconfigured.ends_with("] proj") && !unconfigured.starts_with("[]"),
        "a host with no configured icon falls back to a deterministic one: {unconfigured}"
    );

    // …and the spawn and the validation must agree on it, or every remote `w`
    // prunes its own entry and spawns a duplicate tab. `live_work_tab` derives
    // the expected title from the same key, so a tab wearing the prefixed title
    // validates and a bare-basename one (a pre-prefix `work-tabs.json` entry)
    // does not.
    let key = (box_host, "~/proj".to_string());
    let entry = super::WorkTab {
        tab_id: TabId::from(4),
        window_id: Some(WindowId::from(8)),
    };
    d.app.work_tabs.insert(key.clone(), entry.clone());
    let live = vec![crate::terminal::Tab {
        id: TabId::from(4),
        title: "[🚀] proj".into(),
        is_focused: false,
        windows: vec![WindowId::from(8)],
    }];
    assert_eq!(d.app.live_work_tab(&key, &live), Some(TabId::from(4)));

    d.app.work_tabs.insert(key.clone(), entry.clone());
    let legacy = vec![crate::terminal::Tab {
        id: TabId::from(4),
        title: "proj".into(),
        is_focused: false,
        windows: vec![WindowId::from(8)],
    }];
    assert_eq!(
        d.app.live_work_tab(&key, &legacy),
        None,
        "a pre-prefix entry fails the title check and self-heals into a fresh spawn"
    );

    // Re-iconing the host takes the same self-healing path: the open tab no
    // longer wears the title the key now derives, so it is pruned rather than
    // reused under a stale glyph.
    d.app.work_tabs.insert(key.clone(), entry);
    d.app.host_icons.insert(key.0.clone(), "🐧".into());
    assert_eq!(
        d.app.live_work_tab(&key, &live),
        None,
        "a tab wearing the previous icon fails the title check"
    );
}

#[test]
fn work_tabs_persist_across_restart() {
    // The work-tab map must survive a dashboard restart: the terminal keeps the
    // tabs alive, so `w` should return to them instead of spawning duplicates.
    let mut d = TestDashboard::new(120, 18);
    // Production creates the state dir at startup (write_dashboard_pid_and_window);
    // tests must do so before the first state-file write.
    let _ = std::fs::create_dir_all(crate::state::state_dir());
    let key = (crate::state::HostId::local(), "/home/test/a".to_string());
    d.app.work_tabs.insert(
        key.clone(),
        super::WorkTab {
            tab_id: TabId::from(7),
            window_id: Some(WindowId::from(11)),
        },
    );
    d.app.save_work_tabs();

    // A fresh App (simulating a restart) starts empty, then re-seeds from disk.
    let mut restarted = App::new();
    assert!(
        restarted.work_tabs.is_empty(),
        "a fresh App has no work tabs"
    );
    restarted.load_work_tabs();
    let seeded = restarted.work_tabs.get(&key).expect("re-seeded from disk");
    assert_eq!(seeded.tab_id, TabId::from(7));
    assert_eq!(
        seeded.window_id,
        Some(WindowId::from(11)),
        "the recorded window id should round-trip through work-tabs.json"
    );

    // The `w` handler persists after `live_work_tab` prunes a dead entry, so the
    // pruned map doesn't resurrect it on the next restart. Simulate that here:
    // prune in-memory, then save (as the handler does).
    restarted.live_work_tab(&key, &[]);
    assert!(
        !restarted.work_tabs.contains_key(&key),
        "live_work_tab prunes the stale entry in memory"
    );
    restarted.save_work_tabs();
    let mut again = App::new();
    again.load_work_tabs();
    assert!(
        !again.work_tabs.contains_key(&key),
        "a pruned-then-saved entry should not survive to the next restart"
    );
}

#[test]
fn launch_tab_title_expands_template() {
    use super::run::expand_tab_title;
    use crate::agent::AgentControl;

    // The default template names the session's project and backend.
    assert_eq!(
        expand_tab_title("{agent}: {basename}", AgentControl::Claude, "/home/test/r3"),
        "Claude: r3"
    );
    assert_eq!(
        expand_tab_title("{agent}: {basename}", AgentControl::Codex, "/home/test/r3"),
        "Codex: r3"
    );
    // {cwd} carries the full path; a pathless cwd falls back to itself.
    assert_eq!(
        expand_tab_title("[{cwd}]", AgentControl::Claude, "/a/b"),
        "[/a/b]"
    );
    assert_eq!(
        expand_tab_title("{basename}", AgentControl::Claude, "/"),
        "/"
    );
    // A literal title (no placeholders) passes through untouched.
    assert_eq!(
        expand_tab_title("Claude (new)", AgentControl::Claude, "/x"),
        "Claude (new)"
    );
}

#[test]
fn spawn_target_respects_capabilities_and_layout() {
    use super::run::resolve_spawn_target;
    use crate::terminal::{Capabilities, SessionsLayout, SpawnTarget};

    let kitty = Capabilities::default();
    // The backends' real values, not hand-built literals, so a future
    // capability field can't silently diverge from what a backend reports.
    let zellij = crate::terminal::zellij::CAPABILITIES;
    // tmux: neither stacks nor floats — the one backend that exercises the
    // Stacked fallback arm below (it was dead code before tmux existed).
    let bare = crate::terminal::tmux::CAPABILITIES;
    assert!(!bare.window_stacking && !bare.floating_sessions);

    // Per-tab: a fresh tab per session on every backend.
    for caps in [kitty, zellij, bare] {
        assert!(matches!(
            resolve_spawn_target(caps, SessionsLayout::PerTab),
            SpawnTarget::NewTab
        ));
    }

    // Stacked: a window-stacking backend (kitty) uses the shared `miao:sessions`
    // stack tab.
    assert!(matches!(
        resolve_spawn_target(kitty, SessionsLayout::Stacked),
        SpawnTarget::SharedStackTab
    ));
    // Stacked: a floating-sessions backend (zellij) floats every session in the
    // shared sessions tab.
    assert!(matches!(
        resolve_spawn_target(zellij, SessionsLayout::Stacked),
        SpawnTarget::Floating
    ));
    // Stacked: a backend that neither stacks nor floats (tmux) → a tab per
    // session, i.e. the same answer as Per-tab.
    assert!(matches!(
        resolve_spawn_target(bare, SessionsLayout::Stacked),
        SpawnTarget::NewTab
    ));

    // …which is exactly why `Space l` is not offered there: both layouts resolve
    // to the same target, so the toggle would only flip a persisted label. The
    // key, its `?`-help entry and the header indicator all hang off this.
    assert!(!bare.layout_is_a_choice());
    assert!(kitty.layout_is_a_choice());
    assert!(zellij.layout_is_a_choice());
}

#[test]
fn sessions_layout_label_round_trips() {
    use crate::terminal::SessionsLayout;
    // The persistence contract dashboard-overrides.json relies on: label() is
    // what's written, from_label() reads it back, and an unknown value falls
    // through to None (so the default is kept).
    for l in [SessionsLayout::Stacked, SessionsLayout::PerTab] {
        assert_eq!(SessionsLayout::from_label(l.label()), Some(l));
        assert_eq!(l.toggled().toggled(), l);
        assert_ne!(l.toggled(), l);
    }
    assert_eq!(SessionsLayout::from_label("bogus"), None);
    assert_eq!(SessionsLayout::default(), SessionsLayout::Stacked);
}

/// Committing a hosts-panel row used to rebuild every backend, so changing one
/// host's target dropped and re-dialled every *other* host too — and an emoji
/// edit did the same. What a backend is built from is the label and the
/// transport, so a cosmetic edit carries every connection across untouched.
#[test]
fn only_a_changed_connection_string_reconnects() {
    use super::hosts::HostConfig;
    use crate::state::HostId;
    use std::collections::HashSet;
    let host = |label: &str, ssh: &str, icon: &str| HostConfig {
        label: label.into(),
        ssh: Some(ssh.into()),
        socket: None,
        icon: Some(icon.into()),
        disabled: false,
        options: Vec::new(),
        clipboard: false,
    };
    let none = HashSet::new();
    let ids = |hosts: &[HostConfig]| App::dialled_identities(hosts, &none);
    let before = vec![host("box", "user@box", "🖥")];

    // Icon-only: identical identities, so nothing re-dials.
    assert_eq!(ids(&before), ids(&[host("box", "user@box", "🛰")]));
    // Target, label, and transport kind each move it.
    assert_ne!(ids(&before), ids(&[host("box", "user@other", "🖥")]));
    assert_ne!(ids(&before), ids(&[host("box2", "user@box", "🖥")]));
    assert_ne!(
        ids(&before),
        ids(&[HostConfig {
            label: "box".into(),
            ssh: None,
            socket: Some("/run/x.sock".into()),
            icon: Some("🖥".into()),
            disabled: false,
            options: Vec::new(),
            clipboard: false,
        }])
    );
    // A port forward is part of the ssh child's argv, so changing the set has to
    // re-dial — nothing else re-runs `setup_ssh`, and a forward that only took
    // effect after the next unrelated reconnect would read as the field simply
    // not working.
    assert_ne!(
        ids(&before),
        ids(&[HostConfig {
            options: vec!["-C".into()],
            ..host("box", "user@box", "🖥")
        }])
    );
    // And so does the clipboard toggle: it is one more `-R` on the tunnel child.
    // Turning it *off* is the direction that matters — a forward that lingered
    // until the next unrelated reconnect would leave the host reading a
    // clipboard the user had just revoked.
    assert_ne!(
        ids(&before),
        ids(&[HostConfig {
            clipboard: true,
            ..host("box", "user@box", "🖥")
        }])
    );
    // A suspended host is absent rather than present-and-off, which is what
    // makes `c` drop the connection and pressing it again dial a fresh one.
    assert!(
        ids(&[HostConfig {
            disabled: true,
            ..host("box", "user@box", "🖥")
        }])
        .is_empty()
    );
    // As is a host held down for an upgrade, and one with no target typed yet.
    assert!(App::dialled_identities(&before, &HashSet::from([HostId("box".into())])).is_empty());
    assert!(
        ids(&[HostConfig {
            ssh: None,
            ..host("box", "", "🖥")
        }])
        .is_empty()
    );
    // A host aliasing the reserved local id never gets a backend — its sessions
    // would be misclassified as local everywhere `(host, pid)` keying relies on
    // `is_local()`.
    assert!(ids(&[host("local", "user@box", "🖥")]).is_empty());
}

/// The reconcile is per host: an edit to one leaves every other connection —
/// and so every other host's rows — exactly where it was. Before this, changing
/// one host re-dialled all of them, so the table blanked every remote row and
/// claimed to be loading from every box.
#[test]
fn reconciling_hosts_touches_only_the_hosts_that_changed() {
    use super::hosts::HostConfig;
    use std::collections::HashSet;
    let host = |label: &str, ssh: &str| HostConfig {
        label: label.into(),
        ssh: Some(ssh.into()),
        socket: None,
        icon: None,
        disabled: false,
        options: Vec::new(),
        clipboard: false,
    };
    let none = HashSet::new();
    let live = App::dialled_identities(&[host("a", "user@a"), host("b", "user@b")], &none);

    // Nothing changed: every wanted host claims the backend it already has.
    assert_eq!(App::plan_reconcile(&live, &live), vec![Some(0), Some(1)]);
    // `b` re-targeted: `a` is carried across, `b` is dialled anew, and the old
    // `b` (index 1, claimed by nobody) is dropped.
    let retargeted = App::dialled_identities(&[host("a", "user@a"), host("b", "user@b2")], &none);
    assert_eq!(App::plan_reconcile(&live, &retargeted), vec![Some(0), None]);
    // A host added in the middle shifts nothing: the plan is by identity, not by
    // position, so `b` keeps its connection even though its index moved.
    let inserted = App::dialled_identities(
        &[
            host("a", "user@a"),
            host("c", "user@c"),
            host("b", "user@b"),
        ],
        &none,
    );
    assert_eq!(
        App::plan_reconcile(&live, &inserted),
        vec![Some(0), None, Some(1)]
    );
    // Removing the first host leaves the second alone.
    let removed = App::dialled_identities(&[host("b", "user@b")], &none);
    assert_eq!(App::plan_reconcile(&live, &removed), vec![Some(1)]);
    // Two rows that dial the same thing get one connection each, never the same
    // one twice — the second is a fresh dial.
    let twins = App::dialled_identities(&[host("a", "user@a"), host("a", "user@a")], &none);
    assert_eq!(App::plan_reconcile(&live, &twins), vec![Some(0), None]);
}

/// The clipboard server runs only while some host is actually offered the
/// clipboard, so a user who never enables it never pays for a second process.
#[test]
fn the_clipboard_server_runs_only_when_a_host_wants_it() {
    use super::hosts::HostConfig;
    let host = |clipboard: bool, disabled: bool, socket: Option<&str>| HostConfig {
        label: "box".into(),
        ssh: socket.is_none().then(|| "user@box".to_string()),
        socket: socket.map(str::to_string),
        icon: None,
        disabled,
        options: Vec::new(),
        clipboard,
    };
    assert!(!App::any_host_wants_clipboard(&[]));
    assert!(!App::any_host_wants_clipboard(&[host(false, false, None)]));
    assert!(App::any_host_wants_clipboard(&[host(true, false, None)]));
    // A suspended host is not connected, so nothing there can ask.
    assert!(!App::any_host_wants_clipboard(&[host(true, true, None)]));
    // A socket host counts: pooled-localhost needs no forward, because the
    // socket is already on that machine — but it does need the server running
    // for the shim to find.
    assert!(App::any_host_wants_clipboard(&[host(
        true,
        false,
        Some("/run/x.sock")
    )]));
    // One host asking is enough.
    assert!(App::any_host_wants_clipboard(&[
        host(false, false, None),
        host(true, false, None)
    ]));
}

/// A prune must reach `window-bindings.json`, not just the in-memory map.
///
/// The timer prune exists precisely for the case where **no reload runs** —
/// closing an attach window moves no state file and produces no host delta — and
/// `reload_sessions` is the file's only other writer. Left unpersisted, the file
/// keeps naming a dead window for the external `focus` bell to resolve against,
/// and the next startup re-seeds the stale binding.
#[test]
fn pruning_a_dead_window_rewrites_the_bindings_file() {
    let _guard = bindings_file_guard();
    use crate::state::{HostId, WindowBinding};
    use crate::terminal::WindowId;

    let mut d = TestDashboard::new(120, 10);
    let _ = std::fs::create_dir_all(crate::state::state_dir());
    d.app.terminal_identity = Some("kitty:me".into());

    // A pooled remote row with a live attach window, recorded and persisted the
    // way an attach does it.
    let mut s = session(1, "/srv/proj", SessionStatus::Idle);
    s.host = HostId("box".into());
    s.pool_session = Some("cm-claude-1".into());
    d.set_sessions(vec![s]);
    d.app.record_window_binding(
        HostId("box".into()),
        "cm-claude-1".into(),
        WindowId::from(300u64),
    );
    d.app.write_window_bindings_file();
    let on_disk: Vec<WindowBinding> =
        crate::state::read_json(&crate::state::window_bindings_path()).unwrap_or_default();
    assert!(
        on_disk.iter().any(|b| b.token == "cm-claude-1"),
        "precondition: the binding should be on disk: {on_disk:?}"
    );

    // The window is gone from the terminal: prune, and the file must follow.
    assert!(super::run::prune_detached_from_tabs(&mut d.app, &[]));
    assert!(
        d.app
            .window_bindings
            .window_for(&HostId("box".into()), "cm-claude-1")
            .is_none()
    );
    let on_disk: Vec<WindowBinding> =
        crate::state::read_json(&crate::state::window_bindings_path()).unwrap_or_default();
    assert!(
        !on_disk.iter().any(|b| b.token == "cm-claude-1"),
        "the dropped binding is still on disk: {on_disk:?}"
    );
}

// -- Logo graphics --

/// A resize costs the logo its kitty *images*, not just its placement: ratatui
/// clears the screen on every resize, and kitty's clear frees every image the
/// deleted placements leave unreferenced — all three paws, since only the shown
/// colour is ever placed. So the invalidation must force a re-upload; marking
/// only the placement stale would re-place a freed id, which fails silently
/// under `q=2` and leaves the paw blank for the rest of the run.
#[test]
fn resize_invalidation_forces_a_logo_re_upload() {
    let mut d = TestDashboard::new(80, 24);
    d.app.logo_caps = Some(crate::terminal::graphics::CellSize { w: 8, h: 16 });
    d.app.logo_composed = true;
    d.app.logo_placed_color = Some(super::logo::PawState::Idle);
    // A cat mid-walk, its sheet already uploaded.
    d.app.start_logo_anim();
    assert!(d.app.cat_walking(), "precondition: a cat should be walking");

    d.app.invalidate_logo_graphics();

    assert!(
        !d.app.logo_composed,
        "the paws must be re-uploaded, not merely re-placed"
    );
    assert!(d.app.logo_placed_color.is_none());
    // The walk isn't retired — the cat re-transmits its sheet on the next frame
    // rather than finishing its trip invisibly.
    assert!(d.app.cat_walking());
}

// -- Ctrl-t backend cycle --

/// The ordinary case, and the one the key was designed for when there were two
/// backends: step through the installed set and wrap.
#[test]
fn the_backend_cycle_walks_only_what_is_installed() {
    use super::keys::cycle_agent;
    use crate::agent::AgentControl::{Claude, Codex, Reasonix};

    let installed = [Claude, Reasonix];
    assert_eq!(cycle_agent(Claude, &installed), Reasonix);
    // Wraps rather than stopping at the end.
    assert_eq!(cycle_agent(Reasonix, &installed), Claude);
    // A single installed backend is a no-op, not a panic on `% 1`.
    assert_eq!(cycle_agent(Claude, &[Claude]), Claude);
    // Codex isn't installed here, so the cycle never offers it.
    assert!(!installed.contains(&Codex));
}

/// Both cases that only became reachable once the cycle stopped walking
/// `ALL` — an uninstalled current agent, and nothing installed at all.
#[test]
fn the_backend_cycle_survives_an_agent_it_cannot_see() {
    use super::keys::cycle_agent;
    use crate::agent::AgentControl::{self, Claude, Codex, Reasonix};

    // Selected deliberately (`Space a`, `--agent`, config) but not on `$PATH`.
    // The first press must reach the first stop, not skip past it.
    assert_eq!(cycle_agent(Codex, &[Claude, Reasonix]), Claude);

    // Nothing resolved: far more likely a `PATH` we can't read than a machine
    // with no agents, so the key falls back to the full set instead of going
    // inert.
    assert_eq!(
        cycle_agent(Claude, &[]),
        AgentControl::ALL[1],
        "an empty available set must still advance through ALL"
    );
}
