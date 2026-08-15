//! Pure-policy tests over a synthetic snapshot — no backend involved.

use super::*;

#[test]
fn id_deserializes_from_string_or_integer() {
    // Current format: JSON string.
    assert_eq!(
        serde_json::from_str::<WindowId>("\"42\"").unwrap(),
        WindowId::from(42)
    );
    assert_eq!(
        serde_json::from_str::<TabId>("\"7\"").unwrap(),
        TabId::from(7)
    );
    // Pre-abstraction format: JSON integer, coerced to its decimal string.
    assert_eq!(
        serde_json::from_str::<WindowId>("42").unwrap(),
        WindowId::from(42)
    );
    assert_eq!(serde_json::from_str::<TabId>("7").unwrap(), TabId::from(7));
    // Always serializes back out as a string.
    assert_eq!(
        serde_json::to_string(&WindowId::from(42)).unwrap(),
        "\"42\""
    );
}

fn win(id: u64) -> WindowId {
    WindowId::from(id)
}

fn tab(id: u64, title: &str, focused: bool, windows: Vec<WindowId>) -> Tab {
    Tab {
        id: TabId::from(id),
        title: title.to_string(),
        is_focused: focused,
        windows,
    }
}

#[test]
fn list_tabs_summarizes() {
    let tabs = vec![tab(7, "title", true, vec![win(10), win(11)])];
    let info = list_tabs(&tabs);
    assert_eq!(
        info,
        vec![TabInfo {
            id: TabId::from(7),
            title: "title".to_string(),
            window_count: 2,
            is_focused: true,
        }]
    );
}

#[test]
fn window_tab_map_maps_every_window_to_its_tab() {
    let tabs = vec![
        tab(7, "a", true, vec![win(10), win(11)]),
        tab(8, "b", false, vec![win(20)]),
    ];
    let map = window_tab_map(&tabs);
    assert_eq!(map.get(&WindowId::from(10)), Some(&TabId::from(7)));
    assert_eq!(map.get(&WindowId::from(11)), Some(&TabId::from(7)));
    assert_eq!(map.get(&WindowId::from(20)), Some(&TabId::from(8)));
    assert_eq!(map.get(&WindowId::from(99)), None);
    assert_eq!(map.len(), 3);
}

#[test]
fn tail_lines_returns_last_n() {
    let s = "a\nb\nc\nd\ne";
    assert_eq!(tail_lines(s, 2), "d\ne");
    assert_eq!(tail_lines(s, 0), "");
    assert_eq!(tail_lines(s, 99), s);
}

#[test]
fn detect_backend_override_beats_env() {
    // An explicit `[terminal] backend` pins the backend regardless of env —
    // including forcing Kitty while inside a nested multiplexer (and vice versa).
    for zellij in [true, false] {
        for tmux in [true, false] {
            for ghostty in [true, false] {
                for pinned in [
                    ConfiguredBackend::Kitty,
                    ConfiguredBackend::Tmux,
                    ConfiguredBackend::Zellij,
                    ConfiguredBackend::Ghostty,
                ] {
                    let live = LiveBackends {
                        zellij,
                        tmux,
                        ghostty,
                    };
                    assert_eq!(detect_backend(Some(pinned), live), pinned);
                }
            }
        }
    }
}

#[test]
fn detect_backend_prefers_zellij_then_tmux_then_ghostty_then_kitty() {
    // No override: a live multiplexer wins over the ambient emulator env (a
    // nested mux shares both the outer KITTY_WINDOW_ID and the outer
    // TERM_PROGRAM), else Kitty is the status-quo fallback. Zellij stays ahead
    // of tmux when both are live — the env can't say which is inner, and this
    // order keeps existing zellij users unchanged.
    let live = |zellij, tmux, ghostty| LiveBackends {
        zellij,
        tmux,
        ghostty,
    };
    assert_eq!(
        detect_backend(None, live(true, true, true)),
        ConfiguredBackend::Zellij
    );
    assert_eq!(
        detect_backend(None, live(true, false, true)),
        ConfiguredBackend::Zellij
    );
    assert_eq!(
        detect_backend(None, live(false, true, true)),
        ConfiguredBackend::Tmux
    );
    assert_eq!(
        detect_backend(None, live(false, false, true)),
        ConfiguredBackend::Ghostty
    );
    assert_eq!(
        detect_backend(None, LiveBackends::default()),
        ConfiguredBackend::Kitty
    );
}

/// Ghostty is the one backend with no shared-tab arrangement *and* no capture,
/// so both of the derived policies that key off `Capabilities` have to land the
/// same way they do for the backends that share each trait: `Space l` is not a
/// choice (tmux's shape), and the preview declines up front.
#[test]
fn ghostty_capabilities_resolve_the_derived_policies() {
    let caps = ghostty::CAPABILITIES;
    assert!(
        !caps.layout_is_a_choice(),
        "with neither stacked arrangement, `Space l` would toggle a label that changes nothing"
    );
    assert!(!caps.capture);
    assert!(!caps.move_to_tab);
    // tmux differs only in being able to reparent a pane, so the two must agree
    // on everything else — a new capability field that Ghostty can't do either
    // should be a deliberate edit here, not a silent divergence.
    assert_eq!(
        caps,
        Capabilities {
            move_to_tab: false,
            capture: false,
            ..tmux::CAPABILITIES
        }
    );
}

/// Both multiplexer backends share this: a pane command inherits the *server*'s
/// environment, so an exec argv is re-pointed at the dashboard's `PATH`. One
/// test, since there is now one implementation.
#[test]
fn wrap_env_prefixes_env_path() {
    let argv = vec!["miao".to_string(), "claude".to_string()];
    assert_eq!(
        wrap_env(&argv, Some("/a:/b")),
        vec!["/usr/bin/env", "PATH=/a:/b", "miao", "claude"]
    );
    // No PATH to forward → argv unchanged.
    assert_eq!(wrap_env(&argv, None), argv);
}
