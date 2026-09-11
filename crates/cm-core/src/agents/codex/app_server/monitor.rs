//! Reduce protocol observations to dashboard facts. Unknown methods and fields
//! pass through the relay unchanged; this module only understands what it draws.
use crate::state::{LauncherState, SessionStatus};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) enum Observation {
    Client { id: String, method: String },
    Server(Value),
    Disconnected,
}

impl Observation {
    /// Retain only metadata request identities. Codex also starts root threads
    /// for internal features (such as recaps) on the TUI's connection; neither
    /// their replies nor their errors belong to the managed conversation.
    pub(crate) fn client(value: &Value) -> Option<Self> {
        let method = value["method"].as_str()?;
        if !matches!(
            method,
            "thread/start" | "thread/resume" | "thread/fork" | "thread/read"
        ) || background_thread(&value["params"])
        {
            return None;
        }
        Some(Self::Client {
            id: value.get("id")?.to_string(),
            method: method.into(),
        })
    }
}

fn background_thread(value: &Value) -> bool {
    // Older threads can lack this classification. Ephemeral alone is not a
    // background marker: the user can start a genuine ephemeral conversation.
    value["threadSource"]
        .as_str()
        .is_some_and(|source| source != "user")
}

#[derive(Default)]
pub(crate) struct Monitor {
    pending: HashMap<String, String>,
    waiting: HashMap<String, SessionStatus>,
    pub(crate) turn: Option<String>,
    goal_active: bool,
    idle_between_goal_turns: bool,
}

impl Monitor {
    pub(crate) fn apply(&mut self, state: &mut LauncherState, observation: Observation) {
        match observation {
            Observation::Disconnected => {
                self.pending.clear();
                self.waiting.clear();
                state.codex_connected = Some(false);
                self.idle_between_goal_turns = false;
                state.last_error =
                    Some("Codex app-server disconnected; reconnect in the Codex terminal".into());
                transition(state, SessionStatus::Starting);
            }
            Observation::Client { id, method } => {
                if self.pending.len() >= 128 {
                    self.pending.clear();
                }
                self.pending.insert(id, method);
            }
            Observation::Server(value) => {
                self.server(state, value);
                if self.goal_active && state.status == SessionStatus::Idle {
                    self.idle_between_goal_turns = true;
                    transition(state, SessionStatus::Active);
                }
            }
        }
    }

    fn server(&mut self, state: &mut LauncherState, value: Value) {
        if value.get("method").is_none() {
            let Some(method) = value
                .get("id")
                .and_then(|id| self.pending.remove(&id.to_string()))
            else {
                return;
            };
            let result = &value["result"];
            let thread = &result["thread"];
            if thread["id"].as_str().is_some()
                && thread["parentThreadId"].is_null()
                && !background_thread(thread)
                && (method != "thread/read" || thread["id"].as_str() == state.session_id.as_deref())
            {
                state.last_error = None;
                self.thread(state, thread);
                if let Some(model) = result["model"].as_str() {
                    state.model = Some(model.into());
                }
                if let Some(turns) = result["initialTurnsPage"]["data"].as_array() {
                    for turn in turns.iter().rev() {
                        if let Some(items) = turn["items"].as_array() {
                            for item in items {
                                self.item(state, item, false);
                            }
                        }
                        if turn["status"] == "inProgress" {
                            self.turn = turn["id"].as_str().map(str::to_owned);
                        }
                    }
                    status(state, &thread["status"]);
                }
            } else if let Some(error) = value["error"]["message"].as_str() {
                state.last_error = Some(error.into());
            }
            return;
        }
        let method = value["method"].as_str().unwrap_or_default();
        let params = &value["params"];
        // A connection can observe many threads. Only a successful TUI lifecycle
        // response selects the row; subagents and feature threads never take it over.
        if params["threadId"].as_str() != state.session_id.as_deref() || state.session_id.is_none()
        {
            return;
        }
        match method {
            "thread/status/changed" => {
                if params["status"]["type"] == "active" {
                    self.idle_between_goal_turns = false;
                }
                status(state, &params["status"]);
            }
            "thread/name/updated" => {
                state.name = params["threadName"]
                    .as_str()
                    .or_else(|| params["name"].as_str())
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
            }
            "thread/goal/updated" | "thread/goal/cleared" => {
                self.goal_active = params["goal"]["status"] == "active";
                if !self.goal_active && self.idle_between_goal_turns {
                    self.idle_between_goal_turns = false;
                    transition(state, SessionStatus::Idle);
                }
            }
            "thread/settings/updated" => {
                if let Some(model) = params["threadSettings"]["model"].as_str() {
                    state.model = Some(model.into());
                }
            }
            "thread/tokenUsage/updated" => {
                state.context_tokens = params["tokenUsage"]["last"]["totalTokens"].as_u64();
                state.context_window = params["tokenUsage"]["modelContextWindow"].as_u64();
            }
            "turn/started" => {
                self.idle_between_goal_turns = false;
                self.turn = params["turn"]["id"].as_str().map(str::to_owned);
                state.last_error = None;
                transition(state, SessionStatus::Active);
            }
            "turn/completed" => {
                self.waiting.clear();
                self.turn = None;
                if let Some(error) = params["turn"]["error"]["message"].as_str() {
                    state.last_error = Some(error.into());
                }
                transition(state, SessionStatus::Idle);
            }
            "item/started" | "item/completed" => {
                self.item(state, &params["item"], method == "item/started")
            }
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                self.waiting_request(state, value.get("id"), SessionStatus::WaitingForDecision)
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval" => {
                self.waiting_request(state, value.get("id"), SessionStatus::WaitingForApproval)
            }
            "serverRequest/resolved" => {
                self.waiting.remove(&params["requestId"].to_string());
                if matches!(
                    state.status,
                    SessionStatus::WaitingForApproval | SessionStatus::WaitingForDecision
                ) {
                    transition(
                        state,
                        self.waiting_status().unwrap_or(SessionStatus::Active),
                    );
                }
            }
            "thread/closed" => {
                state.codex_connected = Some(false);
                transition(state, SessionStatus::Starting);
            }
            "error" => {
                if let Some(error) = params["error"]["message"].as_str() {
                    state.last_error = Some(error.into());
                }
            }
            _ => {}
        }
    }

    fn waiting_status(&self) -> Option<SessionStatus> {
        if self
            .waiting
            .values()
            .any(|s| *s == SessionStatus::WaitingForDecision)
        {
            Some(SessionStatus::WaitingForDecision)
        } else if !self.waiting.is_empty() {
            Some(SessionStatus::WaitingForApproval)
        } else {
            None
        }
    }

    fn waiting_request(
        &mut self,
        state: &mut LauncherState,
        id: Option<&Value>,
        status: SessionStatus,
    ) {
        if let Some(id) = id {
            self.waiting.insert(id.to_string(), status.clone());
        }
        transition(state, self.waiting_status().unwrap_or(status));
    }

    fn thread(&mut self, state: &mut LauncherState, thread: &Value) {
        let id = thread["id"].as_str().unwrap();
        if state.session_id.as_deref() != Some(id) {
            self.waiting.clear();
            self.turn = None;
            self.goal_active = false;
            self.idle_between_goal_turns = false;
            state.context_tokens = None;
            state.context_window = None;
            state.last_prompt = None;
            state.last_tool = None;
        }
        state.session_id = Some(id.into());
        state.codex_connected = Some(true);
        state.name = thread["name"].as_str().map(str::to_owned);
        state.first_prompt = thread["preview"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        if let Some(cwd) = thread["cwd"].as_str() {
            state.cwd = cwd.into();
        }
        if let Some(model) = thread["model"].as_str() {
            state.model = Some(model.into());
        }
        if let Some(turns) = thread["turns"].as_array() {
            for turn in turns {
                if let Some(items) = turn["items"].as_array() {
                    for item in items {
                        self.item(state, item, false);
                    }
                }
                if turn["status"] == "inProgress" {
                    self.turn = turn["id"].as_str().map(str::to_owned);
                }
            }
        }
        status(state, &thread["status"]);
    }

    fn item(&self, state: &mut LauncherState, item: &Value, started: bool) {
        if !started {
            if let Some(error) = item["error"]["message"]
                .as_str()
                .or_else(|| item["error"].as_str())
            {
                state.last_error = Some(error.into());
            } else if let Some(code) = item["exitCode"].as_i64().filter(|code| *code != 0) {
                state.last_error = Some(format!("Command exited with status {code}"));
            }
        }
        let kind = item["type"].as_str().unwrap_or_default();
        match kind {
            "userMessage" => {
                let text = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    state.first_prompt.get_or_insert_with(|| text.clone());
                    state.last_prompt = Some(text);
                }
            }
            "contextCompaction" => transition(
                state,
                if started {
                    SessionStatus::Compacting
                } else {
                    SessionStatus::Compacted
                },
            ),
            "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "webSearch"
            | "imageView"
            | "imageGeneration"
            | "collabAgentToolCall" => {
                state.last_tool = Some(item["tool"].as_str().unwrap_or(kind).into());
                if started
                    && !matches!(
                        state.status,
                        SessionStatus::WaitingForApproval | SessionStatus::WaitingForDecision
                    )
                {
                    transition(state, SessionStatus::Active);
                }
            }
            _ => {}
        }
    }
}

fn status(state: &mut LauncherState, value: &Value) {
    let next = match value["type"].as_str() {
        Some("idle") => SessionStatus::Idle,
        Some("active") => {
            let flags = value["activeFlags"].as_array();
            if flags.is_some_and(|f| f.iter().any(|v| v == "waitingOnUserInput")) {
                SessionStatus::WaitingForDecision
            } else if flags.is_some_and(|f| f.iter().any(|v| v == "waitingOnApproval")) {
                SessionStatus::WaitingForApproval
            } else {
                SessionStatus::Active
            }
        }
        Some("notLoaded") => {
            state.codex_connected = Some(false);
            SessionStatus::Starting
        }
        Some("systemError") => {
            state.last_error = Some("Codex app-server reported a thread error".into());
            SessionStatus::Starting
        }
        _ => return,
    };
    transition(state, next);
}

fn transition(state: &mut LauncherState, next: SessionStatus) {
    if state.status != next {
        state.active_since = if next.is_busy() {
            state.active_since.or_else(|| Some(LauncherState::now()))
        } else {
            None
        };
        state.status = next;
    }
}
