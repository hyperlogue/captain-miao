use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

use crate::state::{LauncherState, SessionStatus};
use crate::terminal::{TabId, TabInfo, TabTarget, WindowId};

use super::format::{ansi_to_lines, base64_encode, default_dir_emoji_and_color, format_coarse_age};
use super::{Action, App, InputMode};

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

// -- Mock builders --

fn session(pid: u32, cwd: &str, status: SessionStatus) -> LauncherState {
    LauncherState {
        agent: crate::agent::AgentControl::Claude,
        launcher_pid: pid,
        session_id: Some(format!("sess-{pid}")),
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
#[test]
fn the_workdir_picker_carries_its_agent_on_its_own_status_line() {
    let mut d = TestDashboard::new(120, 20);
    d.press(KeyCode::Char('O'));
    let out = d.render();
    let footer_line = out
        .lines()
        .find(|l| l.contains("Agent"))
        .unwrap_or_else(|| panic!("no agent status line in the popup:\n{out}"));
    assert!(footer_line.contains("Claude"), "{footer_line}");
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
    // A session launched directly (`miao claude`, not via the dashboard)
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

/// The host glyph shares the workdir-icon column rather than holding a Host
/// column of its own — `<host>│<workdir>`, and no `Host` header anywhere.
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
    // The foreign row wears the "lives elsewhere" glyph, divided from its
    // workdir icon; the same-terminal row pads the divider away.
    // Everything left of the name is the row's icon cell — the detail panel's
    // own border lives well to the right of it.
    let icons = |name: &str| {
        let line = out
            .lines()
            .find(|l| l.contains(name) && l.contains("Idle"))
            .unwrap_or_else(|| panic!("no {name} row:\n{out}"));
        line.split(name).next().unwrap().to_string()
    };
    let foreign_icons = icons("session-1");
    assert!(foreign_icons.contains('\u{29C9}'), "{foreign_icons:?}");
    assert!(foreign_icons.contains('\u{2502}'), "{foreign_icons:?}");
    let plain_icons = icons("session-2");
    assert!(!plain_icons.contains('\u{2502}'), "{plain_icons:?}");
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
    use crate::state::HostId;
    use crate::terminal::WindowId;
    // The reload writes window-bindings.json; a restarted dashboard seeds from it
    // and re-resolves a live local session by its launch_id (§15.7 recovery), plus
    // a remote session by its pool_session. This is the sole test that touches the
    // shared window-bindings.json file. The state dir is only created below, after
    // the first TestDashboard::new redirects `state_dir()` into a per-process
    // tempdir — creating it before the redirect would make the wrong dir and the
    // atomic write would silently fail.
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

/// …but an attention state still outranks: a parked approval prompt is urgent
/// regardless of whether a window happens to be bound here.
#[test]
fn attention_outranks_detached() {
    use crate::state::HostId;
    let mut d = TestDashboard::new(120, 12);
    let mut waiting = session(1, "/srv/away", SessionStatus::WaitingForApproval);
    waiting.host = HostId("box".into());
    waiting.pool_session = Some("cm-away".into());
    let here = session(2, "/home/test/here", SessionStatus::Idle);
    d.set_sessions(vec![waiting, here]);

    let order: Vec<u32> = d
        .app
        .visible_sessions()
        .iter()
        .map(|s| s.launcher_pid)
        .collect();
    assert_eq!(order, vec![1, 2]);
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
        muted: false,
        follow_up: false,
    });
    d.app.sessions = vec![s];
    d.app.adopt_host_flags();

    let key = super::flag_key(&d.app.sessions[0]);
    assert!(d.app.flags_of(&key).pinned);
    // A locally-issued pin sequence is assigned so it sorts among our own pins.
    assert!(d.app.flags_of(&key).pin_seq > 0);
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
    d.app
        .update_flags(super::flag_key(&d.app.sessions[1]), |f| {
            f.follow_up = true;
        });

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
    d.app.update_flags(key.clone(), |f| f.follow_up = true);
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

    // Each rest-entering transition marks follow_up; a muted row is skipped even
    // though it entered rest too. Entering `BackgroundServer` (parking a
    // long-running dev server) also arms the bell — but only on a real transition
    // into it, not for a Server row seen for the first time (prev is None at
    // startup). Entering the *busy* `BackgroundActive` (a short-term task) does
    // NOT arm — it's work in progress, and arms on its exit to Idle like Active.
    let active_to_idle = session(1, "/home/test/a", SessionStatus::Idle);
    let bg_to_idle = session(2, "/home/test/b", SessionStatus::Idle);
    let review_to_idle = session(3, "/home/test/c", SessionStatus::Idle);
    let compacting_to_compacted = session(4, "/home/test/d", SessionStatus::Compacted);
    let muted_to_idle = session(5, "/home/test/e", SessionStatus::Idle);
    let active_to_server = session(6, "/home/test/f", SessionStatus::BackgroundServer);
    let fresh_server = session(7, "/home/test/g", SessionStatus::BackgroundServer);
    let active_to_bg = session(8, "/home/test/h", SessionStatus::BackgroundActive);
    let server_to_idle = session(9, "/home/test/i", SessionStatus::Idle);
    let sessions = vec![
        active_to_idle.clone(),
        bg_to_idle.clone(),
        review_to_idle.clone(),
        compacting_to_compacted.clone(),
        muted_to_idle.clone(),
        active_to_server.clone(),
        fresh_server.clone(),
        active_to_bg.clone(),
        server_to_idle.clone(),
    ];
    d.app
        .update_flags(flag_key(&muted_to_idle), |f| f.muted = true);

    let prev: HashMap<FlagKey, SessionStatus> = [
        (flag_key(&active_to_idle), SessionStatus::Active),
        (flag_key(&bg_to_idle), SessionStatus::BackgroundActive),
        (flag_key(&review_to_idle), SessionStatus::ReviewPending),
        (
            flag_key(&compacting_to_compacted),
            SessionStatus::Compacting,
        ),
        (flag_key(&muted_to_idle), SessionStatus::Active),
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
        .update_flags(flag_key(&resumed), |f| f.follow_up = true);
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
    d.app
        .update_flags(super::flag_key(&d.app.sessions[1]), |f| f.follow_up = true);

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
    // Push 6 ASCII chars; only the first DIR_ICON_MAX_CHARS (4 cells) survive.
    for c in ['a', 'b', 'c', 'd', 'e', 'f'] {
        d.press(KeyCode::Char(c));
    }
    let custom = d.app.dir_edit.as_ref().unwrap().custom.text();
    assert_eq!(custom, "abcd");
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
        label: "polaris".to_string(),
        target: "polaris".to_string(),
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
        super::draw::host_tally_spans(&super::HostTally { good, error, down }, &ui)
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    // The cloud carries its emoji variation selector: the bare U+2601 is a
    // hairline text glyph that vanished against the header.
    // All healthy: one green number, nothing else — the quiet case stays quiet.
    assert_eq!(text(3, 0, 0), "\u{2601}\u{fe0f} 3");
    // A problem announces itself by a number appearing, not by a 0 changing.
    assert_eq!(text(2, 1, 0), "\u{2601}\u{fe0f} 2 1");
    assert_eq!(text(0, 0, 2), "\u{2601}\u{fe0f} 2");
    assert_eq!(text(1, 2, 3), "\u{2601}\u{fe0f} 1 2 3");
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
    // The session that died was muted and waiting on a follow-up.
    d.app.pending_flag_restores.insert(
        new_wid,
        super::SessionFlags {
            muted: true,
            follow_up: true,
            ..Default::default()
        },
    );

    assert!(d.app.apply_pending_flag_restores());
    let f = d.app.flags_of(&(crate::state::HostId::local(), 7));
    assert!(f.muted);
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
    d.app.update_flags((crate::state::HostId::local(), 1), |f| {
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
            muted: true,
            ..Default::default()
        },
    );
    assert!(!d.app.apply_pending_flag_restores());
    // Nothing applied, entry retained for a later reload.
    assert!(!d.app.flags_of(&(crate::state::HostId::local(), 1)).muted);
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
            muted: true,
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
fn override_glyph_gets_wide_symbol_fixup() {
    // Regression: the Nerd Font pin glyph in the override column must be
    // rewritten to its width-2 form ("glyph "), so ratatui's diff skips the
    // trailing cell and Kitty's 2-cell-wide render survives a partial repaint.
    // The fix-up scan has to clear the highlight-symbol gutter to land on the
    // override column (the panel has no left border to skip); an off-by-gutter
    // scan leaves the glyph narrow, and it half-renders at rest (only a
    // full-row repaint — e.g. selecting the row — papers over it).
    let mut d = TestDashboard::new(120, 18);
    d.set_sessions(vec![session(1, "/home/test/a", SessionStatus::Idle)]);
    d.app
        .update_flags((crate::state::HostId::local(), 1), |f| f.pinned = true);
    d.render();

    let buf = d.terminal.backend().buffer();
    let pin = "\u{eba0}";
    let glyph_cells: Vec<&str> = (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .map(|(x, y)| buf[(x, y)].symbol())
        .filter(|s| s.contains(pin))
        .collect();
    assert_eq!(
        glyph_cells.len(),
        1,
        "expected exactly one pin glyph cell, got {glyph_cells:?}"
    );
    assert_eq!(
        glyph_cells[0], "\u{eba0} ",
        "pin glyph must carry the width-2 fix-up symbol, not the bare narrow form"
    );
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
    // The backend's real value, not a hand-built literal, so a future
    // capability field can't silently diverge from what zellij reports.
    let zellij = crate::terminal::zellij::CAPABILITIES;
    // Neither stacks nor floats.
    let bare = Capabilities {
        window_stacking: false,
        floating_sessions: false,
        ..kitty
    };

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
    // Stacked: a backend that neither stacks nor floats → a tab per session.
    assert!(matches!(
        resolve_spawn_target(bare, SessionsLayout::Stacked),
        SpawnTarget::NewTab
    ));
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
