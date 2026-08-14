//! Configurable Normal-mode keybindings.
//!
//! The dashboard's Normal-mode (and `Space`-leader) commands are dispatched
//! through a [`Keymap`]: a table of [`KeySeq`] → [`Command`]. The defaults
//! reproduce the historical hard-coded bindings; a `[keybinds]` table in
//! `config.toml` overlays user remaps on top (see [`Keymap::from_config`]).
//!
//! Scope: only Normal-mode commands are remappable. The text-input modes
//! (Search / Picker / DirEdit / Confirm / Help) keep fixed keys,
//! as do a handful of structural keys that aren't table-dispatched: `Ctrl-c`
//! (always quit), the `g g` prefix (jump-to-top), and the digit selectors
//! `1..9` / `Ctrl-1..9`.
//!
//! A [`KeySeq`] is one or two [`Chord`]s. Two-chord sequences (e.g. `Space e`)
//! work via a generic prefix mechanism in `keys.rs`: the first chord of any
//! two-chord binding is a *prefix*; once pressed, the next key either completes
//! a binding or is swallowed (so `Space` + an unbound key never falls through
//! to a dangerous single-key command like `x`).

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        // Keep only the modifiers we dispatch on. Shift is meaningless for a
        // `Char` (the case already encodes it) and we never bind Super/Hyper/
        // Meta, so masking here makes a pressed key compare equal to its
        // parsed binding regardless of how the terminal reports extras.
        let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        // Shift is redundant on a `Char` (the case already encodes it) and on
        // `BackTab` (which *is* Shift+Tab — crossterm delivers it as
        // `BackTab` + SHIFT, while a parsed `"backtab"` carries no SHIFT).
        // Strip it in both so a live event compares equal to its binding.
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.remove(KeyModifiers::SHIFT);
        }
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
        // token ends in `+` the key itself is `+` (no split).
        let mut mods = KeyModifiers::NONE;
        let key_part = if token.ends_with('+') || !token.contains('+') {
            token
        } else {
            let parts: Vec<&str> = token.split('+').collect();
            let (mod_parts, last) = parts.split_at(parts.len() - 1);
            for m in mod_parts {
                match m.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
                    "alt" | "option" | "a" | "meta" | "m" => mods |= KeyModifiers::ALT,
                    "shift" | "s" => mods |= KeyModifiers::SHIFT,
                    "" => {}
                    _ => return None,
                }
            }
            last[0]
        };

        let code = parse_key_code(key_part)?;
        // `shift+<letter>` → uppercase letter, so it lands in the same chord as
        // a bare `O`. `Chord::new` then strips the (now redundant) Shift.
        if mods.contains(KeyModifiers::SHIFT)
            && let KeyCode::Char(c) = code
            && c.is_ascii_alphabetic()
        {
            return Some(Self::new(KeyCode::Char(c.to_ascii_uppercase()), mods));
        }
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

/// One or two chords. Two-chord sequences are leader/prefix bindings such as
/// `Space e`. Stored small-and-flat (no heap): a sequence is at most two chords,
/// so lookups construct one on the stack rather than allocating a `Vec`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct KeySeq {
    first: Chord,
    second: Option<Chord>,
}

impl KeySeq {
    /// Parse a whitespace-separated sequence like `"g g"` or `"Space e"` or a
    /// single `"ctrl+u"`. Rejects empty and over-long (>2 chord) sequences.
    fn parse(s: &str) -> Option<Self> {
        let mut tokens = s.split_whitespace();
        let first = Chord::parse(tokens.next()?)?;
        let second = match tokens.next() {
            Some(tok) => Some(Chord::parse(tok)?),
            None => None,
        };
        // At most two chords: a third token rejects the whole sequence.
        if tokens.next().is_some() {
            return None;
        }
        Some(Self { first, second })
    }

    fn first(&self) -> Chord {
        self.first
    }

    fn second(&self) -> Option<Chord> {
        self.second
    }

    fn len(&self) -> usize {
        if self.second.is_some() { 2 } else { 1 }
    }

    pub(super) fn display(&self) -> String {
        match self.second {
            Some(second) => format!("{} {}", self.first.display(), second.display()),
            None => self.first.display(),
        }
    }
}

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
    /// Set the persistent default *host* for new-session operations — the exact
    /// analog of `DefaultAgent`, and what replaced the cross-host unions (§9).
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
    /// Open the scrollback of footer status messages.
    MessageLog,
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
            Command::MessageLog => "messages",
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
            Command::DefaultHost => "set default host for new sessions",
            Command::StealAttach => "attach, kicking the client already attached",
            Command::AttachAll => "attach every free detached session",
            Command::SessionsLayout => "toggle session layout (stacked / per-tab)",
            Command::ManageHosts => "manage remote hosts",
            Command::MessageLog => "message log (status messages the footer showed)",
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
            Command::MessageLog => "messages",
        }
    }
}

/// Default bindings, in display order. The first string per command is its
/// canonical key; extra strings are alternates (all dispatch to the same
/// command). These reproduce the dashboard's historical hard-coded keys.
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
    (Command::KillSelected,       &["x"]),
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
    (Command::TogglePreview,      &["space v"]),
    (Command::ToggleDetail,       &["space d"]),
    (Command::RestartSelected,    &["space e"]),
    (Command::RestartAll,         &["space E"]),
    (Command::EditDir,            &["space i"]),
    (Command::ToggleKeepAwake,    &["space z"]),
    (Command::DefaultAgent,       &["space a"]),
    (Command::DefaultHost,        &["space H"]),
    (Command::StealAttach,        &["space s"]),
    (Command::AttachAll,          &["space A"]),
    (Command::SessionsLayout,     &["space l"]),
    (Command::ManageHosts,        &["space h"]),
    (Command::MessageLog,         &["space m"]),
];

/// Resolved binding table: sequence → command, plus the set of prefix chords
/// (first chords of two-chord sequences) and an ordered list for display.
pub(crate) struct Keymap {
    by_seq: HashMap<KeySeq, Command>,
    prefixes: HashSet<Chord>,
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
                    Some(seq) => seqs.push(seq),
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

        // A surviving single-chord binding whose chord *also* begins a two-chord
        // sequence is unreachable: `handle_normal_key` checks `is_prefix` first,
        // so the chord always starts a pending sequence and the single-key
        // command never fires. Warn (the prefix wins at dispatch). Walk the
        // ordered winners so the message set is deterministic.
        for (seq, cmd) in &km.ordered {
            if seq.second().is_none() && km.prefixes.contains(&seq.first()) {
                warnings.push(format!(
                    "keybinds: '{}' is bound to '{}' but also begins a leader sequence; \
                     the leader prefix wins, so '{}' is unreachable",
                    seq.first().display(),
                    cmd.id(),
                    cmd.id(),
                ));
            }
        }

        (km, warnings)
    }

    fn build(entries: Vec<(Command, Vec<KeySeq>)>) -> Self {
        let mut by_seq: HashMap<KeySeq, Command> = HashMap::new();
        let mut prefixes: HashSet<Chord> = HashSet::new();
        // Insert into the lookup map first so later duplicates win (matches the
        // collision warning's "last wins").
        for (cmd, seqs) in &entries {
            for s in seqs {
                by_seq.insert(s.clone(), *cmd);
                if s.len() == 2 {
                    prefixes.insert(s.first());
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

    /// Look up a single-chord binding.
    pub(super) fn lookup_single(&self, chord: Chord) -> Option<Command> {
        self.by_seq
            .get(&KeySeq {
                first: chord,
                second: None,
            })
            .copied()
    }

    /// Look up a two-chord (prefix) binding.
    pub(super) fn lookup_pair(&self, first: Chord, second: Chord) -> Option<Command> {
        self.by_seq
            .get(&KeySeq {
                first,
                second: Some(second),
            })
            .copied()
    }

    /// Whether `chord` begins some two-chord binding (so the dispatcher should
    /// wait for a second key).
    pub(super) fn is_prefix(&self, chord: Chord) -> bool {
        self.prefixes.contains(&chord)
    }

    /// The two-chord bindings that begin with `prefix`, as
    /// `(second-key display, command)` in display order. Drives the which-key
    /// footer strip shown while a prefix (e.g. `Space`) is pending.
    pub(super) fn continuations(&self, prefix: Chord) -> Vec<(String, Command)> {
        self.ordered
            .iter()
            .filter(|(seq, _)| seq.len() == 2 && seq.first() == prefix)
            .filter_map(|(seq, cmd)| seq.second().map(|c| (c.display(), *cmd)))
            .collect()
    }

    /// The leader prefix to advertise in the steady-state footer: the chord
    /// that begins the *most* two-chord bindings — `Space` by default, or
    /// whatever chord a remap moved the bulk of the leader sequences onto. When
    /// leader sequences are split across several prefixes, `more…` points at the
    /// one that opens the largest menu. Ties break by display order (the
    /// earliest-listed prefix wins), so the result is deterministic. `None` when
    /// no two-chord bindings exist. Derived from the live table, so it tracks a
    /// customized leader without any special-casing.
    pub(super) fn primary_prefix(&self) -> Option<String> {
        let mut counts: HashMap<Chord, usize> = HashMap::new();
        let mut order: Vec<Chord> = Vec::new();
        for (seq, _) in &self.ordered {
            if seq.len() == 2 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn defaults_build_without_panicking() {
        let km = Keymap::defaults();
        assert_eq!(km.lookup_single(chord("x")), Some(Command::KillSelected));
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
            km.lookup_pair(chord("space"), chord("e")),
            Some(Command::RestartSelected)
        );
        assert_eq!(
            km.lookup_pair(chord("space"), chord("E")),
            Some(Command::RestartAll)
        );
        // The leader chord alone isn't a single binding.
        assert_eq!(km.lookup_single(chord("space")), None);
    }

    #[test]
    fn continuations_lists_leader_options_in_order() {
        let km = Keymap::defaults();
        let conts = km.continuations(chord("space"));
        // First leader option is `v` → toggle preview.
        assert_eq!(
            conts.first(),
            Some(&("v".to_string(), Command::TogglePreview))
        );
        // Detail moved to `Space d`; the icon editor now owns `Space i`.
        assert!(conts.contains(&("d".to_string(), Command::ToggleDetail)));
        assert!(conts.contains(&("i".to_string(), Command::EditDir)));
        // Every option is a real leader command; a non-prefix yields nothing.
        assert!(conts.iter().any(|(_, c)| *c == Command::DefaultAgent));
        assert!(km.continuations(chord("x")).is_empty());
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
            crate::config::KeyBinding::One("X".to_string()),
        );
        let (km, warnings) = Keymap::from_config(&cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.lookup_single(chord("X")), Some(Command::KillSelected));
        // Old default `x` is freed.
        assert_eq!(km.lookup_single(chord("x")), None);
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
            km.lookup_pair(chord("ctrl+x"), chord("e")),
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
        assert!(KeySeq::parse("a b c").is_none(), "3-chord seq rejected");
        assert!(KeySeq::parse("").is_none());
    }
}
