//! Direct agent and host choices over the directory and resume pickers.
//!
//! Choices stay in a draft until Enter applies them or Ctrl-o launches. Capture
//! the highlighted directory or conversation on open so background list updates
//! cannot change that target. A resume must still match its original host and
//! agent; otherwise Ctrl-o applies the filters and returns to session selection.
//! Key assignments snapshot preference order, with ten hosts per page and fixed
//! agent keys throughout. Only resume offers All (0).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};

use crate::{
    agent::{AgentControl, ResumeCandidate},
    config,
    state::HostId,
};

use super::{Action, App, InputMode, PickerKind, format::clear_overlay, format::truncate_str};

const AGENT_KEYS: &str = "qwertyuiop";
const HOST_KEYS: &str = "asdfgzxcvb";
const PAGE_SIZE: usize = HOST_KEYS.len();

#[derive(Debug)]
pub(super) struct LaunchOptions {
    agents: Vec<AgentControl>,
    hosts: Vec<HostId>,
    agent: Option<AgentControl>,
    host: HostId,
    cwd: String,
    worktree: Option<String>,
    resume: Option<ResumeSelection>,
    page: usize,
    scroll: usize,
    reveal: Option<usize>,
    error: Option<String>,
}

#[derive(Debug)]
struct ResumeSelection {
    host: HostId,
    candidate: Option<ResumeCandidate>,
    title: String,
}

impl LaunchOptions {
    fn pages(&self) -> usize {
        self.hosts.len().div_ceil(PAGE_SIZE).max(1)
    }

    fn resumable_selection(&self) -> Option<&ResumeCandidate> {
        let selected = self.resume.as_ref()?;
        selected.candidate.as_ref().filter(|candidate| {
            selected.host == self.host && self.agent.is_none_or(|agent| agent == candidate.agent)
        })
    }

    pub(super) fn action_label(&self) -> &'static str {
        if self.resume.is_none() {
            "launch"
        } else if self.resumable_selection().is_some() {
            "resume"
        } else {
            "apply filters"
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let ui = &config::get().colors.ui;
        let heading = Style::default().fg(ui.title_fg).bold();
        let dim = Style::default().dim();
        let width = area.width.saturating_sub(2).min(88);
        if width < 12 || area.height < 6 {
            return;
        }
        let controls = Paragraph::new(format!(
            "Enter keep & return   Ctrl-o {}   Esc cancel",
            self.action_label()
        ))
        .wrap(Wrap { trim: false });
        let controls_height = controls.line_count(width.saturating_sub(4)) as u16;
        let height = (17
            + controls_height
            + u16::from(self.error.is_some())
            + u16::from(self.worktree.is_some()))
        .min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.buffer_mut().set_style(area, dim);
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(Span::styled(" Agent & host ", heading));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        // Reserve controls before allocating list space. Short terminals can
        // scroll both columns without changing the ten host keys on the page.
        let navigation_height =
            u16::from(inner.height >= 6 + controls_height + u16::from(self.error.is_some()));
        let [body, error_area, navigation, controls_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(u16::from(self.error.is_some())),
            Constraint::Length(navigation_height),
            Constraint::Length(controls_height),
        ])
        .areas(inner);
        let [summary, columns] = Layout::vertical([
            Constraint::Length(
                (3 + u16::from(self.worktree.is_some())).min(body.height.saturating_sub(2)),
            ),
            Constraint::Min(2),
        ])
        .areas(body);
        let cwd = if self.cwd.is_empty() {
            "(choose a directory)"
        } else {
            &self.cwd
        };
        let mut lines = vec![
            Line::raw(truncate_str(
                &self.resume.as_ref().map_or_else(
                    || format!("Directory: {cwd}"),
                    |resume| format!("Session: {}", resume.title),
                ),
                summary.width as usize,
            )),
            Line::styled(
                truncate_str(
                    &format!(
                        "Selected: {} · {}",
                        self.agent.map_or("All", AgentControl::label),
                        self.host.0
                    ),
                    summary.width as usize,
                ),
                heading,
            ),
        ];
        if let Some(name) = &self.worktree {
            lines.push(Line::styled(
                truncate_str(
                    &format!(
                        "Worktree: {}",
                        if name.is_empty() { "auto-named" } else { name }
                    ),
                    summary.width as usize,
                ),
                dim,
            ));
        }
        frame.render_widget(Paragraph::new(lines), summary);

        let [agents, _, hosts] = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .areas(columns);
        let visible = columns.height.saturating_sub(1) as usize;
        let host_start = self.page * PAGE_SIZE;
        let host_end = (host_start + PAGE_SIZE).min(self.hosts.len());
        let rows =
            (self.agents.len() + usize::from(self.resume.is_some())).max(host_end - host_start);
        if let Some(row) = self.reveal.take() {
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll + visible {
                self.scroll = row.saturating_sub(visible.saturating_sub(1));
            }
        }
        self.scroll = self.scroll.min(rows.saturating_sub(visible));
        let entry = |key: char, label: &str, selected: bool, width: u16| {
            Line::styled(
                truncate_str(
                    &format!("[{key}] {} {label}", if selected { "●" } else { " " }),
                    width as usize,
                ),
                if selected {
                    heading.bg(ui.highlight_bg)
                } else {
                    Style::default()
                },
            )
        };
        let mut agent_rows: Vec<_> = AGENT_KEYS
            .chars()
            .zip(&self.agents)
            .map(|(key, agent)| entry(key, agent.label(), Some(*agent) == self.agent, agents.width))
            .collect();
        if self.resume.is_some() {
            agent_rows.insert(0, entry('0', "All", self.agent.is_none(), agents.width));
        }
        let host_rows: Vec<_> = HOST_KEYS
            .chars()
            .zip(&self.hosts[host_start..host_end])
            .map(|(key, host)| entry(key, &host.0, *host == self.host, hosts.width))
            .collect();
        for (area, title, rows) in [
            (agents, "Agent".to_string(), agent_rows),
            (
                hosts,
                format!(
                    "Host · {}/{}{}",
                    self.page + 1,
                    self.pages(),
                    if navigation_height == 0 && self.pages() > 1 {
                        " [/]"
                    } else {
                        ""
                    }
                ),
                host_rows,
            ),
        ] {
            let [title_area, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
            frame.render_widget(Paragraph::new(title).style(heading), title_area);
            frame.render_widget(Paragraph::new(rows).scroll((self.scroll as u16, 0)), list);
        }
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(truncate_str(error, error_area.width as usize))
                    .style(Style::default().fg(ui.error_fg)),
                error_area,
            );
        }
        let navigation_text = match (self.pages() > 1, rows > visible) {
            (true, true) => "[ previous page   ] next page   ↑↓ scroll",
            (true, false) => "[ previous page   ] next page",
            (false, true) => "↑↓ scroll",
            (false, false) => "Press a letter to select",
        };
        frame.render_widget(Paragraph::new(navigation_text).style(dim), navigation);
        frame.render_widget(controls, controls_area);
    }
}

impl App {
    pub(super) fn open_launch_options(&mut self) {
        let Some(active) = &self.picker else {
            return;
        };
        let (agent, host, cwd, worktree, resume) = match &active.kind {
            PickerKind::Workdir {
                agent,
                host,
                worktree,
                ..
            } => (
                Some(*agent),
                host.clone(),
                self.selected_workdir(),
                worktree.as_ref().map(|arm| arm.requested_name()),
                None,
            ),
            PickerKind::Resume {
                host, agent_filter, ..
            } => {
                let candidate = self.selected_resume().map(|(_, candidate)| candidate);
                let title = candidate.as_ref().map_or_else(
                    || "(choose a session)".into(),
                    |candidate| self.resume_candidate_item(host, candidate).primary,
                );
                (
                    *agent_filter,
                    host.clone(),
                    String::new(),
                    None,
                    Some(ResumeSelection {
                        host: host.clone(),
                        candidate,
                        title,
                    }),
                )
            }
            _ => return,
        };
        let hosts = self.ordered_hosts();
        let position = hosts
            .iter()
            .position(|candidate| *candidate == host)
            .unwrap_or(0);
        self.launch_options = Some(LaunchOptions {
            agents: self
                .agent_order
                .iter()
                .filter_map(|(agent, on)| on.then_some(*agent))
                .collect(),
            hosts,
            agent,
            host,
            cwd,
            worktree,
            resume,
            page: position / PAGE_SIZE,
            scroll: 0,
            reveal: Some(position % PAGE_SIZE),
            error: None,
        });
    }

    pub(super) fn handle_launch_options_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let mut options = self.launch_options.take()?;
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('o') {
            if options.resume.is_some() {
                if let Some(candidate) = options.resumable_selection().cloned() {
                    if let Some(action) = self.resume_action(options.host.clone(), candidate) {
                        self.picker = None;
                        self.input_mode = InputMode::Normal;
                        return Some(action);
                    }
                    options.error = self.status_msg.clone();
                    self.launch_options = Some(options);
                    return None;
                }
                return self.apply_resume_options(&options);
            }
            match self.launch_workdir(
                options.agent?,
                options.host.clone(),
                options.cwd.clone(),
                options.worktree.clone(),
            ) {
                Ok(action) => return Some(action),
                Err(error) => options.error = Some(error),
            }
        } else if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Esc => return None,
                KeyCode::Enter => {
                    if options.resume.is_some() {
                        return self.apply_resume_options(&options);
                    }
                    if let Some(active) = self.picker.as_mut()
                        && let PickerKind::Workdir {
                            agent,
                            host,
                            worktree,
                            pinned,
                        } = &mut active.kind
                    {
                        let host_changed = *host != options.host;
                        *agent = options.agent?;
                        *host = options.host;
                        if options.worktree.is_none() {
                            *worktree = None;
                        }
                        active.picker.error = None;
                        if host_changed {
                            if !options.cwd.is_empty() {
                                *pinned = Some(options.cwd);
                            }
                            self.reseed_workdir_for_host();
                        }
                        self.refresh_picker_status_bar();
                    }
                    return None;
                }
                KeyCode::Char('[') => {
                    options.page = options.page.saturating_sub(1);
                    options.scroll = 0;
                    options.reveal = None;
                }
                KeyCode::Char(']') => {
                    options.page = (options.page + 1).min(options.pages() - 1);
                    options.scroll = 0;
                    options.reveal = None;
                }
                KeyCode::Up => options.scroll = options.scroll.saturating_sub(1),
                KeyCode::Down => options.scroll = (options.scroll + 1).min(PAGE_SIZE - 1),
                KeyCode::Char('0') if options.resume.is_some() => {
                    options.agent = None;
                    options.reveal = Some(0);
                    options.error = None;
                }
                KeyCode::Char(c) => {
                    if let Some(i) = AGENT_KEYS.chars().position(|key| key == c)
                        && let Some(agent) = options.agents.get(i)
                    {
                        options.agent = Some(*agent);
                        if !agent.capabilities().worktrees {
                            options.worktree = None;
                        }
                        options.reveal = Some(i + usize::from(options.resume.is_some()));
                        options.error = None;
                    }
                    if let Some(i) = HOST_KEYS.chars().position(|key| key == c)
                        && let Some(host) = options.hosts.get(options.page * PAGE_SIZE + i)
                    {
                        options.host = host.clone();
                        options.reveal = Some(i);
                        options.error = None;
                    }
                }
                _ => {}
            }
        }
        self.launch_options = Some(options);
        None
    }

    fn apply_resume_options(&mut self, options: &LaunchOptions) -> Option<Action> {
        let Some(PickerKind::Resume { host, .. }) = self.picker.as_ref().map(|active| &active.kind)
        else {
            return None;
        };
        let host_changed = *host != options.host;
        self.set_resume_agent_filter(options.agent);
        host_changed.then(|| Action::SwitchResumeHost {
            host: options.host.clone(),
        })
    }

    pub(super) fn draw_launch_options(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(options) = self.launch_options.as_mut() {
            options.draw(frame, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_agent_has_a_direct_key() {
        assert!(AgentControl::ALL.len() <= AGENT_KEYS.len());
    }
}
