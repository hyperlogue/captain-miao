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
    // including forcing Kitty while inside a nested zellij (and vice versa).
    assert_eq!(
        detect_backend(Some(ConfiguredBackend::Kitty), true),
        ConfiguredBackend::Kitty
    );
    assert_eq!(
        detect_backend(Some(ConfiguredBackend::Zellij), false),
        ConfiguredBackend::Zellij
    );
}

#[test]
fn detect_backend_zellij_beats_kitty_when_unset() {
    // No override: a live zellij session wins over the ambient Kitty env
    // (nested zellij shares the outer KITTY_WINDOW_ID), else Kitty is the
    // status-quo fallback.
    assert_eq!(detect_backend(None, true), ConfiguredBackend::Zellij);
    assert_eq!(detect_backend(None, false), ConfiguredBackend::Kitty);
}
