//! The local, in-process backend and the shared open-session types.
//!
//! [`LocalBackend`] is the **server-core**: it reads this host's session state
//! files, lists resumable sessions, signals local agent processes, plans launch
//! argv, and answers the host-filesystem queries the workdir picker needs. Both
//! the dashboard's localhost row *and* the `miao-server` daemon wrap one,
//! so the same local-read logic backs the in-process path and the remote path.
//!
//! [`OpenSpec`] (what to open) and [`LaunchPlan`] (how the client attaches a
//! window to it) are the seam types the dashboard's `Backend` enum and the wire
//! protocol share; they live here so both crates agree on them. The dashboard's
//! `Backend`/`RemoteBackend` (the ssh client) build on these — see the
//! `backend` module in the `captain-miao` crate.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::agent::{AgentControl, ResumeCandidate, SessionIndex, SessionIndexCache};
use crate::agents::codex;
use crate::paths;
use crate::state::{self, LauncherState, SessionFlags, SessionKey};

/// What to open: which agent, where, and whether it's a fresh session or a
/// resume/fork of an existing one. This is §3/§14.2's `SpawnSpec`, renamed to
/// avoid colliding with `terminal::SpawnSpec` (which describes the *window*).
/// [`LocalBackend::open_session`] turns it into a [`LaunchPlan`]. Serializable
/// because it rides the wire to a remote server (`ClientFrame::OpenSession`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSpec {
    pub agent: AgentControl,
    pub cwd: String,
    /// `Some((session_id, fork))` to resume an existing session (`fork` continues
    /// it on a branch instead of in place); `None` for a brand-new session.
    pub resume: Option<(String, bool)>,
    /// `Some(name)` launches the session in an isolated git worktree; an empty
    /// name lets the agent generate one. `None` is an ordinary launch.
    ///
    /// `#[serde(default)]` because this rides the wire: an older server decodes
    /// a newer client's spec by skipping the field, and a newer server decodes
    /// an older client's by defaulting it — the additive rule that keeps v4 the
    /// last refusing protocol bump.
    #[serde(default)]
    pub worktree: Option<String>,
}

/// How the client attaches a *local* window to a session `open_session` made
/// exist. The client only ever does the window half — `Terminal::spawn(argv)` —
/// so the backend hands back the argv.
pub enum LaunchPlan {
    /// Local: the window *is* the launcher. Spawn this argv directly.
    SpawnLocal { argv: Vec<String> },
    /// Remote: the server already started the launcher inside the pty pool; the
    /// client spawns a window that attaches to it (`ssh -t <host>
    /// miao-server attach <name>`, or a direct `miao-server
    /// attach <name>` for a same-host
    /// socket transport). `session_name` is the pool join key the client records
    /// against the local window (§8 binding).
    AttachRemote {
        argv: Vec<String>,
        #[allow(dead_code)] // recorded as the window↔session binding key in 3d
        session_name: String,
    },
}

impl LaunchPlan {
    /// The argv the client spawns into a local Kitty window.
    pub fn argv(&self) -> &[String] {
        match self {
            LaunchPlan::SpawnLocal { argv } => argv,
            LaunchPlan::AttachRemote { argv, .. } => argv,
        }
    }
}

/// Floor between sqlite re-reads of the Codex title store once a change is
/// detected. Renames are rare and low-stakes, so the overlay trades freshness
/// for quiet: a title change surfaces on the first overlay pass at least this
/// long after the previous read. First sight of a new session id bypasses the
/// floor (one immediate read titles a fresh/resumed session).
const CODEX_TITLE_REFRESH_FLOOR: Duration = Duration::from_secs(30);

/// The per-host Codex title cache behind [`LocalBackend`]'s overlay. Exactly
/// one exists per host process (the dashboard's local backend, or the daemon's
/// server-core shared across every connection), so there is a **single sqlite
/// reader per host** no matter how many Codex sessions run.
#[derive(Default)]
struct CodexTitles {
    /// session id → title at the last read. `None` = queried, no usable title
    /// row yet — the entry still marks the id *known*, so an untitled session
    /// doesn't re-trigger the first-sight read on every overlay pass.
    titles: HashMap<String, Option<String>>,
    /// `(db, wal)` mtimes at the last read — the cheap change gate: an
    /// unchanged stamp means no title can have moved, so sqlite isn't touched.
    store_stamp: (Option<SystemTime>, Option<SystemTime>),
    last_read: Option<Instant>,
}

/// Whether the overlay should hit sqlite this pass. Pure so the throttle rules
/// are pinned by tests: a never-seen session id reads immediately (first-load
/// titling); otherwise a read requires both a store change (the mtime stamp
/// moved) and the refresh floor elapsed since the last read — so even a
/// wal-churning Codex burst costs at most one small read-only query batch per
/// floor interval per host.
fn title_refresh_due(unknown_id: bool, store_changed: bool, since_read: Option<Duration>) -> bool {
    if unknown_id {
        return true;
    }
    store_changed && since_read.is_none_or(|d| d >= CODEX_TITLE_REFRESH_FLOOR)
}

/// Stamp cached titles onto the Codex rows (matched by session id), leaving
/// every other row — and untitled Codex rows — untouched so the display falls
/// through to the folded first prompt. Split from the cache upkeep for tests.
fn stamp_titles(sessions: &mut [LauncherState], titles: &HashMap<String, Option<String>>) {
    for s in sessions
        .iter_mut()
        .filter(|s| s.agent == AgentControl::Codex)
    {
        if let Some(title) = s
            .session_id
            .as_ref()
            .and_then(|id| titles.get(id))
            .and_then(|t| t.as_ref())
        {
            s.name = Some(title.clone());
        }
    }
}

/// In-process backend: read the local filesystem and signal local processes
/// directly. Owns the per-agent session-name cache and the per-host Codex
/// title overlay (per-host state that a remote backend keeps on its own
/// server). Also the server-core (see module docs), so `miao-server`
/// calls the same operations.
#[derive(Default)]
pub struct LocalBackend {
    session_index_caches: HashMap<AgentControl, SessionIndexCache>,
    codex_titles: Mutex<CodexTitles>,
    /// This host's `$HOME`, resolved once. Every path the backend returns is
    /// collapsed against it and every path it receives is expanded against it,
    /// so the seam speaks one host-canonical spelling (§3) and the caller never
    /// learns the home.
    home: String,
    /// Whether this backend is acting as a **server-core** — the daemon's, or a
    /// pooled-localhost one — in which case it also owns the per-session flags
    /// sidecar and the pool's attached bit, overlaying both onto the rows it
    /// serves. A plain local dashboard leaves both off: its flags live in
    /// `dashboard-overrides.json`, and it has no pool.
    serve_host_state: bool,
    /// Reads libshpool's live session list for the attached-bit overlay.
    /// Injected so cm-core stays free of libshpool (only the server links it).
    #[allow(clippy::type_complexity)]
    attached_probe: Option<Box<dyn Fn() -> HashMap<String, bool> + Send + Sync>>,
}

impl LocalBackend {
    /// A backend for the in-process dashboard: reads and signals, no host-owned
    /// state served to anyone else.
    pub fn new() -> Self {
        Self {
            home: paths::host_home(),
            ..Default::default()
        }
    }

    /// A backend acting as the **server-core** — the daemon's, or the one a
    /// pooled-localhost dashboard reaches over a socket. On top of the reads it
    /// owns the per-session flags sidecar and (given a probe) overlays the
    /// pool's attached bit, so every dashboard watching this host agrees.
    pub fn server_core() -> Self {
        Self {
            home: paths::host_home(),
            serve_host_state: true,
            ..Default::default()
        }
    }

    /// Supply the pool's attached-bit reader (the server's libshpool `List`).
    /// Without it `LauncherState.attached` stays `None` — "unknown", which the
    /// UI treats as "don't offer a steal".
    pub fn with_attached_probe(
        mut self,
        probe: impl Fn() -> HashMap<String, bool> + Send + Sync + 'static,
    ) -> Self {
        self.attached_probe = Some(Box::new(probe));
        self
    }

    /// This host's `$HOME`. Only the backend's own boundary conversions and the
    /// server's argv construction should need it — it never crosses the seam.
    pub fn home(&self) -> &str {
        &self.home
    }

    /// A backend with an injected home, so the canonical-path boundary can be
    /// exercised without depending on the test runner's `$HOME`.
    #[cfg(test)]
    fn with_home(home: &str) -> Self {
        Self {
            home: home.to_string(),
            ..Default::default()
        }
    }

    pub fn list_sessions(&self) -> Vec<LauncherState> {
        let mut sessions = state::read_all_launcher_states();
        self.overlay_codex_titles(&mut sessions);
        // Every path leaving the backend is host-canonical, so the client can
        // display it verbatim and hand it straight back (§3).
        for s in sessions.iter_mut() {
            s.cwd = paths::collapse_home(&s.cwd, &self.home);
        }
        if self.serve_host_state {
            self.overlay_host_state(&mut sessions);
        }
        sessions
    }

    /// Stamp the host-owned per-session state onto the rows being served: the
    /// flags sidecar (so every dashboard sees the same pins/bells) and the
    /// pool's live attached bit (so the UI knows whether a steal even applies).
    fn overlay_host_state(&self, sessions: &mut [LauncherState]) {
        let flags = read_session_flags();
        let attached = self.attached_probe.as_ref().map(|p| p());
        for s in sessions.iter_mut() {
            if let Some(f) = flags.get(&s.key()) {
                s.flags = Some(*f);
            }
            if let (Some(map), Some(pool)) = (&attached, s.pool_session.as_deref()) {
                s.attached = map.get(pool).copied();
            }
        }
    }

    /// Record this host's flags for a session. Server-core only — a plain local
    /// dashboard persists its own overrides instead. Returns whether it stuck.
    pub fn set_session_flags(&self, key: &SessionKey, flags: SessionFlags) -> bool {
        if !self.serve_host_state {
            return false;
        }
        let mut all = read_session_flags();
        if flags.is_clear() {
            all.remove(key);
        } else {
            all.insert(key.clone(), flags);
        }
        // Garbage-collect entries whose session is gone, so the sidecar can't
        // grow without bound across a host's lifetime.
        let live: std::collections::HashSet<SessionKey> = state::read_all_launcher_states()
            .iter()
            .map(|s| s.key())
            .collect();
        all.retain(|k, _| live.contains(k) || k == key);
        state::write_json_atomic(&state::session_flags_path(), &all).is_ok()
    }

    /// Overlay Codex display names from the host's single title cache onto the
    /// rows. Codex stores titles (renames *and* its own auto-titles) in
    /// `state_5.sqlite` only — no hook, no rollout line — so this per-host
    /// reader stamps them onto `name` before the sessions are served, which
    /// reaches remote rows for free (the daemon's `LocalBackend` overlays
    /// before `Snapshot`/`Delta`). Heavily throttled: no Codex sessions → no
    /// reads at all; otherwise one batched read-only query, gated by the store
    /// mtime stamp and [`CODEX_TITLE_REFRESH_FLOOR`] (see [`title_refresh_due`]).
    fn overlay_codex_titles(&self, sessions: &mut [LauncherState]) {
        let ids: Vec<String> = sessions
            .iter()
            .filter(|s| s.agent == AgentControl::Codex)
            .filter_map(|s| s.session_id.clone())
            .collect();
        let mut cache = self.codex_titles.lock().unwrap();
        if ids.is_empty() {
            cache.titles.clear();
            return;
        }
        let unknown = ids.iter().any(|id| !cache.titles.contains_key(id));
        let stamp = codex::title_store_mtimes();
        if title_refresh_due(
            unknown,
            stamp != cache.store_stamp,
            cache.last_read.map(|t| t.elapsed()),
        ) {
            let read = codex::read_thread_titles(&ids);
            // Rebuild from the live ids: dead sessions fall out, and every live
            // id becomes *known* (`None` when untitled) so it doesn't re-trigger
            // the first-sight read.
            cache.titles = ids
                .iter()
                .map(|id| (id.clone(), read.get(id).cloned()))
                .collect();
            cache.store_stamp = stamp;
            cache.last_read = Some(Instant::now());
        } else {
            cache.titles.retain(|id, _| ids.contains(id));
        }
        stamp_titles(sessions, &cache.titles);
    }

    pub fn session_index(&mut self) -> SessionIndex {
        let mut merged = SessionIndex::default();
        for &agent in AgentControl::ALL {
            let cache = self.session_index_caches.entry(agent).or_default();
            let shard = agent.read_session_index(cache);
            // Tag every pid this shard contributes with its owning backend so
            // `lookup`'s by_pid fallback can be gated on the row's backend (a
            // recycled pid must not surface another backend's stale name).
            for &pid in shard.by_pid.keys() {
                merged.by_pid_owner.insert(pid, agent);
            }
            merged.by_pid.extend(shard.by_pid);
            merged.by_session_id.extend(shard.by_session_id);
            merged.session_id_by_pid.extend(shard.session_id_by_pid);
        }
        merged
    }

    pub fn list_resumable(&self, limit: usize) -> (Vec<ResumeCandidate>, Vec<String>) {
        let mut all: Vec<ResumeCandidate> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for &agent in AgentControl::ALL {
            match agent.list_resumable(limit) {
                Ok(list) => all.extend(list),
                Err(e) => errors.push(format!("{agent:?}: {e}")),
            }
        }
        all.sort_by_key(|b| std::cmp::Reverse(b.mtime));
        all.truncate(limit);
        // The cwds come straight off the transcripts, so collapse them like
        // every other path the backend returns (§3). Without this a resume
        // candidate would be the one path the client sees in a *different*
        // spelling from the running rows beside it — and, worse, a remote
        // candidate's absolute path would get collapsed against the *client's*
        // home for display, which is meaningless.
        for c in all.iter_mut() {
            c.cwd = paths::collapse_home(&c.cwd, &self.home);
        }
        (all, errors)
    }

    /// Tear a session down, resolving `key` → the **current** agent pid from
    /// the live state file first. That re-resolution is the point: the pid a
    /// caller's mirror holds may name a session that has already exited, and
    /// the OS recycles pids — so signalling the pid off the wire could SIGTERM
    /// an unrelated process. An unknown key is refused rather than guessed at.
    ///
    /// Falls back to the launcher pid when the row carries no `child_pid` (a
    /// `FailedToStart` launcher holding its error), matching the client's own
    /// kill target.
    pub fn kill_session(&self, key: &SessionKey) -> bool {
        let Some(state) = state::read_all_launcher_states()
            .into_iter()
            .find(|s| &s.key() == key)
        else {
            tracing::warn!(
                target: "captain_miao::backend",
                "kill refused: no live session for key {key}"
            );
            return false;
        };
        let pid = state.child_pid.unwrap_or(state.launcher_pid);
        unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
    }

    /// Build the argv for a local launcher window:
    /// `miao <agent> <cwd> [resume args]`. Pure metadata — the client spawns it
    /// into a Kitty window; nothing runs here. argv[0] is the running
    /// captain-miao exe so the launched launcher is the same build as the
    /// dashboard (falls back to a bare `miao` on PATH if unresolvable — the
    /// dashboard binary's name, since that is what a local install puts there).
    ///
    /// Fails only on [`AgentControl::Unknown`], and that is the whole reason it
    /// returns a `Result`: the spec crosses the wire, so a **newer dashboard**
    /// can hand an older server a backend this build has never heard of. The
    /// tolerant decode (see [`crate::agent`]) makes that a live `Unknown`
    /// instead of a refused frame, and `cli_subcommand()` is `""` for it — so
    /// without the guard the argv would be `miao "" <cwd>`, a window that opens
    /// and dies on a clap error. Refusing here rather than at each caller keeps
    /// the check somewhere a future caller cannot forget it, and says the same
    /// thing [`AgentControl::build_launch_command`] does for the launcher's own
    /// side of the same problem.
    pub fn open_session(&self, spec: &OpenSpec) -> anyhow::Result<LaunchPlan> {
        if spec.agent == AgentControl::Unknown {
            anyhow::bail!(crate::agent::UNKNOWN_AGENT_REFUSAL);
        }
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| "miao".into())
            .to_string_lossy()
            .into_owned();
        // The spec's cwd arrives host-canonical; a process needs the real path.
        let cwd = paths::expand_home(&spec.cwd, &self.home);
        let mut argv = vec![exe, spec.agent.cli_subcommand().to_string(), cwd];
        if let Some((session_id, fork)) = &spec.resume {
            argv.extend(spec.agent.resume_args(session_id, *fork));
        }
        // A worktree request the agent can't honour is dropped rather than
        // failing the launch: `worktree_args` is the single authority on which
        // agents have the concept, and the dashboard already hides the toggle
        // for the rest, so reaching here with `Some` means the two disagreed.
        // Losing the isolation is the recoverable half of that; refusing to open
        // a session the user asked for is not.
        if let Some(name) = &spec.worktree
            && let Some(args) = spec.agent.worktree_args(Some(name))
        {
            argv.extend(args);
        }
        Ok(LaunchPlan::SpawnLocal { argv })
    }

    // --- Host filesystem queries (server-core; also answered over the wire) ---
    //
    // These let the workdir picker operate against *this* host: its recent dirs,
    // directory completion, and submit-time validation. Local runs them in
    // process; a remote dashboard reaches the same code via the server
    // (`ListRecentDirs`/`CompletePath`/`CheckDir`). Every one of them expands a
    // host-canonical argument on the way in and collapses its results on the
    // way out, so the two arms are indistinguishable to the picker (§3).

    /// This host's recent working dirs, most-recent first, host-canonical.
    pub fn recent_dirs(&self) -> Vec<String> {
        read_recent_dirs()
            .into_iter()
            .map(|c| paths::collapse_home(&c, &self.home))
            .collect()
    }

    /// Directory completions for `prefix` on this host's filesystem.
    pub fn complete_path(&self, prefix: &str) -> Vec<String> {
        complete_dirs(&paths::expand_home(prefix, &self.home))
            .into_iter()
            .map(|p| paths::collapse_home(&p, &self.home))
            .collect()
    }

    /// Whether `path` is a directory on this host — the picker's submit check.
    pub fn dir_exists(&self, path: &str) -> bool {
        let path = paths::expand_home(path, &self.home);
        !path.is_empty() && Path::new(&path).is_dir()
    }

    /// Record `cwd` into this host's recent-dirs list. The server calls this when
    /// it opens a pool session, so remote launches build up the remote list the
    /// picker then serves back.
    pub fn record_recent_cwd(&self, cwd: &str) {
        record_recent_dir(&paths::collapse_home(cwd, &self.home));
    }
}

/// The recent-dirs list from `recent_cwds_path`, most-recent first. Stored
/// host-canonical, so the same repo path shares an entry (and, client-side, a
/// directory mark) no matter which machine's home it sits under; a legacy
/// absolute entry is collapsed by the caller on the way out.
fn read_recent_dirs() -> Vec<String> {
    state::read_json::<state::RecentCwds>(&state::recent_cwds_path())
        .map(|r| r.cwds)
        .unwrap_or_default()
}

/// Push `cwd` onto the recent-dirs list (dedup, most-recent first, capped),
/// persisting it. Mirrors the dashboard's `push_recent_cwd` so a locally- and a
/// remotely-launched dir age out the same way.
fn record_recent_dir(cwd: &str) {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return;
    }
    let mut cwds = read_recent_dirs();
    cwds.retain(|c| c.trim_end_matches('/') != cwd);
    cwds.insert(0, cwd.to_string());
    let max = crate::config::get().launcher.max_recent_cwds;
    cwds.truncate(max);
    let _ = state::write_json_atomic(&state::recent_cwds_path(), &state::RecentCwds { cwds });
}

/// The host's per-session flags sidecar. Missing/unreadable → empty, so a
/// deleted file just resets flags rather than failing anything.
fn read_session_flags() -> HashMap<SessionKey, SessionFlags> {
    state::read_json(&state::session_flags_path()).unwrap_or_default()
}

/// Directory completions for `prefix` on the local filesystem, as absolute paths
/// with a trailing `/`, sorted. The completion rules the picker relies on: dirs
/// only, skip dotfiles, prefix-match the basename. Returns absolute paths (not
/// `~`-collapsed) so the caller can render them against whichever host's home.
fn complete_dirs(prefix: &str) -> Vec<String> {
    let (parent, base) = split_for_completion(prefix);
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return Vec::new();
    };
    let mut matches: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with(&base) && !name.starts_with('.'))
        .map(|name| {
            let sep = if parent.ends_with('/') { "" } else { "/" };
            format!("{parent}{sep}{name}/")
        })
        .collect();
    matches.sort();
    matches
}

/// Split a path into `(parent, basename-prefix)` for completion: `/a/b`→(`/a`,
/// `b`), `/a/b/`→(`/a/b`,``), `foo`→(`.`,`foo`), ``→(`.`,``).
fn split_for_completion(path: &str) -> (String, String) {
    if path.is_empty() {
        return (".".into(), String::new());
    }
    match path.rfind('/') {
        Some(0) => ("/".into(), path[1..].to_string()),
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        None => (".".into(), path.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{HostId, SessionStatus};

    #[test]
    fn title_refresh_policy_throttles_heavily() {
        // First sight of a session id reads immediately — floor or not — so a
        // fresh/resumed session titles on its first overlay pass.
        assert!(title_refresh_due(true, false, Some(Duration::ZERO)));
        // Store changed + floor elapsed → read.
        assert!(title_refresh_due(
            false,
            true,
            Some(CODEX_TITLE_REFRESH_FLOOR)
        ));
        // Store changed but within the floor → hold (the heavy throttle: a
        // wal-churning burst costs at most one read per floor interval).
        assert!(!title_refresh_due(false, true, Some(Duration::ZERO)));
        // Stamp unchanged → never read, no matter how long it's been.
        assert!(!title_refresh_due(
            false,
            false,
            Some(Duration::from_secs(9999))
        ));
        // Never read yet: the floor can't block the very first change-read.
        assert!(title_refresh_due(false, true, None));
    }

    fn codex_state(session_id: Option<&str>) -> LauncherState {
        LauncherState {
            agent: AgentControl::Codex,
            launcher_pid: 1,
            session_id: session_id.map(str::to_string),
            window_id: None,
            tab_id: None,
            cwd: String::new(),
            status: SessionStatus::Idle,
            last_tool: None,
            updated_at: 0,
            active_since: None,
            last_prompt: None,
            child_pid: None,
            last_error: None,
            context_tokens: None,
            model: None,
            name: None,
            first_prompt: Some("first prompt".into()),
            pool_session: None,
            launch_id: None,
            terminal: None,
            terminfo: None,
            flags: None,
            attached: None,
            host: HostId::local(),
        }
    }

    #[test]
    fn stamp_titles_names_only_matching_codex_rows() {
        let mut titles: HashMap<String, Option<String>> = HashMap::new();
        titles.insert("sid-titled".into(), Some("My Rename".into()));
        titles.insert("sid-untitled".into(), None); // known but no title row yet

        let mut claude = codex_state(Some("sid-titled"));
        claude.agent = AgentControl::Claude;
        let mut sessions = vec![
            codex_state(Some("sid-titled")),
            codex_state(Some("sid-untitled")),
            codex_state(None),
            claude,
        ];
        stamp_titles(&mut sessions, &titles);
        // Titled Codex row gets the sqlite title on `name`.
        assert_eq!(sessions[0].name.as_deref(), Some("My Rename"));
        // Untitled / id-less Codex rows keep `name` empty → first-prompt shows.
        assert_eq!(sessions[1].name, None);
        assert_eq!(sessions[2].name, None);
        // A Claude row never takes a Codex title, even with a colliding id.
        assert_eq!(sessions[3].name, None);
    }

    /// `open_session` argv[0] is the resolved exe path (varies by environment),
    /// so the tests assert on the agent-facing tail.
    fn open_argv(agent: AgentControl, resume: Option<(&str, bool)>) -> Vec<String> {
        open_argv_worktree(agent, resume, None)
    }

    fn open_argv_worktree(
        agent: AgentControl,
        resume: Option<(&str, bool)>,
        worktree: Option<&str>,
    ) -> Vec<String> {
        let plan = LocalBackend::default()
            .open_session(&OpenSpec {
                agent,
                cwd: "/work".to_string(),
                resume: resume.map(|(id, fork)| (id.to_string(), fork)),
                worktree: worktree.map(str::to_string),
            })
            .expect("a known agent always plans");
        plan.argv()[1..].to_vec()
    }

    /// Also the happy-path guard for the `Result` signature: a known agent must
    /// still plan its exact argv, not merely "not fail".
    #[test]
    fn open_session_new_session_argv() {
        assert_eq!(open_argv(AgentControl::Claude, None), ["claude", "/work"]);
        assert_eq!(open_argv(AgentControl::Codex, None), ["codex", "/work"]);
        // The cwd is *our* positional in every case — `miao <agent> <cwd>` — and
        // stays one for Reasonix even though the agent itself takes `--dir`,
        // because translating it is `build_launch_command`'s job (and must be:
        // `reasonix`'s own positional is a prompt).
        assert_eq!(
            open_argv(AgentControl::Reasonix, None),
            ["reasonix", "/work"]
        );
        // Kimi takes no directory argument at all (`build_launch_command` sets
        // the process cwd instead), so ours is the only positional here too.
        assert_eq!(open_argv(AgentControl::Kimi, None), ["kimi", "/work"]);
        // Grok takes no directory argument of its own either — `build_launch_command`
        // sets the process cwd and passes nothing positional, because a bare
        // `--worktree` would otherwise swallow it.
        assert_eq!(open_argv(AgentControl::Grok, None), ["grok", "/work"]);
        // opencode's cwd is *our* positional here and becomes `--dir <path>`
        // inside `build_launch_command`, the same split Reasonix has: what
        // `opencode`'s own positional means is undocumented, so translating it
        // is the backend module's job and never this argv's.
        assert_eq!(
            open_argv(AgentControl::OpenCode, None),
            ["opencode", "/work"]
        );
    }

    /// A spec crosses the wire, so a **newer dashboard** can name a backend this
    /// build has never heard of; the tolerant decode makes that a live `Unknown`
    /// rather than a refused frame (see [`crate::agent`]). `cli_subcommand()` is
    /// `""` for it, so an unguarded plan would be `miao "" /work` — a window
    /// that opens and dies on a clap error instead of an honest refusal.
    #[test]
    fn open_session_refuses_an_unknown_agent() {
        let plan = LocalBackend::default().open_session(&OpenSpec {
            agent: AgentControl::Unknown,
            cwd: "/work".to_string(),
            resume: None,
            worktree: None,
        });
        let err = match plan {
            Ok(p) => panic!(
                "an unknown backend must never yield an argv: {:?}",
                p.argv()
            ),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("upgrade"),
            "the refusal must name the fix: {err}"
        );
    }

    #[test]
    fn open_session_worktree_argv() {
        // An empty name asks the agent to generate one, so no name is passed.
        assert_eq!(
            open_argv_worktree(AgentControl::Claude, None, Some("")),
            ["claude", "/work", "--worktree"]
        );
        assert_eq!(
            open_argv_worktree(AgentControl::Claude, None, Some("feature-auth")),
            ["claude", "/work", "--worktree", "feature-auth"]
        );
        // A `#N` name is a PR to branch from, and must reach the agent intact.
        assert_eq!(
            open_argv_worktree(AgentControl::Claude, None, Some("#1234")),
            ["claude", "/work", "--worktree", "#1234"]
        );
        // Codex has no worktree flag: the request is dropped, not turned into
        // an argument it would reject (which would fail the whole launch).
        assert_eq!(
            open_argv_worktree(AgentControl::Codex, None, Some("feature-auth")),
            ["codex", "/work"]
        );
        assert_eq!(
            open_argv_worktree(AgentControl::Reasonix, None, Some("feature-auth")),
            ["reasonix", "/work"]
        );
        assert_eq!(
            open_argv_worktree(AgentControl::Kimi, None, Some("feature-auth")),
            ["kimi", "/work"]
        );
        // Grok's flag takes its value with `=`, in **one** argv element: its
        // worktree name is optional, so the separated form would let any later
        // positional be read as the name.
        assert_eq!(
            open_argv_worktree(AgentControl::Grok, None, Some("feature-auth")),
            ["grok", "/work", "--worktree=feature-auth"]
        );
        // With no name to pass there is no value to attach, and nothing follows
        // the flag in our argv, so the bare form is both safe and the only way to
        // ask Grok to mint the name.
        assert_eq!(
            open_argv_worktree(AgentControl::Grok, None, Some("")),
            ["grok", "/work", "--worktree"]
        );
        // opencode's plugin context has a `worktree` field, but no flag
        // launches into one, so the request is dropped like Codex's.
        assert_eq!(
            open_argv_worktree(AgentControl::OpenCode, None, Some("feature-auth")),
            ["opencode", "/work"]
        );
    }

    #[test]
    fn open_session_resume_and_fork_argv() {
        // Claude resumes/forks with flags on the same subcommand.
        assert_eq!(
            open_argv(AgentControl::Claude, Some(("s1", false))),
            ["claude", "/work", "--resume", "s1"]
        );
        assert_eq!(
            open_argv(AgentControl::Claude, Some(("s1", true))),
            ["claude", "/work", "--resume", "s1", "--fork-session"]
        );
        // Codex uses a `resume` / `fork` subcommand instead.
        assert_eq!(
            open_argv(AgentControl::Codex, Some(("s2", false))),
            ["codex", "/work", "resume", "s2"]
        );
        assert_eq!(
            open_argv(AgentControl::Codex, Some(("s2", true))),
            ["codex", "/work", "fork", "s2"]
        );
        // Reasonix resumes with a short flag and forks with `--copy`, which
        // continues in a writable copy rather than reopening the original.
        assert_eq!(
            open_argv(AgentControl::Reasonix, Some(("s3", false))),
            ["reasonix", "/work", "-r", "s3"]
        );
        assert_eq!(
            open_argv(AgentControl::Reasonix, Some(("s3", true))),
            ["reasonix", "/work", "-r", "s3", "--copy"]
        );
        // Kimi names the session it resumes. `--session <id>` and `--continue`
        // are mutually exclusive, so only ever one of them is emitted — and
        // never the bare `--session`, which opens its session browser.
        assert_eq!(
            open_argv(AgentControl::Kimi, Some(("s4", false))),
            ["kimi", "/work", "--session", "s4"]
        );
        // opencode resumes with `-s <id>` and branches with `--fork`. Both are
        // pinned even though the dashboard cannot reach them yet — no opencode
        // hook payload names a session id, so nothing ever fills `s5` — because
        // the day one does, this is what has to already be right.
        assert_eq!(
            open_argv(AgentControl::OpenCode, Some(("s5", false))),
            ["opencode", "/work", "-s", "s5"]
        );
        assert_eq!(
            open_argv(AgentControl::OpenCode, Some(("s5", true))),
            ["opencode", "/work", "-s", "s5", "--fork"]
        );
    }

    /// Kimi has no fork flag, so the two argvs are **identical** — which is the
    /// whole mechanism behind `supports_fork()` and therefore behind `f` hiding
    /// itself on a Kimi row. Asserted end to end (through `open_session`, where
    /// the argv is actually built) rather than left implied by the equality in
    /// `resume_args`, because a stray `if fork` anywhere on this path would
    /// silently turn a hidden key back into one that resumes in place.
    #[test]
    fn kimi_forks_and_resumes_identically() {
        let plain = open_argv(AgentControl::Kimi, Some(("s4", false)));
        let forked = open_argv(AgentControl::Kimi, Some(("s4", true)));
        assert_eq!(plain, forked, "a fork request must add nothing for Kimi");
        assert!(!AgentControl::Kimi.supports_fork());
        // The other three still differ, so this test can't pass by the argv
        // having stopped carrying the flag for everyone.
        for agent in [
            AgentControl::Claude,
            AgentControl::Codex,
            AgentControl::Reasonix,
        ] {
            assert_ne!(
                open_argv(agent, Some(("s4", false))),
                open_argv(agent, Some(("s4", true))),
                "{agent:?}"
            );
        }
        // Grok's resume flags are Claude's exactly, which is why the two share an
        // arm — this pins that they may only share it while that stays true.
        assert_eq!(
            open_argv(AgentControl::Grok, Some(("s4", false))),
            ["grok", "/work", "--resume", "s4"]
        );
        assert_eq!(
            open_argv(AgentControl::Grok, Some(("s4", true))),
            ["grok", "/work", "--resume", "s4", "--fork-session"]
        );
    }

    /// The seam's canonical-path contract (§3), on the *local* arm — which is
    /// what makes the in-process and the wire path indistinguishable to the
    /// picker: what comes out is `~`-collapsed, and what goes in is expanded
    /// before it touches the filesystem or an argv.
    #[test]
    fn local_backend_speaks_host_canonical_paths() {
        let root = std::env::temp_dir().join(format!("cm-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/sub")).unwrap();
        let backend = LocalBackend::with_home(&root.display().to_string());

        // In: a `~` argument is expanded against the host's home.
        assert!(backend.dir_exists("~/proj"));
        assert!(!backend.dir_exists("~/nope"));
        // …and an absolute path still works, so nothing regresses.
        assert!(backend.dir_exists(&root.join("proj").display().to_string()));

        // Out: completions come back collapsed, never as absolute twins.
        assert_eq!(backend.complete_path("~/proj/"), vec!["~/proj/sub/"]);

        // The launch argv gets the *expanded* path — a process chdir, not a
        // shell word, so a `~` there would be a literal directory name.
        let plan = backend
            .open_session(&OpenSpec {
                agent: AgentControl::Claude,
                cwd: "~/proj".to_string(),
                resume: None,
                worktree: None,
            })
            .expect("plans");
        assert_eq!(plan.argv()[2], root.join("proj").display().to_string());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_for_completion_cases() {
        assert_eq!(split_for_completion("/a/b/c"), ("/a/b".into(), "c".into()));
        assert_eq!(split_for_completion("/a/b/"), ("/a/b".into(), "".into()));
        assert_eq!(split_for_completion("/foo"), ("/".into(), "foo".into()));
        assert_eq!(split_for_completion("foo"), (".".into(), "foo".into()));
        assert_eq!(split_for_completion(""), (".".into(), "".into()));
    }

    #[test]
    fn complete_dirs_lists_matching_subdirs_absolute() {
        let root = std::env::temp_dir().join(format!("cm-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["alpha", "apple", "apricot", "banana"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("apex-file"), "x").unwrap(); // a file, must be skipped

        let prefix = format!("{}/ap", root.display());
        let got = complete_dirs(&prefix);
        // Dirs only, prefix-matched, absolute + trailing slash, sorted; no
        // dotfiles, no plain files.
        assert_eq!(
            got,
            vec![
                format!("{}/apple/", root.display()),
                format!("{}/apricot/", root.display()),
            ]
        );
        // A bare directory (no basename) lists all its non-hidden subdirs.
        let all = complete_dirs(&format!("{}/", root.display()));
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|p| p.ends_with('/')));

        let _ = std::fs::remove_dir_all(&root);
    }
}
