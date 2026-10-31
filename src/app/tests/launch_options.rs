use super::*;
use crate::{
    agent::AgentControl,
    backend::{Backend, RemoteBackend},
    state::HostId,
};

fn with_hosts(count: usize, width: u16, height: u16) -> TestDashboard {
    let mut d = TestDashboard::new(width, height);
    d.app.recent_cwds = vec!["~/project".into()];
    for i in 1..count {
        let host = HostId(format!("host-{i:02}"));
        d.app
            .backends
            .push(Backend::Remote(RemoteBackend::unconnected_for_tests(
                host.clone(),
                vec![],
            )));
        d.app
            .recent_dirs_cache
            .insert(host, vec!["~/other".into(), "~/project".into()]);
    }
    d
}

fn open(d: &mut TestDashboard) {
    d.press(KeyCode::Char('O'));
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.app.launch_options.is_some());
}

#[test]
fn launch_options_start_with_defaults_and_follow_enabled_preference_order() {
    let mut d = with_hosts(3, 100, 24);
    d.app.agent_order = vec![
        (AgentControl::Kimi, false),
        (AgentControl::Codex, true),
        (AgentControl::Claude, true),
    ];
    d.app.new_session_agent = AgentControl::Codex;
    d.app.extra_prefs.host_order = Some(vec!["host-02".into(), "local".into(), "host-01".into()]);
    open(&mut d);
    let out = d.render();
    assert!(out.contains("[q] ● Codex"), "{out}");
    assert!(out.contains("[a] ● host-02"), "{out}");
    assert!(!out.contains("Kimi"), "{out}");
    d.press(KeyCode::Char('w'));
    d.press(KeyCode::Char('s'));
    assert!(
        d.press(KeyCode::Enter).is_none(),
        "Enter keeps choices without launching"
    );
    assert!(d.app.launch_options.is_none());
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(matches!(&d.app.picker.as_ref().unwrap().kind,
        super::super::PickerKind::Workdir { agent: AgentControl::Claude, host, .. } if host.is_local()));
    assert_eq!(d.app.new_session_agent, AgentControl::Codex);
    assert_eq!(d.app.default_host_or_local(), HostId("host-02".into()));
}

#[test]
fn launch_options_cancel_restores_the_entire_directory_picker() {
    let mut d = with_hosts(3, 100, 24);
    d.press(KeyCode::Char('O'));
    for c in "proj".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Left);
    d.press_ctrl(KeyCode::Char('g'));
    for c in "feature".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Enter);
    let before = format!("{:?}", d.app.picker);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('y')); // opencode drops the draft worktree.
    d.press(KeyCode::Char('s'));
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.render().contains("host-01 unreachable"));
    d.press(KeyCode::Esc);
    assert!(d.app.launch_options.is_none());
    assert_eq!(format!("{:?}", d.app.picker), before);
    assert_eq!(d.app.input_mode, InputMode::Picker);
}

#[test]
fn launch_options_ctrl_o_launches_the_captured_highlight_not_the_filter() {
    let mut d = with_hosts(1, 100, 24);
    d.app.recent_cwds = vec!["~/project-one".into(), "~/project-two".into()];
    d.app.dir_exists = |path| path == "/home/test/project-two";
    d.press(KeyCode::Char('O'));
    for c in "proj".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Down);
    d.press_ctrl(KeyCode::Char('o'));
    assert!(d.render().contains("Directory: ~/project-two"));
    d.press(KeyCode::Char('w'));
    // A background refresh can change the underlying list while the selector
    // is open. The directory shown in the selector remains the launch target.
    d.app.picker.as_mut().unwrap().picker.items.clear();
    match d.press_ctrl(KeyCode::Char('o')) {
        Some(Action::NewSessionSplit {
            agent,
            host,
            cwd,
            worktree,
        }) => {
            assert_eq!(agent, AgentControl::Codex);
            assert!(host.is_local());
            assert_eq!(cwd, "~/project-two");
            assert!(worktree.is_none());
        }
        other => panic!("expected immediate launch, got {other:?}"),
    }
    assert!(d.app.picker.is_none());
    assert!(d.app.launch_options.is_none());
    assert_eq!(d.app.input_mode, InputMode::Normal);
}

#[test]
fn launch_options_paging_reaches_thirty_hosts_and_remembers_an_off_page_choice() {
    let mut d = with_hosts(30, 100, 24);
    open(&mut d);
    d.press(KeyCode::Char(','));
    assert!(d.render().contains("Host · 1/3"));
    for page in 0..3 {
        let out = d.render();
        assert!(out.contains("[q] ● Claude"), "agent keys stay fixed: {out}");
        for (offset, key) in "asdfgzxcvb".chars().enumerate() {
            d.press(KeyCode::Char(key));
            let index = page * 10 + offset;
            let host = if index == 0 {
                "local".into()
            } else {
                format!("host-{index:02}")
            };
            let out = d.render();
            assert!(out.contains(&format!("Selected: Claude · {host}")), "{out}");
            assert!(out.contains(&format!("[{key}] ● {host}")), "{out}");
        }
        d.press(KeyCode::Char('.'));
    }
    assert!(d.render().contains("Host · 3/3"), "last page does not wrap");
    d.press(KeyCode::Char(','));
    d.press(KeyCode::Char(','));
    let out = d.render();
    assert!(out.contains("Host · 1/3"));
    assert!(out.contains("Selected: Claude · host-29"));
    d.press(KeyCode::Enter);
    assert_eq!(
        d.app.selected_workdir(),
        "~/project",
        "keep the directory on the chosen host when present"
    );
    d.press_ctrl(KeyCode::Char('o'));
    assert!(
        d.render().contains("Host · 3/3"),
        "reopen on selected host's page"
    );
    d.press(KeyCode::Esc);
    d.press(KeyCode::Esc);
    open(&mut d);
    assert!(
        d.render().contains("Selected: Claude · local"),
        "a new launch starts with defaults"
    );
}

#[test]
fn launch_options_partial_page_and_modifier_keys_do_not_change_selection() {
    let mut d = with_hosts(31, 100, 24);
    open(&mut d);
    for _ in 0..3 {
        d.press(KeyCode::Char('.'));
    }
    d.press(KeyCode::Char('a'));
    d.press(KeyCode::Char('b')); // No tenth entry on this page.
    d.press_ctrl(KeyCode::Char('q'));
    d.app
        .handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
    let out = d.render();
    assert!(out.contains("Host · 4/4"), "{out}");
    assert!(out.contains("Selected: Claude · host-30"), "{out}");
}

#[test]
fn launch_options_launch_failure_keeps_drafts_and_checks_the_chosen_host() {
    let mut d = with_hosts(2, 100, 24);
    open(&mut d);
    d.press(KeyCode::Char('s'));
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.render().contains("host-01 unreachable"));
    d.press(KeyCode::Char('a'));
    d.app.dir_exists = |_| false;
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.render().contains("Not a directory: ~/project"));
    assert!(d.app.launch_options.is_some());
    d.app.dir_exists = |_| true;
    assert!(
        matches!(d.press_ctrl(KeyCode::Char('o')), Some(Action::NewSessionSplit { host, cwd, .. }) if host.is_local() && cwd == "~/project")
    );
}

#[test]
fn launch_options_empty_directory_requires_a_choice_before_launching() {
    let mut d = TestDashboard::new(100, 24);
    open(&mut d);
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.render().contains("Choose a directory before launching"));
    d.press(KeyCode::Enter);
    assert!(d.app.launch_options.is_none());
    assert!(d.app.picker.is_some());
}

#[test]
fn launch_options_keep_preserves_input_and_disarms_unsupported_worktrees() {
    let mut d = with_hosts(1, 100, 24);
    d.press(KeyCode::Char('O'));
    for c in "proj".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Left);
    d.press_ctrl(KeyCode::Char('g'));
    d.press(KeyCode::Enter);
    let before = format!("{:?}", d.app.picker.as_ref().unwrap().picker.input);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('y'));
    d.press(KeyCode::Enter);
    assert_eq!(
        format!("{:?}", d.app.picker.as_ref().unwrap().picker.input),
        before
    );
    assert!(matches!(
        d.app.picker.as_ref().unwrap().kind,
        super::super::PickerKind::Workdir {
            agent: AgentControl::OpenCode,
            worktree: None,
            ..
        }
    ));
}

#[test]
fn launch_options_respects_worktree_naming_and_launches_the_named_worktree() {
    let mut d = with_hosts(1, 100, 24);
    d.press(KeyCode::Char('O'));
    d.press_ctrl(KeyCode::Char('g'));
    d.press_ctrl(KeyCode::Char('o'));
    assert!(
        d.app.launch_options.is_none(),
        "worktree naming owns the keyboard"
    );
    for c in "feature".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Enter);
    d.press_ctrl(KeyCode::Char('o'));
    assert!(d.render().contains("Worktree: feature"));
    assert!(
        matches!(d.press_ctrl(KeyCode::Char('o')), Some(Action::NewSessionSplit { worktree: Some(name), .. }) if name == "feature")
    );
}

#[test]
fn launch_options_held_shortcuts_cannot_launch_across_view_transitions() {
    use crossterm::event::KeyEventKind;
    let mut d = with_hosts(1, 100, 24);
    open(&mut d);
    assert!(
        d.app
            .handle_key(KeyEvent::new_with_kind(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat
            ))
            .is_none()
    );
    assert!(d.app.launch_options.is_some());
    d.press(KeyCode::Enter);
    assert!(
        d.app
            .handle_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            ))
            .is_none()
    );
    assert!(d.app.picker.is_some());
    assert!(d.app.launch_options.is_none());
}

#[test]
fn launch_options_small_viewports_keep_controls_and_reveal_selected_rows() {
    for (width, height) in [(40, 12), (80, 24), (120, 30)] {
        let mut d = with_hosts(30, width, height);
        d.press(KeyCode::Char('O'));
        let out = d.render();
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("Ctrl-o"), "{width}x{height}: {footer}");
        d.press_ctrl(KeyCode::Char('o'));
        let out = d.render();
        if width == 80 {
            println!("{out}");
        }
        for text in ["Enter", "keep & return", "Ctrl-o", "launch", "Esc cancel"] {
            assert!(out.contains(text), "{width}x{height}: {out}");
        }
        d.press(KeyCode::Char('b'));
        let out = d.render();
        assert!(out.contains("[b] ● host-09"), "{width}x{height}: {out}");
        d.press(KeyCode::Char('q'));
        let out = d.render();
        assert!(out.contains("[q] ● Claude"), "{width}x{height}: {out}");
    }
}
