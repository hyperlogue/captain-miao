//! The host's forwarding list and its draft editor. Saved rules apply
//! independently; Escape cancels only an unfinished draft. SSH acknowledgements
//! and persistence finish before the displayed configuration changes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
};
use tokio::sync::oneshot;

use super::{
    App,
    format::{centered_rect, clear_overlay},
    host_edit::{EditOrigin, text_field_lines},
    picker::{TextInput, TextInputEvent},
};
use crate::{
    backend::{
        Backend,
        forwards::{Manager, Status},
    },
    ssh_forward::{self, Forward, Rule, TcpForward},
};

#[derive(Debug, Default)]
pub(super) struct ForwardView {
    cursor: usize,
    edit: Option<Draft>,
    import: Option<TextInput>,
    remove: bool,
    message: Option<String>,
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    rules: Vec<Rule>,
    result: oneshot::Receiver<Result<(), String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Kind,
    Port,
    Destination,
    DestinationPort,
    Bind,
    Raw,
    Enabled,
}

impl Field {
    fn label(self, remote: bool) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Kind => "Type",
            Self::Raw => "SSH specification",
            Self::Port => {
                if remote {
                    "Remote port"
                } else {
                    "Local port"
                }
            }
            Self::Destination => "Destination host",
            Self::DestinationPort => "Destination port",
            Self::Bind => "Bind address",
            Self::Enabled => "Enabled",
        }
    }
}

#[derive(Debug)]
struct Draft {
    index: Option<usize>,
    original: Option<Forward>,
    focus: Field,
    kind: usize,
    name: TextInput,
    port: TextInput,
    destination: TextInput,
    destination_port: TextInput,
    bind: TextInput,
    raw: Option<TextInput>,
    disabled: bool,
    linked_port: bool,
}

const FLAGS: [&str; 3] = ["-L", "-R", "-D"];
const KINDS: [&str; 3] = [
    "Local forwarding (-L)",
    "Remote forwarding (-R)",
    "SOCKS proxy (-D)",
];

impl Draft {
    fn new(rule: Option<&Rule>, index: Option<usize>) -> Self {
        let tcp = rule.and_then(|r| r.forward.tcp());
        Self {
            index,
            original: rule.map(|r| r.forward.clone()),
            focus: Field::Port,
            kind: rule
                .and_then(|r| FLAGS.iter().position(|f| *f == r.forward.flag))
                .unwrap_or(0),
            name: TextInput::with_text(rule.map(|r| r.name.clone()).unwrap_or_default()),
            port: TextInput::with_text(tcp.as_ref().map(|t| t.port.clone()).unwrap_or_default()),
            destination: TextInput::with_text(
                tcp.as_ref()
                    .map(|t| t.destination.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "localhost".into()),
            ),
            destination_port: TextInput::with_text(
                tcp.as_ref()
                    .map(|t| t.destination_port.clone())
                    .unwrap_or_default(),
            ),
            bind: TextInput::with_text(
                tcp.as_ref()
                    .map(|t| t.bind.clone())
                    .unwrap_or_else(|| "localhost".into()),
            ),
            raw: rule
                .filter(|_| tcp.is_none())
                .map(|r| TextInput::with_text(r.forward.spec.clone())),
            disabled: rule.is_some_and(|r| r.disabled),
            linked_port: rule.is_none(),
        }
    }

    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Name, Field::Kind];
        if self.raw.is_some() {
            fields.push(Field::Raw);
        } else {
            fields.push(Field::Port);
            if self.kind != 2 {
                fields.extend([Field::Destination, Field::DestinationPort]);
            }
            fields.push(Field::Bind);
        }
        fields.push(Field::Enabled);
        fields
    }

    fn input(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Port => Some(&mut self.port),
            Field::Destination => Some(&mut self.destination),
            Field::DestinationPort => Some(&mut self.destination_port),
            Field::Bind => Some(&mut self.bind),
            Field::Raw => self.raw.as_mut(),
            _ => None,
        }
    }

    fn rule(&self) -> Result<Rule, String> {
        let flag = FLAGS[self.kind];
        let forward = if let Some(raw) = &self.raw {
            Forward {
                flag: flag.into(),
                spec: raw.text().to_owned(),
            }
        } else {
            let tcp = TcpForward {
                bind: self.bind.text().trim().into(),
                port: self.port.text().trim().into(),
                destination: self.destination.text().trim().into(),
                destination_port: self.destination_port.text().trim().into(),
            };
            // A name/toggle edit must not rewrite an imported spec's bind
            // semantics (an omitted bind follows ssh_config).
            if let Some(original) = &self.original
                && original.flag == flag
                && original.tcp().as_ref() == Some(&tcp)
            {
                original.clone()
            } else {
                Forward::from_tcp(flag, &tcp)?
            }
        };
        forward.validate()?;
        Ok(Rule {
            name: self.name.text().trim().into(),
            disabled: self.disabled,
            forward,
        })
    }

    fn toggle_raw(&mut self) -> Result<(), String> {
        if let Some(raw) = &self.raw {
            let forward = Forward {
                flag: FLAGS[self.kind].into(),
                spec: raw.text().into(),
            };
            let tcp = forward
                .tcp()
                .ok_or("This specification needs the raw editor")?;
            self.port.set_text(tcp.port);
            self.destination.set_text(tcp.destination);
            self.destination_port.set_text(tcp.destination_port);
            self.bind.set_text(tcp.bind);
            self.original = Some(forward);
            self.raw = None;
            self.focus = Field::Port;
        } else {
            // A new socket forward starts in raw mode before any TCP port
            // exists. Keep incomplete form fields available until replaced.
            self.raw = Some(TextInput::with_text(
                self.rule().map(|r| r.forward.spec).unwrap_or_default(),
            ));
            self.focus = Field::Raw;
        }
        Ok(())
    }

    fn key(&mut self, key: KeyEvent) -> Result<(), String> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('r') {
            return self.toggle_raw();
        }
        let direction = match key.code {
            KeyCode::Tab | KeyCode::Down => Some(true),
            KeyCode::BackTab | KeyCode::Up => Some(false),
            KeyCode::Char('n') if ctrl => Some(true),
            KeyCode::Char('p') if ctrl => Some(false),
            _ => None,
        };
        if let Some(forward) = direction {
            let fields = self.fields();
            let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
            self.focus = fields[(i + if forward { 1 } else { fields.len() - 1 }) % fields.len()];
        } else if matches!(
            key.code,
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
        ) && self.focus == Field::Kind
        {
            self.kind = (self.kind + if key.code == KeyCode::Left { 2 } else { 1 }) % 3;
        } else if matches!(
            key.code,
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
        ) && self.focus == Field::Enabled
        {
            self.disabled = !self.disabled;
        } else {
            let focus = self.focus;
            let changed = self
                .input()
                .is_some_and(|input| matches!(input.handle_key(key), TextInputEvent::Changed));
            if changed && focus == Field::DestinationPort {
                self.linked_port = false;
            }
            if changed && focus == Field::Port && self.linked_port {
                self.destination_port.set_text(self.port.text().to_owned());
            }
        }
        Ok(())
    }
}

impl App {
    pub(super) fn selected_host_has_forwards(&self) -> bool {
        self.host_edit
            .as_ref()
            .and_then(|s| s.rows.get(s.cursor))
            .is_some_and(|r| !r.is_local && !r.is_socket && r.config().is_some())
    }

    fn forward_manager(&self) -> Option<std::sync::Arc<Manager>> {
        let row = self
            .host_edit
            .as_ref()?
            .rows
            .get(self.host_edit.as_ref()?.cursor)?;
        match self.backend_for(&row.host())? {
            Backend::Remote(remote) => remote.forwards.clone(),
            _ => None,
        }
    }

    pub(super) fn open_port_forwards(&mut self) {
        if !self.selected_host_has_forwards() {
            return;
        }
        let state = self.host_edit.as_mut().unwrap();
        if let Some(edit) = &state.edit {
            let unchanged = match &edit.origin {
                EditOrigin::Existing(original) => {
                    let row = &state.rows[state.cursor];
                    serde_json::to_value(row.config()).ok()
                        == serde_json::to_value(original.config()).ok()
                        && row.codex_endpoint.text() == original.codex_endpoint.text()
                        && row.codex == original.codex
                }
                EditOrigin::Added => false,
            };
            if !unchanged {
                state.message = Some("Save the host changes before managing port forwards".into());
                return;
            }
        }
        state.forward_view = Some(ForwardView::default());
    }

    fn apply_forward_rules(&mut self, rules: Vec<Rule>) {
        let manager = self.forward_manager();
        let state = self.host_edit.as_mut().unwrap();
        let view = state.forward_view.as_mut().unwrap();
        if let Err(error) = ssh_forward::validate_rules(&rules) {
            view.message = Some(error);
            return;
        }
        // A local listener belongs to this machine across all hosts. Remote
        // listeners collide only on the same SSH target.
        let row = &state.rows[state.cursor];
        for (i, other) in state.rows.iter().enumerate() {
            if i == state.cursor || other.disabled || other.is_local || other.is_socket {
                continue;
            }
            for rule in rules.iter().filter(|r| !r.disabled) {
                if (rule.forward.flag != "-R" || row.target.text() == other.target.text())
                    && other
                        .forwards
                        .iter()
                        .any(|r| !r.disabled && rule.forward.conflicts(&r.forward))
                {
                    view.message = Some(format!(
                        "This listener is already configured on host {}",
                        other.label.text()
                    ));
                    return;
                }
            }
        }
        let old = row.forwards.clone();
        let mut configs: Vec<_> = state.rows.iter().filter_map(|r| r.config()).collect();
        if let Some(host) = configs.iter_mut().find(|h| h.label == row.host().0) {
            host.forwards = rules.clone();
        }
        let (tx, rx) = oneshot::channel();
        if let Some(manager) = manager {
            let desired = rules.clone();
            tokio::spawn(async move {
                let result = async {
                    manager.apply(&desired).await?;
                    if let Err(error) = super::hosts::try_save_hosts(&configs) {
                        let rollback = manager.apply(&old).await;
                        return Err(format!(
                            "Could not save forwards: {error}; {}",
                            match rollback {
                                Ok(()) => "previous rules restored".into(),
                                Err(e) => format!("restoration failed: {e}"),
                            }
                        ));
                    }
                    Ok(())
                }
                .await;
                let _ = tx.send(result);
            });
        } else {
            let _ = tx.send(super::hosts::try_save_hosts(&configs).map_err(|e| e.to_string()));
        }
        view.message = None;
        view.pending = Some(Pending { rules, result: rx });
        self.poll_forward_edits();
    }

    pub(super) fn poll_forward_edits(&mut self) -> bool {
        let Some(state) = self.host_edit.as_mut() else {
            return false;
        };
        let Some(view) = state.forward_view.as_mut() else {
            return false;
        };
        let Some(pending) = &mut view.pending else {
            return false;
        };
        let result = match pending.result.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return false,
            Err(oneshot::error::TryRecvError::Closed) => {
                Err("Forwarding request stopped before its result arrived".into())
            }
        };
        let pending = view.pending.take().unwrap();
        match result {
            Ok(()) => {
                state.rows[state.cursor].forwards = pending.rules.clone();
                if let Some(edit) = &mut state.edit
                    && let EditOrigin::Existing(original) = &mut edit.origin
                {
                    original.forwards = pending.rules;
                }
                view.edit = None;
                view.import = None;
                view.remove = false;
                view.cursor = view
                    .cursor
                    .min(state.rows[state.cursor].forwards.len().saturating_sub(1));
                view.message =
                    Some("Saved; enabled rules apply while the host is connected".into());
            }
            Err(error) => {
                view.message = Some(error);
                view.remove = false;
            }
        }
        true
    }

    /// A paste is text, never list commands or an implicit Enter. Multiline
    /// port lists become whitespace-separated input for the importer.
    pub(super) fn paste_port_forward(&mut self, text: &str) {
        if self.input_mode != super::InputMode::HostEdit {
            return;
        }
        let Some(view) = self
            .host_edit
            .as_mut()
            .and_then(|s| s.forward_view.as_mut())
        else {
            return;
        };
        if view.pending.is_some() {
            return;
        }
        if view.edit.is_none() && view.import.is_none() {
            view.import = Some(TextInput::new());
        }
        for c in text.chars() {
            let c = if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else if c.is_control() {
                continue;
            } else {
                c
            };
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            if let Some(input) = &mut view.import {
                input.handle_key(key);
            } else if let Some(edit) = &mut view.edit {
                // Type/toggle controls don't accept pasted text.
                if edit.input().is_some() {
                    let _ = edit.key(key);
                }
            }
        }
    }

    pub(super) fn handle_port_forward_key(&mut self, key: KeyEvent) {
        self.poll_forward_edits();
        let state = self.host_edit.as_mut().unwrap();
        let view = state.forward_view.as_mut().unwrap();
        if view.pending.is_some() {
            return;
        }
        let rules = &state.rows[state.cursor].forwards;
        if let Some(input) = &mut view.import {
            match key.code {
                KeyCode::Esc => {
                    view.import = None;
                    view.message = None;
                }
                KeyCode::Enter => match ssh_forward::import(input.text()) {
                    Ok(imported) => {
                        let mut desired = rules.clone();
                        desired.extend(imported);
                        self.apply_forward_rules(desired);
                    }
                    Err(error) => view.message = Some(error),
                },
                _ => {
                    input.handle_key(key);
                }
            }
        } else if let Some(edit) = &mut view.edit {
            match key.code {
                KeyCode::Esc => {
                    view.edit = None;
                    view.message = None;
                }
                KeyCode::Enter => match edit.rule() {
                    Ok(rule) => {
                        let mut desired = rules.clone();
                        if let Some(i) = edit.index {
                            desired[i] = rule;
                        } else {
                            desired.push(rule);
                            view.cursor = desired.len() - 1;
                        }
                        self.apply_forward_rules(desired);
                    }
                    Err(error) => view.message = Some(error),
                },
                _ => {
                    if let Err(error) = edit.key(key) {
                        view.message = Some(error);
                    }
                }
            }
        } else if view.remove {
            view.remove = false;
            if key.code == KeyCode::Char('y') {
                let mut desired = rules.clone();
                desired.remove(view.cursor);
                self.apply_forward_rules(desired);
            }
        } else if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => state.forward_view = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    view.cursor = (view.cursor + 1).min(rules.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => view.cursor = view.cursor.saturating_sub(1),
                KeyCode::Char('a') => {
                    view.edit = Some(Draft::new(None, None));
                    view.message = None;
                }
                KeyCode::Char('i') => {
                    view.import = Some(TextInput::new());
                    view.message = None;
                }
                KeyCode::Enter | KeyCode::Char('e' | 'y') if !rules.is_empty() => {
                    let mut draft = Draft::new(
                        rules.get(view.cursor),
                        (key.code != KeyCode::Char('y')).then_some(view.cursor),
                    );
                    if draft.raw.is_some() {
                        draft.focus = Field::Raw;
                    }
                    view.edit = Some(draft);
                    view.message = None;
                }
                KeyCode::Char(' ') if !rules.is_empty() => {
                    let mut desired = rules.clone();
                    desired[view.cursor].disabled = !desired[view.cursor].disabled;
                    self.apply_forward_rules(desired);
                }
                KeyCode::Char('r') if !rules.is_empty() => {
                    let desired = rules.clone();
                    self.apply_forward_rules(desired);
                }
                KeyCode::Char('d') if !rules.is_empty() => {
                    view.remove = true;
                    view.message = None;
                }
                _ => {}
            }
        }
    }

    pub(super) fn forward_hints(&self) -> &'static str {
        let view = self
            .host_edit
            .as_ref()
            .and_then(|s| s.forward_view.as_ref())
            .unwrap();
        if view.pending.is_some() {
            "Applying forwarding changes…"
        } else if view.import.is_some() {
            "Enter Import and apply   Esc Cancel"
        } else if view.edit.is_some() {
            "Tab/↑↓ Field   ^r Raw/form   Enter Save and apply   Esc Cancel"
        } else {
            "a Add   Enter Edit   Space Toggle   y Duplicate   i Import   d Delete   r Retry   Esc Back"
        }
    }

    pub(super) fn draw_port_forwards(&self, frame: &mut ratatui::Frame, area: Rect) {
        let state = self.host_edit.as_ref().unwrap();
        let view = state.forward_view.as_ref().unwrap();
        let row = &state.rows[state.cursor];
        let popup = centered_rect(90, 80, area);
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(format!(" Port forwards · {} ", row.label.text()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let width = inner.width as usize;
        let mut lines = Vec::new();
        let mut focus_line = 0;
        if let Some(edit) = &view.edit {
            lines.push(Line::from(if edit.index.is_some() {
                "Edit port forward"
            } else {
                "Add port forward"
            }));
            lines.push(Line::from(""));
            for field in edit.fields() {
                let focused = field == edit.focus;
                if focused {
                    focus_line = lines.len();
                }
                let value_width = width.saturating_sub(24).max(1);
                let values = match field {
                    Field::Kind => vec![vec![Span::raw(format!("[{}]", KINDS[edit.kind]))]],
                    Field::Enabled => vec![vec![Span::raw(if edit.disabled {
                        "[off]"
                    } else {
                        "[on]"
                    })]],
                    _ => {
                        let input = match field {
                            Field::Name => &edit.name,
                            Field::Port => &edit.port,
                            Field::Destination => &edit.destination,
                            Field::DestinationPort => &edit.destination_port,
                            Field::Bind => &edit.bind,
                            Field::Raw => edit.raw.as_ref().unwrap(),
                            _ => unreachable!(),
                        };
                        text_field_lines(input, focused, value_width)
                    }
                };
                for (i, value) in values.into_iter().enumerate() {
                    let mut spans = vec![Span::raw(if i == 0 {
                        format!(
                            "{} {:<21}",
                            if focused { "›" } else { " " },
                            field.label(edit.kind == 1)
                        )
                    } else {
                        " ".repeat(24)
                    })];
                    spans.extend(value);
                    lines.push(Line::from(spans));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(match edit.kind {
                1 => "Remote listens; destination is reached from this machine.",
                2 => "This machine provides a SOCKS proxy through the remote host.",
                _ => "This machine listens; destination is reached from the remote host.",
            }));
            if edit.focus == Field::Bind {
                lines.push(Line::from(
                    "localhost: loopback only · *: all interfaces (remote SSH policy applies)",
                ));
            }
        } else if let Some(input) = &view.import {
            lines.push(Line::from("Import ports or SSH forwarding arguments"));
            lines.push(Line::from(
                "Examples: 3000 5173 8080   or   -L 8080:localhost:3000 -D1080",
            ));
            lines.push(Line::from(""));
            lines.extend(
                text_field_lines(input, true, width)
                    .into_iter()
                    .map(Line::from),
            );
            focus_line = lines.len().saturating_sub(1);
        } else {
            let manager = self.forward_manager();
            if row.forwards.is_empty() {
                lines.push(Line::from(
                    "No port forwards. Press a to add one or i to import ports.",
                ));
            }
            // Two lines per rule keep both endpoints readable on narrow TUI
            // windows and leave errors beside the rule that produced them.
            for (i, rule) in row.forwards.iter().enumerate() {
                if i == view.cursor {
                    focus_line = lines.len();
                }
                let status = match manager.as_ref().map(|m| m.status(&rule.forward)) {
                    Some(Status::Failed(error)) => format!(
                        "Failed{}: {error}",
                        if rule.disabled { " to disable" } else { "" }
                    ),
                    Some(Status::Waiting | Status::Listening) if rule.disabled => {
                        "Waiting to disable".into()
                    }
                    _ if rule.disabled => "Off".into(),
                    Some(Status::Listening) => "Listening".into(),
                    _ => "Waiting for host".into(),
                };
                let name = if rule.name.is_empty() {
                    rule.forward
                        .tcp()
                        .map(|tcp| {
                            format!(
                                "{} :{}",
                                match rule.forward.flag.as_str() {
                                    "-R" => "Remote",
                                    "-D" => "SOCKS",
                                    _ => "Local",
                                },
                                tcp.port
                            )
                        })
                        .unwrap_or_else(|| "Raw forward".into())
                } else {
                    rule.name.clone()
                };
                let title = format!(
                    "{} {} {}  {}",
                    if i == view.cursor { "›" } else { " " },
                    if rule.disabled { "·" } else { "✓" },
                    name,
                    status
                );
                let style = if i == view.cursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                for range in super::draw::wrap_ranges(&title, width) {
                    lines.push(Line::styled(
                        title[range.start..range.end].to_owned(),
                        style,
                    ));
                }
                let detail = format!("    {}", rule.forward.description());
                for range in super::draw::wrap_ranges(&detail, width) {
                    lines.push(Line::from(detail[range.start..range.end].to_owned()));
                }
            }
        }
        let message = if view.pending.is_some() {
            Some("Applying forwarding changes…")
        } else if view.remove {
            Some("Delete this forward? y / N")
        } else {
            view.message.as_deref()
        };
        let message_lines: Vec<Line> = message
            .map(|message| {
                super::draw::wrap_ranges(message, width)
                    .into_iter()
                    .map(|r| Line::from(message[r.start..r.end].to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let footer_height = (message_lines.len() as u16).min(inner.height.saturating_sub(2));
        let body_height = inner
            .height
            .saturating_sub(footer_height + u16::from(footer_height > 0));
        let scroll = focus_line.saturating_sub(body_height.saturating_sub(3) as usize);
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            Rect {
                height: body_height,
                ..inner
            },
        );
        if footer_height > 0 {
            frame.render_widget(
                Paragraph::new(message_lines)
                    .style(Style::default().fg(crate::config::get().colors.ui.attention_fg)),
                Rect {
                    y: inner.y + inner.height - footer_height,
                    height: footer_height,
                    ..inner
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_editor_accepts_a_new_socket_forward_without_a_dummy_tcp_port() {
        let mut draft = Draft::new(None, None);
        draft
            .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        for c in "/path/to/listen.sock:/path/to/destination.sock".chars() {
            draft
                .key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        let rule = draft.rule().unwrap();
        assert_eq!(rule.forward.flag, "-L");
        assert_eq!(
            rule.forward.spec,
            "/path/to/listen.sock:/path/to/destination.sock"
        );
        assert!(
            draft.toggle_raw().is_err(),
            "socket specs stay in the raw editor"
        );
        let mut reopened = Draft::new(Some(&rule), Some(0));
        reopened.name.set_text("Editor");
        assert_eq!(reopened.rule().unwrap().forward, rule.forward);
    }
}
