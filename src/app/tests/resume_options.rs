use super::*;
use crate::{
    agent::{AgentControl, ResumeCandidate},
    state::HostId,
};
use ratatui::style::Modifier;

fn candidates() -> Vec<ResumeCandidate> {
    [
        AgentControl::Claude,
        AgentControl::Codex,
        AgentControl::Claude,
        AgentControl::Pi,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, agent)| ResumeCandidate {
        agent,
        session_id: format!("session-{i}"),
        cwd: "~/project".into(),
        first_prompt: Some(format!("Task {i}")),
        custom_title: None,
        git_branch: Some("feature".into()),
        mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(600),
    })
    .collect()
}

fn dashboard() -> TestDashboard {
    let mut d = dashboard_with_three_hosts(&[], &[], &[]);
    d.terminal.backend_mut().resize(120, 30);
    d.app.open_resume_picker(HostId::local(), candidates());
    d
}

#[test]
fn resume_options_default_to_all_and_filter_before_resolving_original_indices() {
    let mut d = dashboard();
    d.app.new_session_agent = AgentControl::Codex;
    assert!(d.render().contains("Agent All"));
    for c in "Task".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press_ctrl(KeyCode::Char('o'));
    assert!(d.render().contains("[0] ● All"));
    d.press(KeyCode::Char('w'));
    assert!(d.press(KeyCode::Enter).is_none());
    let picker = &d.app.picker.as_ref().unwrap().picker;
    assert_eq!(picker.input.text(), "Task");
    assert_eq!(picker.filtered(), [1]);
    let out = d.render();
    assert!(out.contains("Agent Codex"), "{out}");
    assert!(out.contains("Task 1"), "{out}");
    assert!(!out.contains("Task 0"), "{out}");
    assert!(
        matches!(d.press(KeyCode::Enter), Some(Action::ResumeSession {
        agent: AgentControl::Codex, session_id, host, ..
    }) if session_id == "session-1" && host.is_local())
    );
}

#[test]
fn resume_options_all_restores_loaded_agents_and_cancel_preserves_search_and_cursor() {
    let mut d = dashboard();
    d.press(KeyCode::Down);
    for c in "Task".chars() {
        d.press(KeyCode::Char(c));
    }
    d.press(KeyCode::Left);
    let before = format!("{:?}", d.app.picker);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('w'));
    d.press(KeyCode::Char('s'));
    d.press(KeyCode::Esc);
    assert_eq!(format!("{:?}", d.app.picker), before);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('w'));
    d.press(KeyCode::Enter);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.filtered(), [1]);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('0'));
    d.press(KeyCode::Enter);
    assert_eq!(
        d.app.picker.as_ref().unwrap().picker.filtered(),
        [0, 1, 2, 3]
    );
    assert_eq!(d.app.selected_resume().unwrap().1.session_id, "session-1");
}

#[test]
fn resume_options_ctrl_o_resumes_only_when_the_captured_session_matches() {
    let mut d = dashboard();
    d.press(KeyCode::Down);
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('w'));
    assert!(d.render().contains("Ctrl-o resume"));
    assert!(
        matches!(d.press_ctrl(KeyCode::Char('o')), Some(Action::ResumeSession {
        agent: AgentControl::Codex, session_id, host, ..
    }) if session_id == "session-1" && host.is_local())
    );
    assert!(d.app.picker.is_none());
    assert!(d.app.launch_options.is_none());

    let mut d = dashboard();
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('w'));
    assert!(d.render().contains("apply filters"));
    assert!(
        d.press_ctrl(KeyCode::Char('o')).is_none(),
        "never change the saved session's agent"
    );
    assert_eq!(d.app.input_mode, InputMode::Picker);
    assert!(d.app.launch_options.is_none());
    assert_eq!(d.app.selected_resume().unwrap().1.session_id, "session-1");
}

#[test]
fn resume_options_switch_host_without_resuming_another_hosts_session() {
    let mut d = dashboard();
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('w'));
    d.press(KeyCode::Char('s'));
    assert!(
        matches!(d.press_ctrl(KeyCode::Char('o')), Some(Action::SwitchResumeHost { host }) if host.0 == "box")
    );
    d.app.reseed_resume_picker(HostId("box".into()), Vec::new());
    d.app.set_picker_loading(true);
    d.app
        .reseed_resume_picker(HostId("box".into()), candidates());
    d.app.set_picker_loading(false);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.filtered(), [1]);
    let out = d.render();
    assert!(out.contains("Agent Codex"), "{out}");
    assert!(
        matches!(d.press(KeyCode::Enter), Some(Action::ResumeSession {
        agent: AgentControl::Codex, session_id, host, ..
    }) if session_id == "session-1" && host.0 == "box")
    );
}

#[test]
fn resume_options_empty_agent_filter_is_recoverable_with_all() {
    let mut d = dashboard();
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('r')); // Kimi Code has no candidates in this inventory.
    d.press(KeyCode::Enter);
    assert!(d.app.picker.as_ref().unwrap().picker.filtered().is_empty());
    assert!(d.render().contains("No items match filter."));
    assert!(d.press(KeyCode::Enter).is_none());
    d.press_ctrl(KeyCode::Char('o'));
    d.press(KeyCode::Char('0'));
    d.press(KeyCode::Enter);
    assert_eq!(d.app.picker.as_ref().unwrap().picker.filtered().len(), 4);
}

#[test]
fn resume_options_rechecks_managed_conversations_before_immediate_resume() {
    let mut d = dashboard();
    d.press_ctrl(KeyCode::Char('o'));
    let mut owner = session(1, "~/project", SessionStatus::Idle);
    owner.agent = AgentControl::Claude;
    owner.session_id = Some("session-0".into());
    d.set_sessions(vec![owner]);
    assert!(d.press_ctrl(KeyCode::Char('o')).is_none());
    assert!(d.app.launch_options.is_some());
    assert!(d.render().contains("already managed"));
    d.press(KeyCode::Esc);
    assert!(d.app.picker.is_some());
}

#[test]
fn resume_rows_keep_titles_paths_and_agent_labels_readable() {
    for width in [80, 140] {
        let mut d = dashboard();
        d.terminal.backend_mut().resize(width, 30);
        d.app.open_resume_picker(HostId("box".into()), candidates());
        let out = d.render();
        if width == 140 {
            println!("{out}");
        }
        let buf = d.terminal.backend().buffer();
        let (x, y) = find_cell(buf, "Task 0").unwrap();
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
        let meta = out.lines().nth(y as usize + 1).unwrap();
        assert!(
            meta.find("Claude").unwrap() < meta.find("~/project").unwrap(),
            "{out}"
        );
        assert!(
            !meta.contains("box"),
            "the host already appears above the list: {out}"
        );
        let (path_x, path_y) = find_cell(buf, "~/project").unwrap();
        assert!(!buf[(path_x, path_y)].modifier.contains(Modifier::DIM));
        assert_eq!(
            find_cell(buf, "Task 1").unwrap().1,
            y + 3,
            "a blank line separates entries"
        );
    }
}
