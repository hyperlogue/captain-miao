//! Configurable Normal-mode keybindings.
//!
//! The dashboard's Normal-mode (and `Space`-leader) commands are dispatched
//! through a [`Keymap`]: a table of [`KeySeq`] → [`Command`]. The defaults
//! reproduce the historical hard-coded bindings; a `[keybinds]` table in
//! `config.toml` overlays user remaps on top (see [`Keymap::from_config`]).
//!
//! Scope: only Normal-mode commands are remappable. The text-input modes
//! (Search / Picker / DirEdit / Confirm / Help) keep fixed keys,
//! as does `Ctrl-c` (always quit). The structural `g g` prefix (jump-to-top)
//! and digit selectors `1..9` / `Ctrl-1..9` are built-in fallbacks; explicit
//! configured bindings take precedence over them.
//!
//! A [`KeySeq`] is one, two, or three [`Chord`]s. Longer sequences (e.g. `Space e`,
//! `Space v p`) work via a generic prefix mechanism in `keys.rs`: every proper
//! prefix of a binding waits for the next key, which either completes a binding,
//! extends the prefix, or is swallowed (so `Space` + an unbound key never falls
//! through to a dangerous single-key command like `X`).

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// =============================================================================
// A chord, and a one-or-two chord sequence
// =============================================================================

/// A single key press: a key code plus the Ctrl/Alt modifiers that matter for
/// dispatch. Shift is folded into the character itself (e.g. `Shift+o` is
/// stored as `Char('O')`), matching crossterm's delivery and the dashboard's
/// long-standing "match on `code`, ignore Shift for letters" behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Chord {
    fn new(mut code: KeyCode, mut mods: KeyModifiers) -> Self {
        // Some enhanced events carry the base letter plus Shift instead of an
        // uppercase codepoint. Normalize those exactly like config strings.
        if mods.contains(KeyModifiers::SHIFT) {
            code = match code {
                KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                    KeyCode::Char(c.to_ascii_uppercase())
                }
                KeyCode::Tab => KeyCode::BackTab,
                other => other,
            };
        }
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.remove(KeyModifiers::SHIFT);
        }
        // Preserve unsupported modifiers: Super+X must never match plain X.
        Self { code, mods }
    }

    /// Normalize a live key event into a comparable chord.
    pub(super) fn from_event(key: KeyEvent) -> Self {
        Self::new(key.code, key.modifiers)
    }

    /// Parse one chord token like `"ctrl+u"`, `"O"`, `"<"`, `"enter"`, `"f5"`.
    /// `+` separates modifiers from the final key. Returns `None` on an
    /// unrecognized key name.
    fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        // Split modifiers off the front. The final segment is the key; if the
        // token ends in `+` the key itself is `+`, as in `ctrl++`.
        let mut mods = KeyModifiers::NONE;
        let key_part = if let Some((prefix, key)) = token.rsplit_once('+') {
            let (prefix, key) = if key.is_empty() {
                (prefix.trim_end_matches('+'), "+")
            } else {
                (prefix, key)
            };
            for m in prefix.split('+').filter(|m| !m.is_empty()) {
                match m.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
                    "alt" | "option" | "a" | "meta" | "m" => mods |= KeyModifiers::ALT,
                    "shift" | "s" => mods |= KeyModifiers::SHIFT,
                    _ => return None,
                }
            }
            key
        } else {
            token
        };

        let code = parse_key_code(key_part)?;
        Some(Self::new(code, mods))
    }

    /// Human-readable form used in the help overlay and footer, e.g. `C-u`,
    /// `↑`, `Space`, `Enter`, `?`.
    pub(super) fn display(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("C-");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("A-");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("S-");
        }
        let body = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "S-Tab".to_string(),
            KeyCode::Backspace => "Bksp".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PgUp".to_string(),
            KeyCode::PageDown => "PgDn".to_string(),
            KeyCode::Delete => "Del".to_string(),
            KeyCode::Insert => "Ins".to_string(),
            KeyCode::F(n) => format!("F{n}"),
            other => format!("{other:?}"),
        };
        s.push_str(&body);
        s
    }
}

fn parse_key_code(key: &str) -> Option<KeyCode> {
    // A single character is taken verbatim so case is preserved (`o` vs `O`).
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Some(KeyCode::Char(c));
    }
    let lower = key.to_ascii_lowercase();
    // Function keys `f1`..`f12`: parse the number once (a bare `f` is a single
    // char, already handled above).
    if let Some(n) = lower.strip_prefix('f').and_then(|d| d.parse::<u8>().ok()) {
        return Some(KeyCode::F(n));
    }
    Some(match lower.as_str() {
        "space" | "spc" => KeyCode::Char(' '),
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" | "s-tab" => KeyCode::BackTab,
        "backspace" | "bs" | "bksp" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        _ => return None,
    })
}

/// One, two, or three chords. Longer sequences are leader/prefix bindings such
/// as `Space e` or `Space v p`. Stored small-and-flat (no heap): a sequence is
/// at most three chords, so lookups construct one on the stack.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct KeySeq {
    first: Chord,
    second: Option<Chord>,
    third: Option<Chord>,
}

impl KeySeq {
    /// Parse a whitespace-separated sequence like `"g g"`, `"Space e"`,
    /// `"Space v p"`, or a single `"ctrl+u"`. Rejects empty and over-long
    /// (>3 chord) sequences.
    fn parse(s: &str) -> Option<Self> {
        let mut chords = Vec::new();
        for tok in s.split_whitespace() {
            if chords.len() == 3 {
                return None;
            }
            chords.push(Chord::parse(tok)?);
        }
        Self::from_slice(&chords)
    }

    fn from_slice(chords: &[Chord]) -> Option<Self> {
        match *chords {
            [first] => Some(Self {
                first,
                second: None,
                third: None,
            }),
            [first, second] => Some(Self {
                first,
                second: Some(second),
                third: None,
            }),
            [first, second, third] => Some(Self {
                first,
                second: Some(second),
                third: Some(third),
            }),
            _ => None,
        }
    }

    /// The first `n` chords, when this sequence is at least that long.
    fn leading(&self, n: usize) -> Option<Self> {
        let chords: Vec<Chord> = self.iter().take(n).collect();
        (chords.len() == n)
            .then(|| Self::from_slice(&chords))
            .flatten()
    }

    fn iter(&self) -> impl Iterator<Item = Chord> {
        std::iter::once(self.first)
            .chain(self.second)
            .chain(self.third)
    }

    fn first(&self) -> Chord {
        self.first
    }

    fn chord_at(&self, index: usize) -> Option<Chord> {
        match index {
            0 => Some(self.first),
            1 => self.second,
            2 => self.third,
            _ => None,
        }
    }

    fn starts_with(&self, prefix: &[Chord]) -> bool {
        prefix
            .iter()
            .enumerate()
            .all(|(i, chord)| self.chord_at(i) == Some(*chord))
    }

    fn len(&self) -> usize {
        self.iter().count()
    }

    pub(super) fn display(&self) -> String {
        self.iter()
            .map(|chord| chord.display())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// =============================================================================
// The commands a key can be bound to
// =============================================================================

/// Every remappable Normal-mode command. Each carries a stable config id (used
/// as the `[keybinds]` key) and a help description. The `keys.rs` dispatcher
/// turns a resolved `Command` into the matching side effect via `run_command`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Command {
    // Navigation
    SelectNext,
    SelectPrev,
    JumpBottom,
    // Session actions
    FocusSelected,
    NewSession,
    NewSessionPrompt,
    ResumePicker,
    ForkSession,
    CopySessionId,
    KillSelected,
    DetachRemote,
    MoveToTab,
    ShellTab,
    JumpAttention,
    RefreshPreview,
    // Preview scrolling
    ScrollPreviewUp,
    ScrollPreviewDown,
    ScrollPreviewLeft,
    ScrollPreviewRight,
    // Flags
    TogglePin,
    ToggleFollowUp,
    // Modes
    Search,
    ClearSearch,
    Help,
    Quit,
    // Leader (Space …)
    TogglePreview,
    ToggleDetail,
    RestartSelected,
    RestartAll,
    EditDir,
    ToggleKeepAwake,
    DefaultAgent,
    /// Compatibility command for custom bindings; opens the Hosts panel,
    /// where the first row now owns the default.
    DefaultHost,
    SessionsLayout,
    ManageHosts,
    /// Attach to the selected pooled session, kicking whatever client currently
    /// holds it. Behind a y/N confirm: the pool is one client at a time, so this
    /// takes someone else's terminal away (§10.2).
    StealAttach,
    /// Attach a window to every detached pooled session that is free to take —
    /// the manual form of the reconnect sweep. Rows another client holds are
    /// skipped rather than stolen: a steal is a per-session decision (§10.2),
    /// and one keypress must not kick a roomful of terminals.
    AttachAll,
    /// Dismiss the newest notification.
    DismissNotification,
    /// Open the notification history.
    MessageLog,
    /// Open the preferences overlay.
    Preferences,
    /// Open the full session record. The side detail panel keeps the glance.
    SessionDetail,
    /// Publish the selected session's branch.
    VcsPush,
    /// Fast-forward the selected session from its upstream.
    VcsPull,
}

impl Command {
    /// Stable id used as the `[keybinds]` table key.
    pub(super) fn id(self) -> &'static str {
        match self {
            Command::SelectNext => "next",
            Command::SelectPrev => "prev",
            Command::JumpBottom => "bottom",
            Command::FocusSelected => "focus",
            Command::NewSession => "new_session",
            Command::NewSessionPrompt => "new_session_cwd",
            Command::ResumePicker => "resume",
            Command::ForkSession => "fork",
            Command::CopySessionId => "copy_id",
            Command::KillSelected => "kill",
            Command::DetachRemote => "detach",
            Command::MoveToTab => "move_tab",
            Command::ShellTab => "shell_tab",
            Command::JumpAttention => "jump_attention",
            Command::RefreshPreview => "refresh_preview",
            Command::ScrollPreviewUp => "scroll_up",
            Command::ScrollPreviewDown => "scroll_down",
            Command::ScrollPreviewLeft => "scroll_left",
            Command::ScrollPreviewRight => "scroll_right",
            Command::TogglePin => "pin",
            Command::ToggleFollowUp => "needs_input",
            Command::Search => "search",
            Command::ClearSearch => "clear",
            Command::Help => "help",
            Command::Quit => "quit",
            Command::TogglePreview => "toggle_preview",
            Command::ToggleDetail => "toggle_detail",
            Command::RestartSelected => "restart",
            Command::RestartAll => "restart_all",
            Command::EditDir => "edit_dir",
            Command::ToggleKeepAwake => "keep_awake",
            Command::DefaultAgent => "default_agent",
            Command::DefaultHost => "default_host",
            Command::StealAttach => "steal_attach",
            Command::AttachAll => "attach_all",
            Command::SessionsLayout => "sessions_layout",
            Command::ManageHosts => "manage_hosts",
            Command::DismissNotification => "dismiss_notification",
            Command::MessageLog => "messages",
            Command::Preferences => "preferences",
            Command::SessionDetail => "session_detail",
            Command::VcsPush => "vcs_push",
            Command::VcsPull => "vcs_pull",
        }
    }

    fn from_id(id: &str) -> Option<Command> {
        DEFAULTS.iter().map(|(c, _)| *c).find(|c| c.id() == id)
    }

    /// Short help description shown in the keybindings overlay.
    pub(super) fn description(self) -> &'static str {
        match self {
            Command::SelectNext => "next session",
            Command::SelectPrev => "previous session",
            Command::JumpBottom => "jump to bottom",
            Command::FocusSelected => "focus selected window",
            Command::NewSession => "new session (same cwd)",
            Command::NewSessionPrompt => "new session (prompt for cwd; Ctrl-g for a worktree)",
            Command::ResumePicker => "resume picker",
            // Only ever a fork. The "/ resume selected in place" half dates from
            // before the key refused to plain-resume, which it now does rather
            // than quietly deliver the one outcome a fork exists to avoid.
            Command::ForkSession => "fork the selected session",
            Command::CopySessionId => "copy selected session id to clipboard",
            Command::KillSelected => "kill selected session",
            Command::DetachRemote => "detach remote session (keep it running)",
            Command::MoveToTab => "move window to another tab",
            Command::ShellTab => "switch to / open the cwd's work tab",
            Command::JumpAttention => "jump to next attention",
            Command::RefreshPreview => "refresh preview now",
            Command::ScrollPreviewUp => "scroll preview up",
            Command::ScrollPreviewDown => "scroll preview down",
            Command::ScrollPreviewLeft => "scroll preview left",
            Command::ScrollPreviewRight => "scroll preview right",
            Command::TogglePin => "pin",
            Command::ToggleFollowUp => "toggle needs-input (idle only)",
            Command::Search => "search",
            Command::ClearSearch => "clear search / status",
            Command::Help => "help",
            Command::Quit => "quit",
            Command::TogglePreview => "toggle preview panel",
            Command::ToggleDetail => "toggle detail panel",
            Command::RestartSelected => "restart selected (idle only, confirm)",
            Command::RestartAll => "restart all (idle only, confirm)",
            Command::EditDir => "edit directory icon + color (^E emoji picker)",
            Command::ToggleKeepAwake => "toggle keep-awake (prevent OS sleep)",
            // No parenthetical list of backends. It read as a closed set and so
            // went stale the moment a third arrived — but deriving one from
            // `ALL` only trades a wrong list for an unwieldy one, since this
            // grows to seven names. The picker it opens shows them all anyway.
            Command::DefaultAgent => "set default new-session backend",
            Command::DefaultHost => "reorder hosts to set the default",
            Command::StealAttach => "attach, kicking the client already attached",
            Command::AttachAll => "attach every free detached session",
            Command::SessionsLayout => "toggle session layout (stacked / per-tab)",
            Command::ManageHosts => "manage remote hosts",
            Command::DismissNotification => "dismiss the newest notification",
            Command::MessageLog => "message log (notification history)",
            Command::Preferences => "open preferences",
            Command::SessionDetail => "session record (pid, terminfo, context, first prompt)",
            Command::VcsPush => "push the branch",
            Command::VcsPull => "pull from the remote (fast-forward only)",
        }
    }

    /// Terse one-or-two-word label for the compact which-key footer strip.
    pub(super) fn short_label(self) -> &'static str {
        match self {
            Command::SelectNext => "next",
            Command::SelectPrev => "prev",
            Command::JumpBottom => "bottom",
            Command::FocusSelected => "focus",
            Command::NewSession => "new",
            Command::NewSessionPrompt => "new (cwd)",
            Command::ResumePicker => "resume",
            Command::ForkSession => "fork",
            Command::CopySessionId => "copy id",
            Command::KillSelected => "kill",
            Command::DetachRemote => "detach",
            Command::MoveToTab => "move tab",
            Command::ShellTab => "shell",
            Command::JumpAttention => "attn",
            Command::RefreshPreview => "refresh",
            Command::ScrollPreviewUp => "scroll up",
            Command::ScrollPreviewDown => "scroll down",
            Command::ScrollPreviewLeft => "scroll left",
            Command::ScrollPreviewRight => "scroll right",
            Command::TogglePin => "pin",
            Command::ToggleFollowUp => "needs-input",
            Command::Search => "search",
            Command::ClearSearch => "clear",
            Command::Help => "help",
            Command::Quit => "quit",
            Command::TogglePreview => "preview",
            Command::ToggleDetail => "detail",
            Command::RestartSelected => "restart",
            Command::RestartAll => "restart all",
            Command::EditDir => "color",
            Command::ToggleKeepAwake => "keep-awake",
            Command::DefaultAgent => "agent",
            Command::DefaultHost => "host",
            Command::StealAttach => "steal",
            Command::AttachAll => "attach all",
            Command::SessionsLayout => "layout",
            Command::ManageHosts => "hosts",
            Command::DismissNotification => "dismiss notification",
            Command::MessageLog => "messages",
            Command::Preferences => "prefs",
            Command::SessionDetail => "session",
            Command::VcsPush => "push",
            Command::VcsPull => "pull",
        }
    }

    fn is_vcs(self) -> bool {
        matches!(self, Command::VcsPush | Command::VcsPull)
    }

    fn is_toggle(self) -> bool {
        matches!(
            self,
            Command::TogglePreview
                | Command::ToggleDetail
                | Command::ToggleKeepAwake
                | Command::SessionDetail
        )
    }
}

// =============================================================================
// The default table
// =============================================================================

/// Default bindings, in display order. The first string per command is its
/// canonical key; extra strings are alternates (all dispatch to the same
/// command). Existing assignments and command ids are compatibility contracts:
/// do not repurpose them for new actions. Add related actions beneath an
/// existing leader group, keep useful aliases, and leave removed keys vacant.
/// Navigation uses lowercase letters/arrows; explicit session termination is X;
/// panel toggles live under Space t and version-control actions under Space v.
#[rustfmt::skip]
const DEFAULTS: &[(Command, &[&str])] = &[
    (Command::SelectNext,         &["j", "down", "ctrl+n"]),
    (Command::SelectPrev,         &["k", "up", "ctrl+p"]),
    (Command::JumpBottom,         &["G"]),
    (Command::FocusSelected,      &["enter"]),
    (Command::NewSession,         &["o"]),
    (Command::NewSessionPrompt,   &["O"]),
    (Command::ResumePicker,       &["r"]),
    (Command::ForkSession,        &["f"]),
    (Command::CopySessionId,      &["y"]),
    (Command::KillSelected,       &["X"]),
    (Command::DetachRemote,       &["D"]),
    (Command::MoveToTab,          &["t"]),
    (Command::ShellTab,           &["w"]),
    (Command::JumpAttention,      &["s"]),
    (Command::RefreshPreview,     &["R"]),
    (Command::ScrollPreviewUp,    &["ctrl+u"]),
    (Command::ScrollPreviewDown,  &["ctrl+d"]),
    (Command::ScrollPreviewLeft,  &["h", "left", "<"]),
    (Command::ScrollPreviewRight, &["l", "right", ">"]),
    (Command::TogglePin,          &["p"]),
    (Command::ToggleFollowUp,     &["i"]),
    (Command::Search,             &["/"]),
    (Command::ClearSearch,        &["esc"]),
    (Command::Help,               &["?"]),
    (Command::Quit,               &["q"]),
    (Command::TogglePreview,      &["space t v"]),
    (Command::ToggleDetail,       &["space t d"]),
    (Command::SessionDetail,      &["space t s"]),
    (Command::ToggleKeepAwake,    &["space t z"]),
    (Command::VcsPush,            &["space v p"]),
    (Command::VcsPull,            &["space v l"]),
    (Command::RestartSelected,    &["space e"]),
    (Command::RestartAll,         &["space E"]),
    (Command::EditDir,            &["space i"]),
    (Command::DefaultAgent,       &[]),
    (Command::DefaultHost,        &[]),
    (Command::StealAttach,        &["space s"]),
    (Command::AttachAll,          &["space A"]),
    (Command::SessionsLayout,     &[]),
    (Command::ManageHosts,        &["space h"]),
    (Command::MessageLog,         &["space m"]),
    (Command::DismissNotification, &["x"]),
    (Command::Preferences,        &[",", "space p"]),
];

// =============================================================================
// The keymap: build it, then ask it
// =============================================================================

/// What the next key of a pending prefix will do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Continuation {
    /// The next key runs this command.
    Run(Command),
    /// The next key opens another which-key page. The label is the page name.
    Menu(&'static str),
}

/// Resolved binding table: sequence → command, plus the proper prefixes of
/// multi-chord sequences and an ordered list for display.
pub(crate) struct Keymap {
    by_seq: HashMap<KeySeq, Command>,
    /// Leading one- and two-chord sequences that a longer binding continues.
    prefixes: HashSet<KeySeq>,
    /// `(seq, command)` in default display order, filtered to the entries that
    /// actually won in `by_seq` (so the help overlay never shows a stale key).
    ordered: Vec<(KeySeq, Command)>,
}

impl Keymap {
    /// The built-in defaults with no user overrides.
    #[cfg(test)]
    pub(super) fn defaults() -> Self {
        let entries: Vec<(Command, Vec<KeySeq>)> = DEFAULTS
            .iter()
            .map(|(cmd, keys)| {
                let seqs = keys
                    .iter()
                    .map(|k| KeySeq::parse(k).expect("built-in default binding must parse"))
                    .collect();
                (*cmd, seqs)
            })
            .collect();
        Self::build(entries)
    }

    /// Build the keymap from the defaults overlaid with a `[keybinds]` config
    /// table (`command-id → key | [keys]`). Overriding a command *replaces*
    /// all of its default keys. Any sequence claimed by an override is removed
    /// from the non-overridden command that previously held it. Returns the
    /// keymap plus human-readable warnings for unknown ids / unparseable keys /
    /// collisions, which the caller surfaces to the user.
    pub(super) fn from_config(
        cfg: &HashMap<String, crate::config::KeyBinding>,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut overrides: HashMap<Command, Vec<KeySeq>> = HashMap::new();

        for (id, binding) in cfg {
            let Some(cmd) = Command::from_id(id) else {
                warnings.push(format!("keybinds: unknown command '{id}'"));
                continue;
            };
            let mut seqs = Vec::new();
            for key in binding.keys() {
                match KeySeq::parse(key) {
                    Some(seq) => {
                        if seq.iter().any(|chord| {
                            chord == Chord::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
                        }) && !(cmd == Command::Quit && seq.len() == 1)
                        {
                            warnings.push(format!(
                                "keybinds.{id}: '{key}' uses Ctrl-c, which is reserved for quit"
                            ));
                        } else {
                            seqs.push(seq);
                        }
                    }
                    None => warnings.push(format!("keybinds.{id}: cannot parse key '{key}'")),
                }
            }
            // An empty list (or all-unparseable) unbinds the command rather
            // than silently falling back to the default — that's the only way
            // to express "I never want this key".
            overrides.insert(cmd, seqs);
        }

        let claimed: HashSet<KeySeq> = overrides.values().flatten().cloned().collect();

        let mut entries: Vec<(Command, Vec<KeySeq>)> = Vec::with_capacity(DEFAULTS.len());
        for (cmd, keys) in DEFAULTS {
            if let Some(seqs) = overrides.get(cmd) {
                entries.push((*cmd, seqs.clone()));
            } else {
                // Keep defaults, minus any sequence an override stole.
                let seqs: Vec<KeySeq> = keys
                    .iter()
                    .filter_map(|k| KeySeq::parse(k))
                    .filter(|s| !claimed.contains(s))
                    .collect();
                entries.push((*cmd, seqs));
            }
        }

        // Warn on collisions between two overridden commands (last wins).
        let mut seen: HashMap<KeySeq, Command> = HashMap::new();
        for (cmd, seqs) in &entries {
            for s in seqs {
                if let Some(prev) = seen.insert(s.clone(), *cmd)
                    && prev != *cmd
                {
                    warnings.push(format!(
                        "keybinds: '{}' bound to both '{}' and '{}' ('{}' wins)",
                        s.display(),
                        prev.id(),
                        cmd.id(),
                        cmd.id(),
                    ));
                }
            }
        }

        let km = Self::build(entries);

        // A binding that is also a proper prefix of a longer one is unreachable:
        // `handle_normal_key` extends the prefix instead of firing the command.
        // Warn (the longer sequence wins at dispatch). Walk the ordered winners
        // so the message set is deterministic.
        for (seq, cmd) in &km.ordered {
            if seq.len() < 3 && km.prefixes.contains(seq) {
                warnings.push(format!(
                    "keybinds: '{}' is bound to '{}' but also begins a longer sequence; \
                     the prefix wins, so '{}' is unreachable",
                    seq.display(),
                    cmd.id(),
                    cmd.id(),
                ));
            }
        }

        (km, warnings)
    }

    fn build(entries: Vec<(Command, Vec<KeySeq>)>) -> Self {
        let mut by_seq: HashMap<KeySeq, Command> = HashMap::new();
        let mut prefixes: HashSet<KeySeq> = HashSet::new();
        // Insert into the lookup map first so later duplicates win (matches the
        // collision warning's "last wins").
        for (cmd, seqs) in &entries {
            for s in seqs {
                by_seq.insert(s.clone(), *cmd);
                if s.len() >= 2
                    && let Some(head) = s.leading(1)
                {
                    prefixes.insert(head);
                }
                if s.len() == 3
                    && let Some(head) = s.leading(2)
                {
                    prefixes.insert(head);
                }
            }
        }
        // Ordered display list, filtered to entries that actually won.
        let mut ordered = Vec::new();
        for (cmd, seqs) in &entries {
            for s in seqs {
                if by_seq.get(s) == Some(cmd) {
                    ordered.push((s.clone(), *cmd));
                }
            }
        }
        Self {
            by_seq,
            prefixes,
            ordered,
        }
    }

    /// Look up the command bound to this exact sequence.
    pub(super) fn lookup(&self, chords: &[Chord]) -> Option<Command> {
        let seq = KeySeq::from_slice(chords)?;
        self.by_seq.get(&seq).copied()
    }

    /// Look up a single-chord binding.
    pub(super) fn lookup_single(&self, chord: Chord) -> Option<Command> {
        self.lookup(&[chord])
    }

    /// Whether `chord` begins some longer binding (so the dispatcher should
    /// wait for another key).
    pub(super) fn is_prefix(&self, chord: Chord) -> bool {
        self.is_prefix_seq(&[chord])
    }

    /// Whether `chords` is a proper prefix of some longer binding.
    pub(super) fn is_prefix_seq(&self, chords: &[Chord]) -> bool {
        KeySeq::from_slice(chords).is_some_and(|seq| self.prefixes.contains(&seq))
    }

    /// Bindings one chord longer than `so_far`, in display order. A next chord
    /// that finishes a command is [`Continuation::Run`]. A next chord that only
    /// continues toward longer bindings is [`Continuation::Menu`], so `Space`
    /// can offer `t` even though every toggle completes on the third chord.
    pub(super) fn continuations(&self, so_far: &[Chord]) -> Vec<(String, Continuation)> {
        let mut seen: Vec<(String, Continuation)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (seq, cmd) in &self.ordered {
            if !seq.starts_with(so_far) || seq.len() <= so_far.len() {
                continue;
            }
            let Some(next) = seq.chord_at(so_far.len()) else {
                continue;
            };
            let key = next.display();
            if seq.len() == so_far.len() + 1 {
                if let Some(slot) = index.get(&key) {
                    seen[*slot].1 = Continuation::Run(*cmd);
                } else {
                    index.insert(key.clone(), seen.len());
                    seen.push((key, Continuation::Run(*cmd)));
                }
            } else if !index.contains_key(&key) {
                index.insert(key.clone(), seen.len());
                seen.push((key, Continuation::Menu(self.menu_label(so_far, next))));
            }
        }
        seen
    }

    /// Label for a which-key entry that does not finish a command. The toggle
    /// prefix is the one menu whose commands are all toggles.
    fn menu_label(&self, so_far: &[Chord], next: Chord) -> &'static str {
        let mut prefix = so_far.to_vec();
        prefix.push(next);
        let commands: Vec<Command> = self
            .ordered
            .iter()
            .filter(|(seq, _)| seq.starts_with(&prefix))
            .map(|(_, cmd)| *cmd)
            .collect();
        if commands.iter().all(|cmd| cmd.is_toggle()) {
            "toggles"
        } else if commands.iter().all(|cmd| cmd.is_vcs()) {
            "vcs"
        } else {
            "more"
        }
    }

    /// The leader prefix to advertise in the steady-state footer: the chord
    /// that begins the *most* multi-chord bindings — `Space` by default, or
    /// whatever chord a remap moved the bulk of the leader sequences onto. When
    /// leader sequences are split across several prefixes, `more…` points at the
    /// one that opens the largest menu. Ties break by display order (the
    /// earliest-listed prefix wins), so the result is deterministic. `None` when
    /// no multi-chord bindings exist. Derived from the live table, so it tracks a
    /// customized leader without any special-casing.
    pub(super) fn primary_prefix(&self) -> Option<String> {
        let mut counts: HashMap<Chord, usize> = HashMap::new();
        let mut order: Vec<Chord> = Vec::new();
        for (seq, _) in &self.ordered {
            if seq.len() >= 2 {
                let first = seq.first();
                if !counts.contains_key(&first) {
                    order.push(first);
                }
                *counts.entry(first).or_insert(0) += 1;
            }
        }
        // Walk in display order, replacing only on a strictly larger count, so
        // the earliest prefix wins ties.
        let mut best: Option<Chord> = None;
        let mut best_count = 0;
        for chord in order {
            let n = counts[&chord];
            if n > best_count {
                best_count = n;
                best = Some(chord);
            }
        }
        best.map(|c| c.display())
    }

    /// The canonical (first-listed) key bound to `command`, for compact spots
    /// like the footer. `None` when the command is unbound.
    pub(super) fn primary_key(&self, command: Command) -> Option<String> {
        self.ordered
            .iter()
            .find(|(_, c)| *c == command)
            .map(|(s, _)| s.display())
    }

    /// All keys bound to `command`, joined as `"j / ↓ / C-n"` for the help
    /// overlay. `None` when the command is unbound.
    pub(super) fn keys_for(&self, command: Command) -> Option<String> {
        let joined = self
            .ordered
            .iter()
            .filter(|(_, c)| *c == command)
            .map(|(s, _)| s.display())
            .collect::<Vec<_>>()
            .join(" / ");
        (!joined.is_empty()).then_some(joined)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn defaults_have_unique_reachable_bindings_and_valid_command_ids() {
        let (keymap, warnings) = Keymap::from_config(&HashMap::new());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut commands = HashSet::new();
        let mut sequences = HashSet::new();
        for (command, keys) in DEFAULTS {
            assert!(
                commands.insert(command.id()),
                "duplicate command {}",
                command.id()
            );
            assert_eq!(Command::from_id(command.id()), Some(*command));
            for key in *keys {
                let sequence = KeySeq::parse(key).unwrap();
                assert!(
                    sequences.insert(sequence.clone()),
                    "duplicate default {key}"
                );
                assert!(
                    !keymap.prefixes.contains(&sequence),
                    "unreachable default {key}"
                );
                assert_eq!(keymap.by_seq.get(&sequence), Some(command));
            }
        }
    }

    #[test]
    fn enhanced_shift_events_and_config_chords_match() {
        assert_eq!(
            Chord::from_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SHIFT)),
            chord("X")
        );
        assert_eq!(
            Chord::from_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            chord("shift+tab")
        );
        assert_eq!(chord("shift+tab"), chord("backtab"));
        assert_eq!(chord("shift+enter").display(), "S-Enter");
        assert_eq!(chord("ctrl+shift+up").display(), "C-S-↑");
        assert_eq!(
            chord("ctrl++"),
            Chord::from_event(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::CONTROL))
        );
        let super_x = Chord::from_event(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SUPER));
        assert_eq!(Keymap::defaults().lookup_single(super_x), None);
    }

    #[test]
    fn config_warns_about_chords_intercepted_by_global_quit() {
        for key in ["ctrl+c", "space ctrl+c", "ctrl+c x"] {
            let cfg = HashMap::from([("kill".into(), crate::config::KeyBinding::One(key.into()))]);
            let (map, warnings) = Keymap::from_config(&cfg);
            assert!(
                warnings.iter().any(|w| w.contains("reserved for quit")),
                "{warnings:?}"
            );
            assert_eq!(map.keys_for(Command::KillSelected), None);
        }
        let cfg = HashMap::from([(
            "quit".into(),
            crate::config::KeyBinding::One("ctrl+c".into()),
        )]);
        assert!(Keymap::from_config(&cfg).1.is_empty());
    }

    #[test]
    fn defaults_build_without_panicking() {
        let km = Keymap::defaults();
        assert_eq!(km.lookup_single(chord("X")), Some(Command::KillSelected));
        assert_eq!(km.lookup(&[chord("space"), chord("n")]), None);
        assert_eq!(
            km.lookup_single(chord("x")),
            Some(Command::DismissNotification)
        );
        assert_eq!(
            km.lookup_single(chord("enter")),
            Some(Command::FocusSelected)
        );
        assert_eq!(
            km.lookup_single(chord("ctrl+u")),
            Some(Command::ScrollPreviewUp)
        );
    }

    #[test]
    fn shift_letter_normalizes_to_uppercase_char() {
        // `O`, `shift+o`, and a live Shift+O event all resolve identically.
        assert_eq!(chord("O"), chord("shift+o"));
        let ev = KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT);
        assert_eq!(Chord::from_event(ev), chord("O"));
        let km = Keymap::defaults();
        assert_eq!(
            km.lookup_single(chord("O")),
            Some(Command::NewSessionPrompt)
        );
        assert_eq!(km.lookup_single(chord("o")), Some(Command::NewSession));
    }

    #[test]
    fn ctrl_modifier_is_significant() {
        let km = Keymap::defaults();
        assert_eq!(km.lookup_single(chord("ctrl+n")), Some(Command::SelectNext));
        // Plain `n` is unbound by default.
        assert_eq!(km.lookup_single(chord("n")), None);
    }

    #[test]
    fn leader_sequences_are_prefixes() {
        let km = Keymap::defaults();
        assert!(km.is_prefix(chord("space")));
        assert!(!km.is_prefix(chord("x")));
        assert_eq!(
            km.lookup(&[chord("space"), chord("e")]),
            Some(Command::RestartSelected)
        );
        assert_eq!(
            km.lookup(&[chord("space"), chord("E")]),
            Some(Command::RestartAll)
        );
        // The leader chord alone isn't a single binding.
        assert_eq!(km.lookup_single(chord("space")), None);
    }

    #[test]
    fn continuations_lists_leader_options_in_order() {
        let km = Keymap::defaults();
        let conts = km.continuations(&[chord("space")]);
        // Every toggle lives under `Space t`, so the leader offers one menu
        // rather than a command on `t`.
        assert_eq!(
            conts.first(),
            Some(&("t".to_string(), Continuation::Menu("toggles")))
        );
        assert!(conts.contains(&("i".to_string(), Continuation::Run(Command::EditDir))));
        assert!(
            conts
                .iter()
                .any(|(_, c)| *c == Continuation::Run(Command::Preferences))
        );
        let toggles = km.continuations(&[chord("space"), chord("t")]);
        assert_eq!(
            toggles.first(),
            Some(&("v".to_string(), Continuation::Run(Command::TogglePreview)))
        );
        assert!(toggles.contains(&("d".to_string(), Continuation::Run(Command::ToggleDetail))));
        assert!(toggles.contains(&("s".to_string(), Continuation::Run(Command::SessionDetail))));
        assert!(toggles.contains(&("z".to_string(), Continuation::Run(Command::ToggleKeepAwake))));
        assert!(!toggles.iter().any(|(_, c)| {
            matches!(
                c,
                Continuation::Run(Command::TogglePin | Command::ToggleFollowUp)
            )
        }));
        assert_eq!(km.lookup_single(chord("p")), Some(Command::TogglePin));
        assert_eq!(km.lookup_single(chord("i")), Some(Command::ToggleFollowUp));
        assert_eq!(km.lookup_single(chord("v")), None);
        assert!(km.continuations(&[chord("x")]).is_empty());
    }

    #[test]
    fn horizontal_scroll_aliases_h_l_arrows() {
        let km = Keymap::defaults();
        assert_eq!(
            km.lookup_single(chord("h")),
            Some(Command::ScrollPreviewLeft)
        );
        assert_eq!(
            km.lookup_single(chord("l")),
            Some(Command::ScrollPreviewRight)
        );
        assert_eq!(
            km.lookup_single(chord("left")),
            Some(Command::ScrollPreviewLeft)
        );
        assert_eq!(
            km.lookup_single(chord("right")),
            Some(Command::ScrollPreviewRight)
        );
        // The old `<`/`>` keys remain as alternates.
        assert_eq!(
            km.lookup_single(chord("<")),
            Some(Command::ScrollPreviewLeft)
        );
    }

    #[test]
    fn keys_for_joins_alternates_in_order() {
        let km = Keymap::defaults();
        assert_eq!(
            km.keys_for(Command::SelectNext).as_deref(),
            Some("j / ↓ / C-n")
        );
        assert_eq!(
            km.keys_for(Command::RestartSelected).as_deref(),
            Some("Space e")
        );
    }

    #[test]
    fn override_replaces_default_key() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "kill".to_string(),
            crate::config::KeyBinding::One("delete".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            km.lookup_single(chord("delete")),
            Some(Command::KillSelected)
        );
        // The kill binding is freed without affecting notification dismissal.
        assert_eq!(km.lookup_single(chord("X")), None);
        assert_eq!(
            km.lookup_single(chord("x")),
            Some(Command::DismissNotification)
        );
    }

    #[test]
    fn override_can_add_multiple_keys() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "jump_attention".to_string(),
            crate::config::KeyBinding::Many(vec!["s".to_string(), "n".to_string()]),
        );
        let (km, _) = Keymap::from_config(&cfg);
        assert_eq!(km.lookup_single(chord("s")), Some(Command::JumpAttention));
        assert_eq!(km.lookup_single(chord("n")), Some(Command::JumpAttention));
    }

    #[test]
    fn stealing_a_key_frees_it_from_the_old_command() {
        // Bind `kill` to `s`; the default owner of `s` (jump_attention) loses it.
        let mut cfg = HashMap::new();
        cfg.insert(
            "kill".to_string(),
            crate::config::KeyBinding::One("s".to_string()),
        );
        let (km, _) = Keymap::from_config(&cfg);
        assert_eq!(km.lookup_single(chord("s")), Some(Command::KillSelected));
        assert_eq!(km.keys_for(Command::JumpAttention), None);
        assert_eq!(km.keys_for(Command::KillSelected).as_deref(), Some("s"));
    }

    #[test]
    fn empty_override_unbinds() {
        let mut cfg = HashMap::new();
        cfg.insert("help".to_string(), crate::config::KeyBinding::Many(vec![]));
        let (km, _) = Keymap::from_config(&cfg);
        assert_eq!(km.lookup_single(chord("?")), None);
        assert_eq!(km.keys_for(Command::Help), None);
    }

    #[test]
    fn single_chord_shadowed_by_prefix_warns() {
        // Bind `search` to the bare leader chord `space`, which still begins every
        // `space …` sequence: the single-key `search` can never fire.
        let mut cfg = HashMap::new();
        cfg.insert(
            "search".to_string(),
            crate::config::KeyBinding::One("space".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        // `space` still leads the leader menu, so it stays a prefix …
        assert!(km.is_prefix(chord("space")));
        // … and the shadowing is reported as unreachable.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unreachable") && w.contains("search")),
            "{warnings:?}"
        );
    }

    #[test]
    fn unknown_command_and_bad_key_warn() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "bogus".to_string(),
            crate::config::KeyBinding::One("x".to_string()),
        );
        cfg.insert(
            "kill".to_string(),
            crate::config::KeyBinding::One("nope+".to_string()),
        );
        let (_km, warnings) = Keymap::from_config(&cfg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unknown command 'bogus'"))
        );
        assert!(warnings.iter().any(|w| w.contains("cannot parse key")));
    }

    #[test]
    fn primary_prefix_picks_most_common_leader() {
        // Default: every leader sequence lives on `Space`.
        let km = Keymap::defaults();
        assert_eq!(km.primary_prefix().as_deref(), Some("Space"));

        // Move the majority of leader sequences onto `ctrl+x`; the footer's
        // `more…` should follow the crowd to the larger menu.
        // Move more than half of the leader sequences onto `ctrl+x`; the
        // footer's `more…` should follow the crowd to the larger menu.
        let moved = [
            ("toggle_preview", "ctrl+x v"),
            ("toggle_detail", "ctrl+x d"),
            ("restart", "ctrl+x e"),
            ("restart_all", "ctrl+x E"),
            ("edit_dir", "ctrl+x i"),
            ("keep_awake", "ctrl+x z"),
            ("default_agent", "ctrl+x a"),
            ("session_detail", "ctrl+x s"),
            ("vcs_push", "ctrl+x p"),
            ("vcs_pull", "ctrl+x l"),
        ];
        let mut cfg = HashMap::new();
        for (id, key) in moved {
            cfg.insert(
                id.to_string(),
                crate::config::KeyBinding::One(key.to_string()),
            );
        }
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        // The moved set must actually be the majority, whatever the leader menu
        // grows to — assert that rather than a hard-coded count, so adding a
        // `Space` binding can't silently invert the test's premise.
        let space_left = DEFAULTS
            .iter()
            .filter(|(c, _)| km.keys_for(*c).is_some_and(|k| k.starts_with("Space ")))
            .count();
        assert!(
            moved.len() > space_left,
            "test premise broken: {} moved vs {space_left} left on Space",
            moved.len()
        );
        assert_eq!(km.primary_prefix().as_deref(), Some("C-x"));
    }

    #[test]
    fn custom_leader_chord_becomes_prefix() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "restart".to_string(),
            crate::config::KeyBinding::One("ctrl+x e".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(km.is_prefix(chord("ctrl+x")));
        assert_eq!(
            km.lookup(&[chord("ctrl+x"), chord("e")]),
            Some(Command::RestartSelected)
        );
    }

    #[test]
    fn special_keys_parse() {
        assert_eq!(
            chord("<"),
            Chord::new(KeyCode::Char('<'), KeyModifiers::NONE)
        );
        assert_eq!(
            chord("?"),
            Chord::new(KeyCode::Char('?'), KeyModifiers::NONE)
        );
        assert_eq!(
            chord("space"),
            Chord::new(KeyCode::Char(' '), KeyModifiers::NONE)
        );
        assert_eq!(chord("f5"), Chord::new(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(chord("up"), Chord::new(KeyCode::Up, KeyModifiers::NONE));
        let three = KeySeq::parse("a b c").expect("3-chord seq");
        assert_eq!(three.len(), 3);
        assert_eq!(three.display(), "a b c");
        assert!(KeySeq::parse("a b c d").is_none(), "4-chord seq rejected");
        assert!(KeySeq::parse("").is_none());
    }

    #[test]
    fn three_chord_sequence_is_a_prefix_then_a_binding() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "restart".to_string(),
            crate::config::KeyBinding::One("space v e".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(km.is_prefix(chord("space")));
        assert!(km.is_prefix_seq(&[chord("space"), chord("v")]));
        assert!(!km.is_prefix_seq(&[chord("space"), chord("v"), chord("e")]));
        assert_eq!(
            km.lookup(&[chord("space"), chord("v"), chord("e")]),
            Some(Command::RestartSelected)
        );
        let nested = km.continuations(&[chord("space"), chord("v")]);
        assert!(nested.contains(&("e".to_string(), Continuation::Run(Command::RestartSelected))));
    }

    #[test]
    fn shorter_binding_hidden_by_three_chord_prefix_warns() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "restart".to_string(),
            crate::config::KeyBinding::One("space v".to_string()),
        );
        cfg.insert(
            "restart_all".to_string(),
            crate::config::KeyBinding::One("space v e".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Space v") && w.contains("unreachable")),
            "{warnings:?}"
        );
        assert!(km.is_prefix_seq(&[chord("space"), chord("v")]));
        assert_eq!(
            km.lookup(&[chord("space"), chord("v"), chord("e")]),
            Some(Command::RestartAll)
        );
    }
}
