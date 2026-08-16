use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::agent::{AgentControl, ResumeCandidate};
use crate::state::{HostId, SessionStatus};
use crate::terminal::TabTarget;

use super::format::{DIR_COLORS, ICON_SLOT_WIDTH};
use super::keymap::{Chord, Command};
use super::picker::{PickerEvent, TextInputEvent};
use super::{
    Action, App, DirEditFocus, DragTarget, HostField, HostLogView, InputMode, PickerKind,
    SessionFlag,
};

/// Max gap between two left-clicks on the same row to count as a double-click.
const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(500);

/// The backend `Ctrl-t` lands on next, given the ones currently installed.
///
/// Pure and separate from the key handler for its two edge cases, both of which
/// only became reachable once the cycle stopped walking the full set:
///
/// - **`available` is empty** — nothing resolved on `$PATH`, which is as likely
///   to mean a `PATH` we can't see as an empty machine. Fall back to every
///   backend so the key still does something rather than silently going inert.
/// - **`current` isn't in the list** — `Space a`, `--agent` and the config file
///   all still name uninstalled backends deliberately, so this is a normal
///   state, not a bug. Land on the *first* stop; advancing from a defaulted
///   index would skip it and make the second press the one that reaches it.
pub(super) fn cycle_agent(current: AgentControl, available: &[AgentControl]) -> AgentControl {
    let all: &[AgentControl] = if available.is_empty() {
        AgentControl::ALL
    } else {
        available
    };
    match all.iter().position(|a| *a == current) {
        Some(cur) => all[(cur + 1) % all.len()],
        None => all[0],
    }
}

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        // Ctrl+c always quits, regardless of current mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }
        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Search => self.handle_search_key(key),
            InputMode::Picker => self.handle_picker_key(key),
            InputMode::Help => self.handle_help_key(key),
            InputMode::Confirm => self.handle_confirm_key(key),
            InputMode::DirEdit => self.handle_dir_edit_key(key),
            InputMode::HostEdit => self.handle_host_edit_key(key),
            InputMode::Messages => {
                self.handle_message_log_key(key);
                None
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        // Only process mouse events in Normal mode; inputs/pickers consume keys only.
        if self.input_mode != InputMode::Normal {
            return None;
        }
        let pt = (mouse.column, mouse.row);
        let in_logo = self
            .logo_rect
            .map(|r| r.contains(pt.into()))
            .unwrap_or(false);
        let in_table = self
            .last_table_rect
            .map(|r| r.contains(pt.into()))
            .unwrap_or(false);
        let in_preview = self
            .last_preview_rect
            .map(|r| r.contains(pt.into()))
            .unwrap_or(false);

        // Border hit-test for resize drags (±1 tolerance). The narrow layout
        // stacks the panels vertically with an auto-sized detail height, so its
        // borders aren't draggable — the wide-only split resize stays inert.
        let vsplit_border_col = self.last_detail_rect.map(|r| r.x.saturating_sub(1));
        let hsplit_border_row = self.last_preview_rect.map(|r| r.y.saturating_sub(1));
        let on_vsplit = !self.narrow_layout
            && vsplit_border_col.is_some_and(|c| {
                (mouse.column as i32 - c as i32).abs() <= 1
                    && self
                        .last_detail_rect
                        .is_some_and(|r| mouse.row >= r.y && mouse.row < r.y + r.height)
            });
        let on_hsplit = !self.narrow_layout
            && hsplit_border_row.is_some_and(|r0| {
                (mouse.row as i32 - r0 as i32).abs() <= 1
                    && self
                        .last_preview_rect
                        .is_some_and(|r| mouse.column >= r.x && mouse.column < r.x + r.width)
            });

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if in_logo => {
                // Clicking the paw logo kicks off its little celebration.
                self.start_logo_anim();
                return None;
            }
            MouseEventKind::Down(MouseButton::Left) if on_vsplit => {
                self.drag = Some(DragTarget::VerticalSplit);
                return None;
            }
            MouseEventKind::Down(MouseButton::Left) if on_hsplit => {
                self.drag = Some(DragTarget::HorizontalSplit);
                return None;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.drag.is_some() => {
                match self.drag {
                    Some(DragTarget::VerticalSplit) => {
                        // Detail panel width = right-edge of top area - mouse column.
                        // Use table rect's right edge as a proxy for top_area.right().
                        if let Some(tbl) = self.last_table_rect {
                            let right = tbl.x + tbl.width;
                            let new_width = right.saturating_sub(mouse.column);
                            let min_detail: u16 = 20;
                            let min_table: u16 = 30;
                            let max_detail = (tbl.width + self.detail_width)
                                .saturating_sub(min_table)
                                .max(min_detail);
                            self.detail_width = new_width.clamp(min_detail, max_detail);
                        }
                    }
                    Some(DragTarget::HorizontalSplit) => {
                        if let Some(prev) = self.last_preview_rect {
                            let bottom = prev.y + prev.height;
                            let new_height = bottom.saturating_sub(mouse.row);
                            let min_preview: u16 = 4;
                            let total = prev.height
                                + (prev
                                    .y
                                    .saturating_sub(self.last_table_rect.map_or(prev.y, |t| t.y)));
                            let max_preview = total.saturating_sub(6).max(min_preview);
                            self.preview_height = new_height.clamp(min_preview, max_preview);
                        }
                    }
                    None => {}
                }
                return None;
            }
            MouseEventKind::Up(MouseButton::Left) if self.drag.is_some() => {
                self.drag = None;
                return None;
            }
            _ => {}
        }

        // If a drag is in progress, swallow other mouse events to avoid interference.
        if self.drag.is_some() {
            return None;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if in_table => {
                let rect = self.last_table_rect?;
                let Some(idx) = self.visible_index_at(mouse.row, rect) else {
                    self.last_click = None;
                    return None;
                };
                self.table_state.select(Some(idx));

                let now = Instant::now();
                let is_double = self.last_click.is_some_and(|(t, r)| {
                    r == idx && now.duration_since(t) <= DOUBLE_CLICK_THRESHOLD
                });
                if is_double {
                    self.last_click = None;
                    // Same focus-or-attach decision as Enter (the first click
                    // already selected this row), so double-clicking a running
                    // remote row with no local window attaches it over ssh.
                    return self.focus_selected();
                }
                self.last_click = Some((now, idx));
                None
            }
            MouseEventKind::ScrollUp if in_preview => {
                self.scroll_preview_up();
                None
            }
            MouseEventKind::ScrollDown if in_preview => {
                self.scroll_preview_down();
                None
            }
            MouseEventKind::ScrollUp if in_table => {
                self.select_prev();
                None
            }
            MouseEventKind::ScrollDown if in_table => {
                self.select_next();
                None
            }
            _ => None,
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Option<Action> {
        // Capture any half-typed chord and clear both flags up front, so an
        // unrelated key seen mid-sequence cancels it rather than being invisible
        // to it.
        let pending_prefix = self.pending_prefix.take();
        let was_g = std::mem::take(&mut self.pending_g);
        let chord = Chord::from_event(key);

        // Completing a leader/prefix sequence (e.g. `Space e`). On a miss the
        // second key is swallowed — this is what keeps `Space` + an unbound key
        // from falling through to a destructive single-key command like `x`.
        if let Some(prefix) = pending_prefix {
            return match self.keymap.lookup_pair(prefix, chord) {
                Some(cmd) => self.run_command(cmd),
                None => None,
            };
        }

        // `g g` (jump to top) is a fixed prefix kept outside the keymap: unlike
        // the leader, a non-`g` key after `g` falls *through* to normal handling
        // (`g` then `j` still navigates down), which the generic prefix can't
        // express. The keymap takes precedence, so binding a two-chord sequence
        // starting with `g` would shadow this.
        if was_g && key.code == KeyCode::Char('g') && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.visible_len() != 0 {
                self.table_state.select(Some(0));
            }
            return None;
        }

        // A configured prefix (the leader, by default `Space`): wait for the
        // second chord.
        if self.keymap.is_prefix(chord) {
            self.pending_prefix = Some(chord);
            return None;
        }

        // Start a `g g` sequence (only when `g` isn't itself a configured key).
        if key.code == KeyCode::Char('g')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && self.keymap.lookup_single(chord).is_none()
        {
            self.pending_g = true;
            return None;
        }

        if let Some(cmd) = self.keymap.lookup_single(chord) {
            return self.run_command(cmd);
        }

        // Digit selectors are fixed (not remappable): plain `1..9` move the
        // cursor to the N-th visible row; `Ctrl+1..9` also focus its window.
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let idx = (c as u8 - b'1') as usize;
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return self.focus_visible_by_index(idx);
            }
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
            {
                return self.select_visible_by_index(idx);
            }
        }

        None
    }

    /// Execute a resolved keymap command. Split out from key dispatch so the
    /// same body serves any key (default or remapped) bound to the command.
    fn run_command(&mut self, cmd: Command) -> Option<Action> {
        match cmd {
            Command::SelectNext => {
                self.select_next();
                None
            }
            Command::SelectPrev => {
                self.select_prev();
                None
            }
            Command::JumpBottom => {
                let len = self.visible_len();
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
                None
            }
            Command::FocusSelected => self.focus_selected(),
            Command::NewSession => {
                match self.selected_cwd() {
                    Some(cwd) => Some(Action::NewSessionSplit {
                        agent: self.new_session_agent,
                        cwd,
                        // Quick same-cwd new session opens on the *selected
                        // session's* host, so `o` on a remote row starts another
                        // session on that server (its pty pool) in the same
                        // workdir. Falls back to local when nothing's selected.
                        host: self
                            .selected_session_ref()
                            .map(|s| s.host.clone())
                            .unwrap_or_else(HostId::local),
                        // `o` is the no-questions path — same cwd, straight to a
                        // window. Isolation is a decision, so it lives on `O`'s
                        // picker (`Ctrl-w`) where it can be seen before Enter.
                        worktree: None,
                    }),
                    None => {
                        self.open_workdir_picker();
                        None
                    }
                }
            }
            Command::NewSessionPrompt => {
                self.open_workdir_picker();
                None
            }
            Command::ResumePicker => Some(Action::FetchResumeList {
                // One host at a time, defaulting to the persisted default host
                // (`Space H`); `Ctrl-h` in the picker switches (§9).
                host: self.default_host_or_local(),
            }),
            Command::ForkSession => {
                let s = self.selected_session()?;
                // Not every backend can branch a resume
                // (`AgentControl::supports_fork`). Drop the key on one that
                // can't, rather than run a plain resume under the name "fork" —
                // continuing the session in place is the single outcome someone
                // pressing `f` is trying to avoid, and it would look like it
                // had worked.
                if !s.agent.capabilities().fork {
                    return None;
                }
                // A fork follows the **focused session's** host, never the
                // default: forking is about *this* session, and its transcript
                // lives on that machine. A remote fork lands in that host's pool
                // and auto-attaches like any open (§9).
                //
                // Say so when there is no id, rather than returning `None` into
                // silence — the treatment `CopySessionId` below already gives
                // the identical condition. Usually this is the first second of
                // a session and the message is never seen, but a backend can
                // also lose its id path wholesale (opencode's first cut named
                // none), and a key that does nothing with no explanation is the
                // worst version of that. Deliberately not phrased "yet":
                // nothing promises the id ever arrives.
                let Some(session_id) = self.index_of(&s).live_session_id(&s).map(str::to_string)
                else {
                    self.set_status("No session id known for this session".to_string(), true);
                    return None;
                };
                // The fork lands per the current layout (`resolve_spawn_target`),
                // not next to the session's window.
                Some(Action::ResumeSession {
                    agent: s.agent,
                    cwd: s.cwd,
                    session_id,
                    fork: true,
                    host: s.host,
                })
            }
            Command::CopySessionId => {
                // Yank the selected session's id to the clipboard. The id isn't
                // known until the backend writes it (early in startup), so give
                // explicit feedback rather than silently doing nothing.
                let s = self.selected_session()?;
                match self.index_of(&s).live_session_id(&s) {
                    Some(sid) => Some(Action::CopySessionId(sid.to_string())),
                    None => {
                        self.set_status("No session id available yet".to_string(), true);
                        None
                    }
                }
            }
            Command::KillSelected => {
                let s = self.selected_session()?;
                // The local window to close alongside the signal, resolved through
                // the binding: a local session's own window, an attached remote's
                // `ssh attach` window, or `None` for a remote we aren't attached to
                // (signal only) — §15.3.
                let window_id = self.window_id_for_session(&s);
                let window_pid = self.window_process_pid(&s);
                Some(Action::KillSession {
                    key: s.key(),
                    host: s.host,
                    window_id,
                    window_pid,
                })
            }
            Command::DetachRemote => {
                let s = self.selected_session()?;
                // Detach only makes sense for a **pooled** session we're
                // attached to: an unpooled local session *is* its window, so
                // closing it would lose the session — that's `x`. Keyed on the
                // capability, not on locality, so it works under
                // pooled-localhost too. Closes the attach window and leaves the
                // pooled session running; the row stays and Enter re-attaches.
                let pooled = self
                    .backend_for(&s.host)
                    .is_some_and(|b| b.capabilities().pooled);
                if !pooled {
                    self.set_status(
                        "Detach is for pooled sessions; use x to kill a local one".to_string(),
                        true,
                    );
                    return None;
                }
                match (self.window_id_for_session(&s), s.pool_session.clone()) {
                    (Some(window_id), Some(token)) => Some(Action::DetachRemote {
                        host: s.host,
                        token,
                        window_id,
                    }),
                    _ => {
                        self.set_status("Not attached to this session".to_string(), true);
                        None
                    }
                }
            }
            Command::MoveToTab => {
                // zellij can't reparent a pane across tabs; the key is offered
                // only when the backend supports the move.
                if !self.capabilities.move_to_tab {
                    self.set_status(
                        "Moving to another tab is not supported by this terminal backend"
                            .to_string(),
                        true,
                    );
                    return None;
                }
                self.selected_window_id().map(Action::FetchTabsForMove)
            }
            Command::ShellTab => {
                let s = self.selected_session()?;
                Some(Action::OpenShellTab {
                    host: s.host,
                    cwd: s.cwd,
                })
            }
            Command::JumpAttention => {
                self.jump_to_next_attention();
                None
            }
            Command::RefreshPreview => {
                // Nothing to re-fetch on a backend that can't read a window at
                // all (Ghostty); say why rather than claim a refresh that the
                // fetch loop will decline anyway.
                if !self.capabilities.capture {
                    self.set_status(
                        "This terminal backend exposes no way to read a window, so there is \
                         nothing to preview"
                            .to_string(),
                        true,
                    );
                    return None;
                }
                self.request_preview_refresh();
                self.set_status("Refreshing preview…".to_string(), false);
                None
            }
            Command::ScrollPreviewUp => {
                self.scroll_preview_up();
                None
            }
            Command::ScrollPreviewDown => {
                self.scroll_preview_down();
                None
            }
            Command::ScrollPreviewLeft => {
                self.scroll_preview_left();
                None
            }
            Command::ScrollPreviewRight => {
                self.scroll_preview_right();
                None
            }
            Command::TogglePin => {
                self.toggle_session_flag(SessionFlag::Pin);
                None
            }
            Command::ToggleFollowUp => {
                // Only allow toggling "needs input" on sessions that are Idle
                // (nothing else is happening, so marking makes sense) or already
                // carry the needs-input overlay (so the user can clear it).
                let allowed = self.selected_session_ref().is_some_and(|s| {
                    self.is_follow_up(&super::flag_key(s))
                        || matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted)
                });
                if allowed {
                    self.toggle_session_flag(SessionFlag::FollowUp);
                } else {
                    self.set_status(
                        "needs-input only works on idle or needs-input sessions".to_string(),
                        false,
                    );
                }
                None
            }
            Command::Search => {
                self.input_mode = InputMode::Search;
                self.search_input.clear();
                None
            }
            Command::ClearSearch => {
                self.set_search_filter(None);
                self.status_msg = None;
                None
            }
            Command::Help => {
                self.input_mode = InputMode::Help;
                None
            }
            Command::MessageLog => {
                self.open_message_log();
                None
            }
            Command::Quit => {
                self.should_quit = true;
                None
            }
            Command::TogglePreview => {
                self.preview_visible = !self.preview_visible;
                None
            }
            Command::ToggleDetail => {
                self.detail_visible = !self.detail_visible;
                None
            }
            Command::RestartSelected => {
                self.request_restart_selected();
                None
            }
            Command::RestartAll => {
                self.request_restart_all();
                None
            }
            Command::EditDir => {
                self.open_dir_edit();
                None
            }
            Command::ToggleKeepAwake => {
                self.toggle_prevent_sleep();
                None
            }
            Command::DefaultAgent => {
                self.open_default_agent_picker();
                None
            }
            Command::DefaultHost => {
                self.open_default_host_picker();
                None
            }
            Command::StealAttach => {
                let s = self.selected_session()?;
                let Some(pool_session) = s.pool_session.clone() else {
                    self.set_status(
                        "Steal only applies to pooled sessions (this one owns its window)"
                            .to_string(),
                        true,
                    );
                    return None;
                };
                // The host overlays libshpool's live attached bit onto each row,
                // so we can tell the user whether anyone is actually there —
                // and skip the confirm entirely when nobody is. `None` means the
                // bit is unknown (the pool couldn't be read), so we still ask.
                if s.attached == Some(false) {
                    return Some(Action::AttachRemoteRunning {
                        host: s.host,
                        pool_session,
                        force: false,
                    });
                }
                self.pending_confirm = Some(super::PendingConfirm {
                    prompt: "Another terminal is attached — kick it? [y/N]".to_string(),
                    action: Action::AttachRemoteRunning {
                        host: s.host,
                        pool_session,
                        force: true,
                    },
                });
                self.input_mode = InputMode::Confirm;
                None
            }
            Command::AttachAll => self.request_attach_all(),
            Command::SessionsLayout => {
                // On a backend with no shared-tab arrangement (tmux) both layouts
                // spawn a tab per session, so the toggle would only flip a label.
                if !self.capabilities.layout_is_a_choice() {
                    self.set_status(
                        "This terminal backend gives every session its own tab; \
                         there is no other layout to switch to"
                            .to_string(),
                        true,
                    );
                    return None;
                }
                self.toggle_sessions_layout();
                None
            }
            Command::ManageHosts => {
                if super::REMOTE_ENABLED {
                    self.open_host_edit();
                } else {
                    self.set_status(
                        "Remote hosts are a work in progress — rebuild with `--features remote`"
                            .to_string(),
                        true,
                    );
                }
                None
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_search_filter(None);
                self.search_input.clear();
                None
            }
            KeyCode::Enter => {
                self.input_mode = InputMode::Normal;
                let filter = self.current_search_filter();
                self.set_search_filter(filter);
                self.search_input.clear();
                None
            }
            // Editing (insert, backspace, cursor motion, readline keys) is
            // delegated to the shared `TextInput`, which — unlike the old bare
            // buffer — guards its Char arm against Ctrl/Alt, so a stray Ctrl-U
            // no longer types a literal `u` into the filter. Re-apply the live
            // filter only when the buffer actually changed.
            _ => {
                if matches!(self.search_input.handle_key(key), TextInputEvent::Changed) {
                    let filter = self.current_search_filter();
                    self.set_search_filter(filter);
                }
                None
            }
        }
    }

    /// The current Search-mode buffer as a filter: `None` when empty.
    fn current_search_filter(&self) -> Option<String> {
        (!self.search_input.is_empty()).then(|| self.search_input.text().to_string())
    }

    /// Whether `path` is a directory on `host`. Local uses the injected
    /// `dir_exists` (real fs in production, a stub in tests); remote makes a
    /// blocking RPC to the host's server (`false` if unreachable).
    fn host_dir_exists(&self, host: &HostId, path: &str) -> bool {
        let Some(backend) = self.backend_for(host) else {
            return false;
        };
        match backend {
            // The injected probe (real fs in production, a stub in tests) keeps
            // the local arm runtime-free, so the picker's unit tests can call
            // it. It takes a *real* path: `path` is host-canonical (§3), and
            // `Path::is_dir` doesn't expand a `~` — nothing here is a shell.
            crate::backend::Backend::Local(_) => {
                (self.dir_exists)(&cm_core::paths::expand_home(path, &self.home_dir))
            }
            crate::backend::Backend::Remote(_) => {
                tokio::task::block_in_place(|| backend.dir_exists(path))
            }
        }
    }

    /// Resolve a workdir-picker submission into a launch, choosing between the
    /// highlighted recent and the typed text and rejecting anything that isn't
    /// an existing directory.
    ///
    /// `idx` is the highlighted recent (`Some`, from `Submit`) or `None` when
    /// the user submitted raw text with no filter match (`SubmitFree`).
    ///
    /// Disambiguation: a typed string that already names a directory is taken
    /// literally (so a full path that merely substrings a recent cwd still
    /// wins); otherwise the text is treated as a filter and the highlighted
    /// recent is launched (so typing `sys` + Enter opens the matched
    /// `~/.system-config`, not a bogus `sys`). Explicit Up/Down navigation
    /// always honors the highlight. A path that doesn't resolve to a directory
    /// is rejected: the picker stays open with an inline error.
    fn submit_workdir(&mut self, idx: Option<usize>) -> Option<Action> {
        let active = self.picker.as_ref()?;
        let PickerKind::Workdir {
            agent,
            host,
            worktree,
        } = &active.kind
        else {
            return None;
        };
        let agent = *agent;
        let host = host.clone();
        let worktree = worktree.as_ref().map(|a| a.requested_name());
        // Extract everything from the picker up front so its borrow ends before we
        // call `&mut self` (set_error) and the blocking host RPCs below.
        let typed = active.picker.input.text().trim().to_string();
        let user_selected = active.picker.user_selected;
        let item_path = idx
            .and_then(|i| active.picker.items.get(i))
            .and_then(|it| it.payload.clone());

        // Paths here are **host-canonical** throughout (§3): what the picker
        // shows is the wire string, a typed `~` stays a `~`, and the host itself
        // expands it — both for the existence checks below and for the launch.
        // Nothing on this side needs to know any machine's `$HOME`.

        // Fast-fail a not-fully-connected remote BEFORE any blocking RPC: the
        // checks go over the wire and `request()` would queue (freezing the TUI on
        // `block_in_place`) through the whole connect attempt. Show "unreachable"
        // and keep the picker open, rather than "not a directory" (misleading — we
        // just can't reach it).
        if !self
            .backend_for(&host)
            .is_some_and(|b| b.conn_state().is_connected())
        {
            if let Some(active) = self.picker.as_mut() {
                active.picker.set_error(format!("{} unreachable", host.0));
            }
            return None;
        }

        // Disambiguation: a typed string that already names a directory on the
        // host is taken literally; otherwise it's a filter and the highlighted
        // recent wins. We consult the host fs only when it can affect the choice —
        // an explicit selection skips the typed check entirely — and remember when
        // the typed path was confirmed a dir so we don't re-check it below.
        let (chosen_raw, typed_known_dir) = if user_selected && let Some(p) = &item_path {
            (p.clone(), false)
        } else {
            let typed_is_dir = !typed.is_empty() && self.host_dir_exists(&host, &typed);
            if typed_is_dir {
                (typed.clone(), true)
            } else if let Some(p) = &item_path {
                (p.clone(), false)
            } else if !typed.is_empty() {
                (typed.clone(), false)
            } else {
                // Nothing typed and nothing highlighted — keep the picker open.
                return None;
            }
        };

        let cwd = chosen_raw.trim().to_string();
        if cwd.is_empty() {
            return None;
        }
        // Validate, skipping a second round-trip when we already confirmed this
        // exact path is a directory (typed_known_dir ⇒ cwd == typed).
        if !typed_known_dir && !self.host_dir_exists(&host, &cwd) {
            if let Some(active) = self.picker.as_mut() {
                active.picker.set_error(format!("Not a directory: {cwd}"));
            }
            return None;
        }

        self.picker = None;
        self.workdir_completion = None;
        self.input_mode = InputMode::Normal;
        Some(Action::NewSessionSplit {
            agent,
            cwd,
            host,
            worktree,
        })
    }

    /// Build the `ResumeSession` action shared by the resume picker and the
    /// browser's resumable rows: resume `c` on `host`, no fork. The resumed
    /// session lands per the current layout (`resolve_spawn_target`).
    fn resume_action(&self, host: HostId, c: ResumeCandidate) -> Action {
        Action::ResumeSession {
            agent: c.agent,
            cwd: c.cwd,
            session_id: c.session_id,
            fork: false,
            host,
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(active) = self.picker.as_mut() else {
            self.input_mode = InputMode::Normal;
            return None;
        };
        // While naming a worktree the keyboard belongs to that field, so this
        // runs ahead of every other picker binding — including the readline
        // edits and the Ctrl-t/Ctrl-h switches, which would otherwise eat the
        // ordinary letters a name is made of. Enter commits the name and hands
        // the keyboard back to the path input; Esc disarms the request
        // entirely, which is the only way `Ctrl-g` can be taken back.
        if let PickerKind::Workdir {
            worktree: Some(arm),
            ..
        } = &mut active.kind
            && arm.naming
        {
            match key.code {
                KeyCode::Enter => {
                    arm.naming = false;
                    self.refresh_picker_footer();
                }
                KeyCode::Esc => {
                    if let PickerKind::Workdir { worktree, .. } = &mut active.kind {
                        *worktree = None;
                    }
                    self.refresh_picker_footer();
                }
                _ => {
                    arm.name.handle_key(key);
                    self.refresh_picker_footer();
                }
            }
            return None;
        }
        // Ctrl-D in the workdir picker forgets the highlighted recent cwd.
        // Intercept before the picker forwards it to TextInput as readline
        // delete-forward, which would otherwise be mostly a no-op here.
        if matches!(active.kind, PickerKind::Workdir { .. })
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('d'))
        {
            self.delete_selected_recent_cwd_in_picker();
            return None;
        }
        // Ctrl-T in the workdir picker cycles the backend this launch will use
        // — a per-launch override of the Space-a default — and updates the
        // picker title in place. (Not Ctrl-A: that's readline beginning-of-line
        // for the path input.)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('t'))
            && let PickerKind::Workdir {
                agent,
                host,
                worktree,
            } = &mut active.kind
        {
            // Cycle only the backends actually installed. At two this key was a
            // toggle; every agent we add is another stop the user probably
            // doesn't have, so without the filter the key gets worse each time
            // we add one (see `AgentControl::is_available`).
            let installed: Vec<AgentControl> = AgentControl::ALL
                .iter()
                .copied()
                .filter(|a| a.is_available())
                .collect();
            *agent = cycle_agent(*agent, &installed);
            // Switching onto an agent without worktrees disarms the request
            // rather than holding it: it would be dropped at launch, and the
            // footer hides it, so it would sit invisibly armed and then reappear
            // on a switch back — a launch nobody asked for.
            if !agent.capabilities().worktrees {
                *worktree = None;
            }
            active.picker.title = super::format::workdir_picker_title(*agent, host);
            // The popup's own status line carries the chosen agent (§9), so it
            // has to be rebuilt with it.
            self.refresh_picker_footer();
            return None;
        }
        // Ctrl-G in the workdir picker arms an isolated **g**it worktree for
        // this launch — the agent creates it (see `AgentControl::worktree_args`),
        // so the key is offered only for an agent that has the concept and is
        // otherwise reported rather than silently ignored. Per-launch only:
        // nothing about it is persisted.
        //
        // Deliberately *not* `Ctrl-W`, the obvious mnemonic: the picker's path
        // input binds it to readline delete-previous-word (pinned by
        // `picker_readline_ctrl_w_deletes_prev_word`), and an intercept here
        // would take it away from every path the user types. Same reason
        // `Ctrl-T`/`Ctrl-H` sit where they do — `a`/`e`/`b`/`f`/`d`/`u`/`k` are
        // all spoken for by the input.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('g'))
            && let PickerKind::Workdir {
                agent, worktree, ..
            } = &mut active.kind
        {
            if !agent.capabilities().worktrees {
                let label = agent.label();
                self.set_status(format!("{label} has no worktree support"), true);
                return None;
            }
            // Arming drops straight into the name field, so `Ctrl-g` + Enter is
            // the auto-named case and `Ctrl-g` + text + Enter is the named one —
            // no separate key to reach the name, and no way to arm without
            // being shown that naming was an option. Pressing it while already
            // armed disarms.
            *worktree = match worktree.take() {
                Some(_) => None,
                None => Some(super::WorktreeArm {
                    naming: true,
                    ..Default::default()
                }),
            };
            self.refresh_picker_footer();
            return None;
        }
        // Ctrl-H in the workdir picker cycles the host this launch opens on —
        // local, then each configured remote — a per-launch choice. A remote
        // host opens the session in its pty pool and attaches over ssh (§8), and
        // the picker re-seeds its recent dirs / completion / validation against
        // that machine (`reseed_workdir_for_host`).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('h'))
            && let PickerKind::Workdir { agent, host, .. } = &mut active.kind
        {
            let hosts: Vec<HostId> = self.backends.iter().map(|b| b.host_id()).collect();
            let cur = hosts.iter().position(|h| h == host).unwrap_or(0);
            *host = hosts[(cur + 1) % hosts.len()].clone();
            active.picker.title = super::format::workdir_picker_title(*agent, host);
            // Drop the `active` borrow before the reseed (it re-borrows self).
            self.reseed_workdir_for_host();
            self.refresh_picker_footer();
            return None;
        }
        // The same `Ctrl-h` in the *resume* picker re-scopes it to the next
        // host. Scoping to one host at a time is what replaced the cross-host
        // union (§9); the switch is the affordance that makes that cheap.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('h'))
            && let PickerKind::Resume { host, .. } = &active.kind
        {
            let hosts: Vec<HostId> = self.backends.iter().map(|b| b.host_id()).collect();
            let cur = hosts.iter().position(|h| h == host).unwrap_or(0);
            let next = hosts[(cur + 1) % hosts.len()].clone();
            return Some(Action::SwitchResumeHost { host: next });
        }
        match active.picker.handle_key(key) {
            PickerEvent::Noop => None,
            PickerEvent::Cancel => {
                // Take the picker out (releasing the `active` borrow) before
                // touching `self.input_mode`. Cancelling the emoji picker drops
                // back into the still-open directory editor; every other picker
                // returns to Normal.
                let active = self.picker.take().expect("picker was Some just above");
                self.workdir_completion = None;
                self.input_mode = match active.kind {
                    PickerKind::Emoji => InputMode::DirEdit,
                    PickerKind::HostEmoji => InputMode::HostEdit,
                    _ => InputMode::Normal,
                };
                None
            }
            PickerEvent::TabComplete => {
                self.complete_workdir_in_picker();
                None
            }
            PickerEvent::Submit(idx) => {
                // The workdir picker needs a filesystem check and may keep the
                // popup open on a bad path, so it must not consume the picker
                // up front. Hand it off before taking ownership.
                if matches!(
                    self.picker.as_ref().map(|a| &a.kind),
                    Some(PickerKind::Workdir { .. })
                ) {
                    return self.submit_workdir(Some(idx));
                }
                // Take the picker out before calling `&self` methods below.
                let active = self.picker.take().expect("picker was Some just above");
                self.workdir_completion = None;
                self.input_mode = InputMode::Normal;
                match active.kind {
                    PickerKind::MoveTab { window_id, tabs } => {
                        let target = if idx < tabs.len() {
                            TabTarget::Existing(tabs[idx].id.clone())
                        } else {
                            TabTarget::New
                        };
                        Some(Action::MoveWindow(window_id, target))
                    }
                    PickerKind::Resume {
                        host,
                        mut candidates,
                    } => {
                        if idx >= candidates.len() {
                            return None;
                        }
                        let c = candidates.swap_remove(idx);
                        Some(self.resume_action(host, c))
                    }
                    // Handled above via `submit_workdir` before the take.
                    PickerKind::Workdir { .. } => None,
                    PickerKind::DefaultAgent => {
                        let chosen = active
                            .picker
                            .items
                            .get(idx)
                            .and_then(|it| it.payload.as_deref())
                            .and_then(AgentControl::from_cli);
                        if let Some(a) = chosen {
                            self.new_session_agent = a;
                            self.save_overrides();
                            self.set_status(format!("Default backend: {}", a.label()), false);
                        }
                        None
                    }
                    PickerKind::DefaultHost => {
                        let chosen = active
                            .picker
                            .items
                            .get(idx)
                            .and_then(|it| it.payload.clone());
                        if let Some(label) = chosen {
                            self.default_host = HostId(label);
                            self.save_overrides();
                            let host = self.default_host.0.clone();
                            self.set_status(format!("Default host: {host}"), false);
                        }
                        None
                    }
                    PickerKind::Emoji => {
                        // The chosen emoji rides in `payload`; drop it into the
                        // editor's icon field and return there (not Normal).
                        if let Some(emoji) = active
                            .picker
                            .items
                            .get(idx)
                            .and_then(|it| it.payload.clone())
                        {
                            self.apply_emoji_pick(&emoji);
                        } else {
                            self.input_mode = InputMode::DirEdit;
                        }
                        None
                    }
                    PickerKind::HostEmoji => {
                        if let Some(emoji) = active
                            .picker
                            .items
                            .get(idx)
                            .and_then(|it| it.payload.clone())
                        {
                            self.apply_host_emoji_pick(&emoji);
                        } else {
                            self.input_mode = InputMode::HostEdit;
                        }
                        None
                    }
                }
            }
            PickerEvent::SubmitFree => {
                // Free input is only enabled for the workdir picker, which
                // resolves and validates the path itself (and may keep the
                // popup open on a bad one) by re-reading the picker text.
                if matches!(
                    self.picker.as_ref().map(|a| &a.kind),
                    Some(PickerKind::Workdir { .. })
                ) {
                    return self.submit_workdir(None);
                }
                self.picker = None;
                self.workdir_completion = None;
                self.input_mode = InputMode::Normal;
                None
            }
        }
    }

    /// The hosts panel (`Space h`). A list view with live per-host state, not a
    /// staged edit form (§9): there is no Save step, because every mutation
    /// persists as it happens — adding a host connects it immediately (so you
    /// watch its state animate in the list), an edit applies when you commit the
    /// row, and a removal takes a `d`-then-`y` confirm.
    fn handle_host_edit_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let editing = self.host_edit.as_ref()?.edit.is_some();

        // The log view owns the keyboard while it's open — it replaces the list,
        // so none of the list's keys are reachable behind it.
        if self.host_edit.as_ref()?.log_view.is_some() {
            self.handle_host_log_key(key);
            return None;
        }

        // A pending upgrade owns the keyboard until answered — or, when it is a
        // refusal rather than a question, until acknowledged.
        if let Some(prompt) = self.host_edit.as_mut()?.pending_upgrade.take() {
            if prompt.actionable && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let host = self.host_edit.as_ref()?.rows.get(prompt.row)?.host();
                return Some(Action::UpgradeHost { host });
            }
            return None;
        }

        // A pending removal owns the keyboard until answered.
        if let Some(idx) = self.host_edit.as_ref()?.pending_remove {
            let state = self.host_edit.as_mut()?;
            state.pending_remove = None;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                if idx < state.rows.len() {
                    state.rows.remove(idx);
                    state.cursor = state.cursor.min(state.rows.len());
                }
                self.apply_host_edits();
            }
            return None;
        }

        // List-mode globals (in field-edit these are text / Esc-back).
        if !editing && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.close_host_edit();
            return None;
        }

        // Ctrl-E opens the same searchable emoji picker the directory marks use
        // — one affordance, learned once. From the Icon field, and from the list
        // as the shortcut that opens the editor *on* that field: the picker was
        // otherwise five keys away from a row whose emoji you wanted to change.
        if ctrl && matches!(key.code, KeyCode::Char('e')) {
            let state = self.host_edit.as_mut()?;
            let opens_picker = match state.focus() {
                // In the Icon field the picker *is* the editor, so it shadows
                // readline's end-of-line — a field of at most four cells has
                // nowhere to jump to anyway.
                Some(HostField::Icon) => true,
                // In a text field ^e keeps that readline meaning and falls
                // through to the input below.
                Some(_) => false,
                // From the list, on a row: open the editor on Icon and go
                // straight where the key would have gone from there.
                None => {
                    let on_row = state.cursor < state.rows.len();
                    if on_row {
                        state.begin_edit(HostField::Icon);
                    }
                    on_row
                }
            };
            if opens_picker {
                self.open_emoji_picker_for_host();
                return None;
            }
        }

        let state = self.host_edit.as_mut()?;
        if let Some(focus) = state.focus() {
            // Field focus, by all three idioms the dashboard already uses: Tab
            // walks the form, ↑↓ walk it as the vertical list it looks like, and
            // ^n/^p are what the pickers bind. Backwards matters as much as
            // forwards — a form you can only cycle one way makes overshooting
            // Options cost three more presses.
            let step = match key.code {
                KeyCode::Tab | KeyCode::Down => Some(true),
                KeyCode::BackTab | KeyCode::Up => Some(false),
                KeyCode::Char('n') if ctrl => Some(true),
                KeyCode::Char('p') if ctrl => Some(false),
                _ => None,
            };
            if let Some(forward) = step {
                if let Some(edit) = state.edit.as_mut() {
                    edit.focus = focus.step(forward);
                }
                return None;
            }
            match key.code {
                // Committing a row applies it: persist + reconnect right away.
                KeyCode::Enter => {
                    state.edit = None;
                    self.apply_host_edits();
                    return None;
                }
                // And Esc abandons it — the snapshot the edit carries is what
                // makes that a real cancel rather than a second commit.
                KeyCode::Esc => {
                    state.cancel_edit();
                    return None;
                }
                KeyCode::Char('t') if ctrl && focus == HostField::Target => {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        r.is_socket = !r.is_socket;
                    }
                    return None;
                }
                // The one field with no text in it, so the keys that would move a
                // cursor have nothing to do and flip the value instead. `Enter`
                // deliberately isn't one of them: it commits the row everywhere
                // else in this form, and a key that means "save" on four fields
                // must not mean "change" on the fifth.
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    if focus == HostField::Clipboard =>
                {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        r.clipboard = !r.clipboard;
                    }
                    return None;
                }
                _ => {}
            }
            // Everything else is text. The fields are `TextInput`s, so the
            // readline keys, the arrows and Home/End all come for free — and a
            // key none of them claim is simply dropped.
            let r = state.rows.get_mut(state.cursor)?;
            match focus {
                HostField::Label => {
                    r.label.handle_key(key);
                }
                HostField::Target => {
                    r.target.handle_key(key);
                }
                HostField::Options => {
                    r.options.handle_key(key);
                }
                // Capped like the directory-mark icon, and for the same reason
                // now that the two share one table column: past ~4 cells an
                // "icon" stops reading as a mark and just widens the column for
                // every row. Post-hoc revert rather than a pre-check, so paste
                // and multi-byte input still go through `TextInput` first.
                HostField::Icon => {
                    use unicode_width::UnicodeWidthStr;
                    let prev = r.icon.text().to_string();
                    if matches!(r.icon.handle_key(key), TextInputEvent::Changed)
                        && r.icon.text().width() > ICON_SLOT_WIDTH
                    {
                        r.icon.set_text(prev);
                    }
                }
                // Nothing to type into: its own keys are handled above, and a key
                // none of them claim is dropped rather than falling through to a
                // `TextInput` this field does not have.
                HostField::Clipboard => {}
            }
        } else {
            let n = state.rows.len();
            // A modified key never falls through to the plain-letter commands
            // below: a stray `^d` in the list must not reach the removal
            // confirm. What Ctrl *does* mean here is "open the editor on this
            // key's field" — `^e` above, `^t` here — plus the pickers' own
            // ^n/^p, which are the list's ↑↓ under another name.
            if ctrl || alt {
                if ctrl {
                    match key.code {
                        KeyCode::Char('n') => state.cursor = (state.cursor + 1).min(n),
                        KeyCode::Char('p') => state.cursor = state.cursor.saturating_sub(1),
                        KeyCode::Char('t') if state.cursor < n => {
                            state.begin_edit(HostField::Target)
                        }
                        _ => {}
                    }
                }
                return None;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => state.cursor = state.cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => state.cursor = (state.cursor + 1).min(n),
                KeyCode::Char('a') => state.begin_new_row(),
                KeyCode::Char('e') | KeyCode::Enter => {
                    if state.cursor == n {
                        state.begin_new_row();
                    } else {
                        state.begin_edit(HostField::Label);
                    }
                }
                // Suspend / resume the host. No confirm: unlike `d` it destroys
                // nothing — the row, its target and its icon all stay — and the
                // same key puts it straight back. No status line either: this
                // mode's footer renders key hints, so a message would only
                // surface, stale, once the panel closed — and the row itself
                // answers immediately (dimmed, reading `disconnected`, or
                // animating back through `connecting`).
                KeyCode::Char('c') if state.cursor < n => {
                    let row = &mut state.rows[state.cursor];
                    row.disabled = !row.disabled;
                    // Persists and rebuilds: `disabled` is part of what a backend
                    // is built from, so this drops (or dials) the connection now.
                    self.apply_host_edits();
                }
                // Removal is destructive (it drops the host and its mirror), so
                // it asks first.
                KeyCode::Char('d') if state.cursor < n => {
                    state.pending_remove = Some(state.cursor);
                }
                // Upgrade the host's server. Offered only where it would land on
                // something else — the row's `↑` says so, and the footer hint
                // appears with it — so a press here always has a decision to
                // report, either the cost or the reason there isn't one.
                KeyCode::Char('u') if state.cursor < n => {
                    let row = state.cursor;
                    let host = state.rows[row].host();
                    let offer = self.selected_host_upgrade()?;
                    let prompt = match self.upgrade_blocker(&host) {
                        Some(why) => super::UpgradePrompt {
                            row,
                            text: format!("  Cannot upgrade \"{}\": {why}", host.0),
                            actionable: false,
                        },
                        None => {
                            let n = self.host_session_counts(&host).0;
                            super::UpgradePrompt {
                                row,
                                text: format!(
                                    "  Upgrade \"{}\" to {}? {} [y/N]",
                                    host.0,
                                    offer.version,
                                    match n {
                                        0 => "The daemon restarts.".to_string(),
                                        n => format!(
                                            "{n} idle {} restart with it.",
                                            super::plural_sessions(n)
                                        ),
                                    }
                                ),
                                actionable: true,
                            }
                        }
                    };
                    self.host_edit.as_mut()?.pending_upgrade = Some(prompt);
                }
                // The row shows one truncated line of a failure; `l` is where
                // the whole thing — and the steps before it — is readable.
                KeyCode::Char('l') if state.cursor < n => {
                    let host = state.rows[state.cursor].host();
                    state.log_view = Some(HostLogView {
                        host,
                        scroll: 0,
                        rows: 0,
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// Scroll keys for the connection log (`l`). Reading, not editing, so the
    /// bindings are the pager ones: `j`/`k`, the arrows, page keys, `g`/`G`.
    ///
    /// Everything else is swallowed rather than falling through to the list
    /// underneath — the same rule the `Space` prefix follows, and for the same
    /// reason: a mistyped key here must not reach `d`.
    fn handle_host_log_key(&mut self, key: KeyEvent) {
        let Some(view) = self.host_edit.as_mut().and_then(|s| s.log_view.as_mut()) else {
            return;
        };
        // The draw clamps against the live line count; a page is the viewport
        // minus one line of overlap, so you never step over a line unread.
        let page = view.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => {
                if let Some(state) = self.host_edit.as_mut() {
                    state.log_view = None;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => view.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char('f') => view.scroll += page,
            KeyCode::PageUp | KeyCode::Char('b') => view.scroll = view.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            // The draw clamps this down to the real last page — it knows the
            // line count, and it has to re-clamp on every frame anyway.
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
            _ => {}
        }
    }

    fn handle_dir_edit_key(&mut self, key: KeyEvent) -> Option<Action> {
        // Esc and Enter are unconditional — even with the text row focused
        // they should close / commit, not get inserted as text.
        match key.code {
            KeyCode::Esc => {
                self.cancel_dir_edit();
                return None;
            }
            KeyCode::Enter => {
                self.commit_dir_edit();
                return None;
            }
            _ => {}
        }

        // `r` resets the override only when Color is focused, so a future
        // third focus mode is opt-in instead of inheriting the reset bind.
        let focus = self.dir_edit.as_ref()?.focus;
        if matches!(key.code, KeyCode::Char('r')) && focus == DirEditFocus::Color {
            self.reset_dir_edit();
            return None;
        }

        // Ctrl-E from the icon field opens the searchable emoji picker. The
        // field is at most a few cells, so shadowing readline's end-of-line
        // here costs nothing. Intercept before TextInput consumes it.
        if focus == DirEditFocus::Custom
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('e'))
        {
            self.open_emoji_picker();
            return None;
        }

        // Tab/↑/↓/^n/^p toggle focus. j/k are reserved for text input — binding
        // them here would let the user *enter* Custom but never *leave* it;
        // ^n/^p carry no such cost, since `TextInput` leaves them alone
        // precisely so a list around it can have them.
        let switches_row = matches!(
            key.code,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down
        ) || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n' | 'p')));
        let s = self.dir_edit.as_mut()?;
        if switches_row {
            s.focus = match s.focus {
                DirEditFocus::Custom => DirEditFocus::Color,
                DirEditFocus::Color => DirEditFocus::Custom,
            };
            return None;
        }

        match s.focus {
            DirEditFocus::Color => {
                let len = DIR_COLORS.len();
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        s.color_idx = if s.color_idx == 0 {
                            len - 1
                        } else {
                            s.color_idx - 1
                        };
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        s.color_idx = (s.color_idx + 1) % len;
                    }
                    _ => {}
                }
            }
            DirEditFocus::Custom => {
                // Post-hoc width cap (revert on overrun) instead of pre-check
                // so paste / multi-byte input still goes through TextInput's
                // normal handling first.
                let prev = s.custom.text().to_string();
                let evt = s.custom.handle_key(key);
                if matches!(evt, TextInputEvent::Changed) {
                    use unicode_width::UnicodeWidthStr;
                    if s.custom.text().width() > ICON_SLOT_WIDTH {
                        s.custom.set_text(prev);
                    }
                }
            }
        }
        None
    }

    /// Scroll keys for the message log. Reading, not editing, so the bindings
    /// are the pager ones the hosts panel's connection log already uses —
    /// `j`/`k`, the arrows, the page keys, `g`/`G` — and everything else is
    /// swallowed rather than reaching the list underneath.
    fn handle_message_log_key(&mut self, key: KeyEvent) {
        let Some(view) = self.message_view.as_mut() else {
            return;
        };
        // The draw clamps against the live line count; a page keeps one line of
        // overlap, so paging never steps over a line unread.
        let page = view.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_message_log(),
            KeyCode::Down | KeyCode::Char('j') => view.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char('f') => view.scroll += page,
            KeyCode::PageUp | KeyCode::Char('b') => view.scroll = view.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            // Clamped down to the real last page by the draw, which knows the
            // line count and has to re-clamp every frame anyway.
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
            _ => {}
        }
    }

    fn handle_help_key(&mut self, _key: KeyEvent) -> Option<Action> {
        // Any key dismisses the help overlay.
        self.input_mode = InputMode::Normal;
        None
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.input_mode = InputMode::Normal;
                self.pending_confirm.take().map(|p| p.action)
            }
            _ => {
                self.input_mode = InputMode::Normal;
                self.pending_confirm = None;
                None
            }
        }
    }
}
