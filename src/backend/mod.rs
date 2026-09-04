//! `Backend` is the dashboard's seam to *where sessions run and where their
//! files live*. `Local` is in-process (the dashboard and the agents share one
//! host); `Remote` reaches a `miao-server` over a (possibly
//! ssh-forwarded) socket. Enum-dispatched to match `AgentControl`'s style: no
//! dyn, no registry, just a `match` per operation.
//!
//! A backend owns *session lifecycle + objective facts on one host*: the live
//! session list (with the per-host Codex title overlay already applied), the
//! resumable list, the session-name index, and killing a session. Everything
//! visual or preference-y — selection, Terminal control, pins/mutes, preview
//! capture — stays in the TUI (the *client*), which overlays its own state on
//! what the backend returns.
//!
//! [`LocalBackend`] is also the **server-core**: `miao-server` wraps one
//! to answer a remote dashboard's requests, so the same local-read logic backs
//! both the in-process path and the remote path. See `docs/remote-sessions.md`.
//!
//! [`Backend::open_session`] is the spawn seam: it turns an [`OpenSpec`] into a
//! [`LaunchPlan`], either a `SpawnLocal` argv (the window *is* the launcher) or
//! an `AttachRemote` argv onto a launcher the host already started inside its
//! pty pool. Either way the client only ever does the window half — the plan is
//! pure metadata until it spawns one. See §5 and §6.

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::agent::{ResumeCandidate, SessionIndex};
use crate::protocol::{
    ClientFrame, PROTOCOL_MIN, PROTOCOL_VERSION, ServerFrame, protocol_compatible, read_frame,
    write_frame,
};
use crate::state::{self, HostId, LauncherState, SessionFlags, SessionKey};
use cm_core::vitals::HostVitals;

// `LocalBackend` (the server-core), `OpenSpec`, and `LaunchPlan` live in cm-core;
// re-exported so `crate::backend::…` paths across the dashboard resolve unchanged.
pub use cm_core::backend::{LaunchPlan, LocalBackend, OpenSpec};

// Probe a host, deploy a `miao-server` if it needs one, and resolve the command
// to invoke. Split out because it is a self-contained subsystem that runs
// *before* a connection exists: nothing else here calls into it except
// `setup_ssh`, and it calls nothing back except `ConnLog` and the ssh
// primitives below.
mod provision;
pub(crate) use provision::{ConsentPrompt, UpgradeOffer, set_consent_channel, upgrade_host_server};
use provision::{Provisioning, UploadGate, incompatible_daemon_reason, resolve_remote_exe};

// =============================================================================
// ssh plumbing, shared with `provision`
// =============================================================================

/// Wrap a POSIX-sh script so it survives the remote's **login shell**.
///
/// `ssh host <command>` does not exec the command — it hands the whole string to
/// the account's login shell, which is regularly `fish` (and occasionally
/// `csh`), neither of which speaks `var=value`, `trap`, or `set -e`. Verified
/// the hard way: a `d="$HOME/…"` assignment came back as *"fish: Unsupported use
/// of '='"*.
///
/// So the command we send is `/bin/sh -c '<script>'`, and the wrapping survives
/// every dialect for one specific reason: a single-quoted string is literal in
/// sh, bash, zsh, fish, **and** csh. The catch is that only fish honours `\'` and
/// `\\` inside one, so the script must contain **neither a single quote nor a
/// backslash** — pinned by `provision::upload_script`'s tests, and the reason the deploy
/// script writes its marker with `echo` rather than `printf '%s\n'`.
fn login_shell_safe(script: &str) -> String {
    debug_assert!(
        !script.contains('\'') && !script.contains('\\'),
        "a script wrapped for the login shell must contain no quote or backslash: {script}"
    );
    format!("/bin/sh -c '{script}'")
}

/// An ssh/scp `Command` detached from the TUI's terminal — stdin/stdout/stderr
/// all null'd. The dashboard owns the terminal (ratatui alt-screen); a child that
/// inherited it would paint over the display (scp's progress meter, ssh
/// diagnostics) and a long-lived one (the `-L` forward) would also compete for
/// stdin keystrokes. `.output()` callers don't need this — they already capture
/// out/err and null stdin — so this is for the fire-and-forget `.status()`/
/// `.spawn()` children.
fn detached(program: &str) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// The shared ssh `-o` options used for every ssh/scp invocation to a host:
/// key/agent auth only (BatchMode), a shared multiplexed connection
/// (ControlMaster/Persist over `ctl`), a keepalive so a half-open link is torn
/// down rather than hanging the UI, and a bounded initial-connect timeout so a
/// black-holed host can't wedge `setup_ssh` on the OS SYN timeout (~2 min) —
/// which would strand the reconnect task in `Connecting` (ServerAlive* only
/// governs an *established* link, not the initial `connect()`).
///
/// `extra` is the host's own connection options, and it goes **first**: ssh
/// keeps the *first* value it obtains for an option, so ours ahead of theirs
/// would make the field inert for exactly the settings it exists to change —
/// `ConnectTimeout`, `ServerAliveInterval` and `ControlPersist` are all set
/// right below. The price is that `ControlPath`, `ControlMaster` and `BatchMode`
/// are overridable too, and each breaks something real: the first two split the
/// multiplexing this depends on (including the `-O cancel` that retires
/// forwards), the third lets ssh prompt on a child whose stdin is `/dev/null`.
/// Documented where the field is edited rather than blocked — an escape hatch
/// that second-guesses isn't one.
///
/// An *edit* to any of these takes effect only because
/// [`options_changed_since_last_dial`] retires the shared master first;
/// re-dialling on its own would re-join it and change nothing.
fn ssh_common_opts(ctl: &Path, extra: &[String]) -> Vec<String> {
    let mut opts: Vec<String> = extra.to_vec();
    opts.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=120".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        format!("ControlPath={}", ctl.display()),
    ]);
    opts
}

/// Read a child's stdout and stderr to completion, but **bounded**: at most
/// `cap` bytes from each, both drained concurrently.
///
/// `Command::output()` is what this replaces, and the difference is the point.
/// `output()` reads until EOF with no ceiling, so a host that connects and then
/// *streams* is not a hung connection ssh will notice — the peer is answering
/// keepalives, `ConnectTimeout` is long past, and the only thing that grows is
/// this process's memory. Trusting a remote host to stop talking is not a
/// property worth relying on, and it doesn't take malice: an `.bashrc` that
/// runs something chatty is enough.
///
/// A child that exceeds the cap stalls on a full pipe rather than being killed
/// here — the caller's `timeout` is what ends it, and `kill_on_drop` reaps it.
/// Both pipes are drained together because reading one to EOF first deadlocks
/// the moment the other fills.
async fn capped_output(
    mut child: tokio::process::Child,
    cap: u64,
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    async fn read_capped<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>, cap: u64) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let Some(pipe) = pipe else {
            return Vec::new();
        };
        let mut buf = Vec::new();
        let _ = pipe.take(cap).read_to_end(&mut buf).await;
        buf
    }
    let (out, err) = tokio::join!(
        read_capped(child.stdout.take(), cap),
        read_capped(child.stderr.take(), cap),
    );
    let status = child.wait().await?;
    // Sanitized *here*, where the bytes arrive, rather than at each place they
    // are shown. This text goes on to at least four destinations — the
    // connection log, the `ConnState::Failed` reason, `tracing` (which writes
    // files a user may later `cat` into a terminal), and the parsers — and only
    // the first two had any treatment. One call at the entry point covers the
    // ones that exist and the ones added later, which is the same argument
    // `ConnLog::push` makes one level further down. Nothing parsed here is
    // affected: line structure is preserved, and every field the probe returns
    // is printable.
    Ok((
        status,
        host_text_safe(&String::from_utf8_lossy(&out)),
        host_text_safe(&String::from_utf8_lossy(&err)),
    ))
}

/// Ceiling on what we'll buffer from one remote command. The probe answers in
/// seven short lines and `tic` in a sentence; anything approaching this is a
/// host misbehaving, and the parse only ever reads the first few lines anyway.
const REMOTE_OUTPUT_CAP: u64 = 256 * 1024;

/// How long `daemon ensure` may take. Longer than the probe: on a host whose
/// daemon isn't up yet this *starts* one, which is a spawn plus a socket bind,
/// and a cold NFS home has been known to make that unhurried.
const ENSURE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `-O cancel` may take. It reads as a local operation — the mux
/// client only writes to a unix socket — but the master turns it into a global
/// request and waits for sshd's reply, so it is a round trip like any other.
const MUX_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the host may take to make its clipboard socket path bindable. A
/// `mkdir` and an `rm` behind a login shell; the generous end of that is a
/// second.
const CLIPBOARD_PREP_TIMEOUT: Duration = Duration::from_secs(20);

/// Run a fire-and-forget ssh child to completion under a deadline, reporting
/// only whether it succeeded.
///
/// Every remote call on the connect path is bounded, and these are remote calls
/// too. The argument the probe's own `PROBE_TIMEOUT` makes holds here unchanged:
/// `ConnectTimeout` bounds only the handshake and
/// `ServerAlive*` only a host that goes *silent*, so neither covers a host — or a
/// wedged `ControlMaster`, which answers its socket and then does nothing — that
/// simply never finishes. Unbounded, one of those parks [`connection_task`] in
/// `Connecting` for good, and that task is the only thing that would ever retry.
///
/// `kill_on_drop` is what makes the deadline mean anything: the timeout ends the
/// attempt by dropping the future, which would otherwise leave an ssh child
/// talking to nobody.
async fn bounded_status(mut cmd: Command, limit: Duration) -> bool {
    cmd.kill_on_drop(true);
    matches!(tokio::time::timeout(limit, cmd.status()).await, Ok(Ok(s)) if s.success())
}

// =============================================================================
// The Backend seam, connection state, and the log
// =============================================================================

/// Per-host session management. `Local` is in-process; `Remote` speaks the wire
/// protocol to a `miao-server` over a (possibly ssh-forwarded) socket.
///
/// `Remote` is behind an `Arc` because the dashboard hands clones to background
/// tasks (see [`RemoteBackend::list_resumable`]); `Local` is boxed only to keep
/// the two arms the same size, since one `Backend` per host exists for the
/// process lifetime and the allocation is paid once at startup.
pub(crate) enum Backend {
    Local(Box<LocalHost>),
    Remote(Arc<RemoteBackend>),
}

/// Connection health of a backend, surfaced in the header aggregate and, in
/// full, in the hosts panel. `Local` is always `Connected`; a `Remote`'s
/// background task moves it Connecting → Connected → Disconnected (then back to
/// Connecting as it retries with backoff), or parks on `Failed` when the reason
/// is diagnosable and won't fix itself by retrying.
///
/// `Failed` is what closes the "silent ⚠" gap (§4): a missing or
/// version-mismatched `miao-server` on the remote used to surface as an
/// ordinary disconnect, so the user saw a warning triangle and no way to learn
/// *why*. The reason travels with the state and the panel prints it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    /// Reachable-but-unusable: the reason is a short human sentence, already
    /// phrased for display.
    Failed(String),
}

impl ConnState {
    /// Whether this host is currently usable for requests.
    pub(crate) fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected)
    }

    /// A short label for the hosts panel / header.
    ///
    /// "Short" is the caller's job to enforce: a `Failed` reason quotes what the
    /// host said, which is routinely a paragraph — a NixOS box refusing a
    /// glibc-linked binary answers in four lines. The panel flattens and
    /// truncates it to its row, and the full text lives in the connection log
    /// (`l`), which exists precisely because one row cannot hold it.
    pub(crate) fn label(&self) -> &str {
        match self {
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::Disconnected => "disconnected",
            ConnState::Failed(reason) => reason,
        }
    }
}

/// One line of a host's connection narrative.
#[derive(Debug, Clone)]
pub(crate) struct ConnLogEntry {
    /// When it happened. Monotonic, and rendered as an age, so neither a clock
    /// jump nor a timezone can make the sequence read wrong.
    pub(crate) at: Instant,
    /// Whether this line is a reason the connection didn't come up — the panel
    /// colors those.
    pub(crate) error: bool,
    /// Free text, **possibly multi-line and never elided**. The whole point of
    /// this log is that it holds what the one-line row cannot.
    pub(crate) text: String,
}

/// The rolling connection narrative for one host — what `l` opens in the hosts
/// panel.
///
/// It exists because the panel gives a failure one row while the reason is
/// routinely longer than that, and because the *sequence* diagnoses where the
/// surviving sentence only reports: "probed the host, decided to deploy, the
/// deploy came back with this" tells you what to fix, where "could not deploy
/// miao-server: …" truncated at the row edge does not. Every step of
/// probe → decide → deploy → ensure → forward → handshake writes here, so a
/// failure at any of them is legible after the fact rather than only in a debug
/// log the user has to know to enable.
///
/// Capped: a host that has been flapping for a week costs a bounded amount.
#[derive(Debug, Default)]
pub(crate) struct ConnLog {
    entries: Mutex<VecDeque<ConnLogEntry>>,
}

/// How many lines one host's log keeps. Two full connect attempts' worth of
/// narrative is ~15 lines, so this holds a long flap without growing.
const CONN_LOG_CAP: usize = 200;

/// Make text safe to paint into a terminal cell, and *legible* while doing it.
///
/// **Most of what this log carries is the host's own words** — a loader's
/// refusal, a `tic` complaint, `uname` output, a version string — captured from
/// stderr and quoted verbatim, which is the whole point of the log.
///
/// **This is a second line of defence, not the only one, and the distinction is
/// worth stating precisely because the obvious reading is wrong.** An `ESC` in
/// remote output would indeed be a command to the emulator rather than a
/// character — but it never reaches one: ratatui filters control characters out
/// of every span before a cell is written, on both paths this log takes
/// (`ratatui_core::buffer::Buffer::set_stringn` and
/// `ratatui_core::text::Span::styled_graphemes`, which also drops zero-width
/// graphemes, covering bidi overrides and friends). Verified against the pinned
/// 0.30.2. So the *security* claim belongs to the renderer, and it is a version
/// pin away from being ours instead.
///
/// What this function is actually worth, today:
/// * **Legibility.** ratatui drops a control character silently, leaving
///   `\u{1b}[2J` on screen as a bare `[2J` and a `\t` as nothing at all. A
///   visible `\u{FFFD}` says the host emitted something unprintable, which is
///   part of the diagnosis; `\t` becomes a space so words don't fuse.
/// * **A backstop that costs nothing.** It holds if the renderer is swapped, if
///   a sink appears that doesn't go through a `Span`, or if this text is ever
///   written somewhere rawer than a ratatui buffer — and one such sink is
///   already here: `tracing` writes host stderr into log *files*, inert until
///   somebody `cat`s one, at which point their terminal is the renderer and
///   ratatui is nowhere in the picture.
///
/// Applied at [`capped_output`], where remote bytes arrive, so every consumer
/// downstream — log, failure reason, tracing, parsers — gets the treated text
/// without each having to remember.
///
/// `\n` survives because the log is line-structured and splits on it. The
/// control classes here are Unicode `Cc` — C0, `DEL`, and the C1 range where a
/// bare `\u{9b}` *is* CSI. Deliberately **not** widened to an ASCII-printable
/// allowlist: that would mangle every non-English error message a host returns,
/// which is a real cost against a class the renderer already handles.
pub(crate) fn host_text_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => '\n',
            '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect()
}

impl ConnLog {
    fn push(&self, error: bool, text: String) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= CONN_LOG_CAP {
            entries.pop_front();
        }
        entries.push_back(ConnLogEntry {
            at: Instant::now(),
            error,
            // Sanitized at the sink rather than at each capture site: the log
            // has four or five separate remote-text sources (every stderr we
            // quote, plus `uname` and the version strings parsed out of the
            // probe), and one of them being added later without the treatment
            // is exactly how this comes back.
            text: host_text_safe(&text),
        });
    }

    /// A step that went as expected.
    fn info(&self, text: impl Into<String>) {
        self.push(false, text.into());
    }

    /// A step that didn't — the lines the user came here to read.
    fn error(&self, text: impl Into<String>) {
        self.push(true, text.into());
    }

    /// Oldest first, which is the order the story happened in.
    pub(crate) fn entries(&self) -> Vec<ConnLogEntry> {
        self.entries.lock().unwrap().iter().cloned().collect()
    }
}

/// The in-process host: a [`LocalBackend`] plus **its own** change watcher.
///
/// Owning the watcher here is the point (§5): the dashboard's run loop used to
/// create a `notify` watch on `sessions/` itself, so "how do I learn a session
/// changed" had two answers — an app-level fs watch for localhost and a mirror
/// push for remotes. Now every backend answers [`Backend::subscribe`] the same
/// way and the app has no filesystem knowledge at all. (It also makes
/// pooled-localhost free: that backend is a `Remote` over a local socket, and
/// it simply has no watcher to own.)
pub(crate) struct LocalHost {
    inner: LocalBackend,
    /// Bumped by the notify callback; the run loop reads it through
    /// [`BackendEvents`]. Held here so the watcher outlives `subscribe`.
    changed: Arc<AtomicBool>,
    watcher: Option<notify::RecommendedWatcher>,
    /// The one fact about this machine's sessions that is *not* in a file: the
    /// pool's attached bit. See [`PoolWatch`].
    pool: PoolWatch,
}

impl LocalHost {
    /// This host's sessions, with the pool's attached bit stamped onto the ones
    /// that are in it.
    ///
    /// The overlay is the whole reason [`PoolWatch`] exists. Every other field
    /// on a row is written to a file by the launcher and read straight back
    /// here; `attached` is not written anywhere at all — it lives in the
    /// daemon's memory, maintained from libshpool's hooks (§10.2), and reaches
    /// a dashboard only over the protocol. A direct-local backend that read
    /// only the files therefore had to leave it `None`, and `None` means
    /// *unknown*, which the UI resolves to "free to take": every pooled row on
    /// this machine looked available whether or not someone was working in it.
    fn list_sessions(&self) -> Vec<LauncherState> {
        let mut rows = self.inner.list_sessions();
        // Nothing on this machine is pooled → nothing to ask the daemon, and no
        // reason to have opened a socket to it. This is the gate that keeps the
        // watch off a laptop entirely (§10.1): the default population never has
        // a pooled row, so it never pays for one.
        if !rows.iter().any(|s| s.pool_session.is_some()) {
            return rows;
        }
        self.pool.ensure_started(&self.changed);
        let attached = self.pool.attached_by_pool_session();
        for row in &mut rows {
            if let Some(pool) = row.pool_session.as_deref() {
                row.attached = attached.get(pool).copied();
            }
        }
        rows
    }
}

/// A subscription to **this machine's own daemon**, held by the direct-local
/// backend for exactly one field: [`LauncherState::attached`].
///
/// The case it serves is a machine running direct-local that nonetheless has a
/// pool — a remote client launched into its daemon, so the sessions are pooled
/// and their state files land in the same `sessions/` dir this backend reads
/// (§10.1, "the third case"). Those rows are attachable, and the one thing
/// their files cannot say is whether a terminal is already in them.
///
/// **Subscribed, not sampled**, and that is the same ruling §10.2 made for the
/// daemon's own overlay. libshpool keeps no attached flag — its `List`
/// reconstructs one by `try_lock`ing the session mutex — so every query is a
/// sample that is stale the moment it is read. The daemon instead maintains the
/// bit from the pool's hooks, in its own causal order, and *pushes* it: a hook
/// wakes every subscriber, which is what makes an attach or a detach visible at
/// all (neither touches anything under `sessions/`, so the notify watch never
/// fires for one). Subscribing is therefore not merely cheaper than polling
/// here; it is the only way to see the transitions.
///
/// It carries no presumption layer, unlike [`RemoteBackend::presumed_attached`].
/// That layer exists to hold an answer the dashboard has already proved through
/// the length of an ssh round trip; here the host is a unix socket away and its
/// correcting `Delta` arrives on the same wake as the attach that caused it, so
/// there is nothing to bridge.
#[derive(Default)]
struct PoolWatch {
    /// The daemon's account of its pool, keyed the way its frames are.
    ///
    /// Keyed by [`SessionKey`] rather than by pool name because `Removed`
    /// names a key, and a map that cannot answer that frame either leaks an
    /// entry per ended session or has to be rebuilt by a scan. The pool name
    /// rides along as the join column: it is what the *file* side of the
    /// overlay carries, and the only token both sides of this join agree on.
    rows: Arc<Mutex<HashMap<SessionKey, PoolRow>>>,
    /// Whether the subscriber task has been spawned. Started on first sight of
    /// a pooled row rather than at [`Backend::subscribe`], so a dashboard with
    /// nothing pooled never dials anything.
    started: AtomicBool,
}

/// One pooled session as the daemon describes it — the join column and the bit.
struct PoolRow {
    pool_session: String,
    attached: Option<bool>,
}

impl PoolWatch {
    /// `pool_session` → a client is attached, for the rows the daemon has told
    /// us about. Absent means *unknown*, which is what a row with no answer must
    /// keep reading as.
    fn attached_by_pool_session(&self) -> HashMap<String, bool> {
        self.rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|r| Some((r.pool_session.clone(), r.attached?)))
            .collect()
    }

    /// Spawn the subscriber, once. `changed` is the local backend's own signal,
    /// deliberately: the machine now has two things worth waking the dashboard
    /// for — a state file moved, and a terminal attached or left — and the
    /// backend answers [`Backend::subscribe`] with one flag either way (§5).
    ///
    /// A no-op outside a tokio runtime, which is what keeps this callable from
    /// the synchronous read path it is called from: a test driving the backend
    /// directly gets no watch and an empty overlay, exactly as if no daemon
    /// were running.
    fn ensure_started(&self, changed: &Arc<AtomicBool>) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(pool_watch_task(
            state::server_sock_path(),
            self.rows.clone(),
            changed.clone(),
        ));
    }
}

/// Keep [`PoolWatch::rows`] current for as long as the dashboard runs,
/// reconnecting on loss with the same backoff a remote host gets.
///
/// The connection is the ordinary protocol handshake — `Hello`, check the
/// version floor, `Subscribe` — and then only the pushed stream matters: this
/// client never sends a request, so there is nothing to multiplex and no
/// `req_id` to route. Every other frame is ignored rather than refused, which is
/// the same forward-tolerance the full client applies (§3).
///
/// **No daemon is a normal state, not a failure.** A machine that has never been
/// remoted into has no socket to connect to, and one whose daemon stops has a
/// pool that no longer exists; both leave the map empty, which reads as
/// "unknown" and puts the UI back exactly where it was before this existed. So
/// the loop announces itself once at `debug` and then retries quietly — it is
/// reached only when the dashboard has already seen a pooled row, so a daemon
/// really is expected to be there.
async fn pool_watch_task(
    sock: PathBuf,
    rows: Arc<Mutex<HashMap<SessionKey, PoolRow>>>,
    changed: Arc<AtomicBool>,
) {
    let mut backoff = RECONNECT_INITIAL;
    loop {
        match UnixStream::connect(&sock).await {
            Ok(stream) => {
                tracing::debug!("pool watch: subscribed to {}", sock.display());
                backoff = RECONNECT_INITIAL;
                pool_watch_serve(stream, &rows, &changed).await;
            }
            Err(e) => {
                tracing::debug!("pool watch: {} unreachable ({e})", sock.display());
            }
        }
        // Whatever ended it, the pool we were describing is no longer one we can
        // see. Clearing is what takes the rows back to *unknown* rather than
        // leaving them asserting a bit from before the daemon went away — and
        // "unknown" is the one reading that is still true.
        if !rows.lock().unwrap().is_empty() {
            rows.lock().unwrap().clear();
            changed.store(true, Ordering::Relaxed);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Handshake, subscribe, and fold the pushed stream into `rows` until the
/// connection ends. Returns on EOF or any error — the caller retries.
async fn pool_watch_serve(
    stream: UnixStream,
    rows: &Arc<Mutex<HashMap<SessionKey, PoolRow>>>,
    changed: &Arc<AtomicBool>,
) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let hello = ClientFrame::Hello {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
    };
    if write_frame(&mut wr, &hello).await.is_err() {
        return;
    }
    match read_frame::<_, ServerFrame>(&mut rd).await {
        Ok(Some(ServerFrame::Welcome { protocol, .. })) if protocol_compatible(protocol) => {}
        other => {
            tracing::debug!("pool watch: no usable Welcome ({other:?})");
            return;
        }
    }
    if write_frame(&mut wr, &ClientFrame::Subscribe).await.is_err() {
        return;
    }
    while let Ok(Some(frame)) = read_frame::<_, ServerFrame>(&mut rd).await {
        // A frame that changes nothing we track must not wake the dashboard: the
        // daemon serves every state file, pooled or not, and a redraw per
        // unpooled delta would spend the whole point of subscribing.
        let touched = match frame {
            ServerFrame::Snapshot { sessions } => {
                let mut m = rows.lock().unwrap();
                m.clear();
                for s in sessions {
                    if let Some(row) = PoolRow::of(&s) {
                        m.insert(s.key(), row);
                    }
                }
                true
            }
            ServerFrame::Delta { state } => match PoolRow::of(&state) {
                Some(row) => {
                    let mut m = rows.lock().unwrap();
                    let before = m.get(&state.key()).and_then(|r| r.attached);
                    let moved = before != row.attached;
                    m.insert(state.key(), row);
                    moved
                }
                None => false,
            },
            ServerFrame::Removed { key } => rows.lock().unwrap().remove(&key).is_some(),
            _ => false,
        };
        if touched {
            changed.store(true, Ordering::Relaxed);
        }
    }
}

impl PoolRow {
    /// The pooled half of a session the daemon described; `None` for a row that
    /// isn't in the pool at all (the daemon serves every state file, and a
    /// direct-local dashboard on the same machine already reads those itself).
    fn of(s: &LauncherState) -> Option<Self> {
        Some(Self {
            pool_session: s.pool_session.clone()?,
            attached: s.attached,
        })
    }
}

// =============================================================================
// Vitals, capabilities, and the plans a backend returns
// =============================================================================

/// A backend's change signal, taken (and cleared) by the run loop. One handle
/// per backend, from [`Backend::subscribe`]; a local one is fed by that
/// backend's fs watcher, a remote one by its connection task's mirror pushes
/// and connect/disconnect transitions.
pub(crate) struct BackendEvents {
    changed: Arc<AtomicBool>,
    /// A utilisation poll came back. Kept apart from `changed` because it must
    /// *not* trigger a reload: it changes no row, only the two numbers on a
    /// host's line in the panel that asked for it. `None` for a local backend,
    /// which measures nothing (see [`Backend::vitals`]).
    vitals: Option<Arc<AtomicBool>>,
}

impl BackendEvents {
    /// Whether this backend changed since the last call (and clear the signal).
    pub(crate) fn take(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    /// Whether a utilisation poll landed since the last call (and clear the
    /// signal). Redraw-only: no row content depends on it.
    pub(crate) fn take_vitals(&self) -> bool {
        self.vitals
            .as_ref()
            .is_some_and(|f| f.swap(false, Ordering::Relaxed))
    }
}

/// How often the hosts panel asks a host for a fresh utilisation reading while
/// it is open. Utilisation is a background fact, not a live meter: a number
/// that ticks four times a minute is plenty to answer "has this box got room?",
/// and the panel is a diagnostic surface, not a monitor. Longer than the
/// daemon's own cache window on purpose, so a lone dashboard's every poll is a
/// genuine probe rather than a repeat of the last answer.
const VITALS_POLL: Duration = Duration::from_secs(15);
/// How long a poll waits before giving up. Shorter than [`VITALS_POLL`] so a
/// host that never answers can't stack requests, and long enough that a slow
/// link (or a daemon priming its CPU counters) still lands.
const VITALS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `ForgetRecentDir` waits for its acknowledgement before giving up.
/// Nothing is displayed either way — the deadline exists only so the task ends
/// against a daemon too old to answer the frame at all.
const FORGET_RECENT_DIR_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a host's utilisation figures stand: on their way, here, or not
/// coming.
///
/// Three states rather than "numbers or nothing" because the figures answer a
/// question about *now* ("has this box got room?"), so the two ways of having
/// none must not look alike — and neither may be filled in with the last
/// answer. A reading is dropped the moment it stops describing the present
/// (the panel opens, the link comes back), leaving `Loading` under the
/// spinner; a poll that comes back empty-handed leaves `Unavailable` in the
/// figures' place. What is on screen is therefore either current or visibly
/// not a number.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) enum VitalsView {
    /// Waiting on the first answer since the figures were last invalidated —
    /// which the panel draws as a spinner rather than a hole that fills in a
    /// round trip later.
    #[default]
    Loading,
    /// The most recent reading. May itself be empty — a host whose OS we can't
    /// sample answered, it just had nothing to say (see [`HostVitals::is_empty`]).
    Reading(HostVitals),
    /// The last poll produced no reading at all: the host didn't answer inside
    /// the deadline, or its daemon predates `GetVitals` and ignored the frame.
    Unavailable,
}

/// A host's last utilisation reading and the state of its polling: the answer,
/// a "this is new" flag for the redraw, when we last asked, and whether an ask
/// is still out.
///
/// One place rather than four fields on [`RemoteBackend`] because they are only
/// ever read and written together — a store that forgets the flag is a panel
/// that silently freezes, and a poll that forgets `inflight` is a second request
/// stacked on a slow link's first.
#[derive(Default)]
pub(crate) struct VitalsCell {
    latest: Mutex<VitalsView>,
    changed: Arc<AtomicBool>,
    /// When the last poll was *sent* (not answered), so a host that never
    /// replies is retried on the same cadence as one that does rather than
    /// hammered.
    asked_at: Mutex<Option<Instant>>,
    inflight: AtomicBool,
}

impl VitalsCell {
    /// Claim the right to poll: `true` at most once per `interval`, and never
    /// while a previous ask is still out. Stamps the attempt, so the caller
    /// must actually make the request when it wins.
    fn claim_poll(&self, interval: Duration) -> bool {
        if self.inflight.load(Ordering::Relaxed) {
            return false;
        }
        let mut asked = self.asked_at.lock().unwrap();
        if asked.is_some_and(|t| t.elapsed() < interval) {
            return false;
        }
        *asked = Some(Instant::now());
        self.inflight.store(true, Ordering::Relaxed);
        true
    }

    /// Record a poll's outcome. `None` — nothing came back — becomes
    /// [`VitalsView::Unavailable`] rather than leaving the previous figures to
    /// stand for a host that has stopped answering.
    fn settle(&self, vitals: Option<HostVitals>) {
        *self.latest.lock().unwrap() = match vitals {
            Some(v) => VitalsView::Reading(v),
            None => VitalsView::Unavailable,
        };
        self.inflight.store(false, Ordering::Relaxed);
        self.changed.store(true, Ordering::Relaxed);
    }

    fn get(&self) -> VitalsView {
        *self.latest.lock().unwrap()
    }

    /// Drop the reading and go back to waiting for one. Called when it stops
    /// describing the present: the link died (numbers from before it did are a
    /// claim about a host we can no longer see) and the hosts panel opened (the
    /// last answer can be hours old, and nothing on a dim row would say so).
    ///
    /// Also re-arms the poll, so the very next pass asks — without that the
    /// spinner would sit through however much of the interval the previous ask
    /// left, which is the whole wait it exists to shorten.
    fn invalidate(&self) {
        *self.latest.lock().unwrap() = VitalsView::Loading;
        *self.asked_at.lock().unwrap() = None;
        self.changed.store(true, Ordering::Relaxed);
    }
}

/// What a host can do, as the host itself reports it — the `capabilities()`
/// seam that replaced `Option`-returning `attach_argv`/`shell_argv` (§5). App
/// code asks "does this host pool its sessions?", never "is this host local?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendCaps {
    /// Sessions live in a pty pool, so a local window *attaches* to one rather
    /// than being it — which is what makes detach (`D`), re-attach, and the
    /// steal meaningful. True for any host reached over the protocol, including
    /// a pooled localhost.
    pub pooled: bool,
    /// A `w` work-tab shell can be opened on this host.
    pub shell: bool,
}

/// How the client opens a shell on a host for the `w` work tab.
pub(crate) enum ShellPlan {
    /// Run the user's own shell locally in `cwd` (the terminal backend does it;
    /// there is no argv).
    InProcess { cwd: String },
    /// Spawn this argv — an `ssh -t <target>` that cds into the host's cwd.
    Spawn { argv: Vec<String> },
}

/// How the client attaches a window to an already-running pooled session.
pub(crate) struct AttachPlan {
    pub argv: Vec<String>,
}

/// What came of asking a host to end a session.
///
/// Three states rather than the `bool` this used to be, because the dashboard
/// now hides the row *before* the answer arrives (`Backend::presume_killed`) and
/// only one of the two failures is grounds for putting it back. "The host says
/// there is no such live session" and "the host never answered" collapse into
/// the same `false`, and they are opposites: the first means the row was right
/// to go, the second that nothing was signalled at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillOutcome {
    /// The host resolved the key and signalled the session.
    Signalled,
    /// The host had no live session under that key — it had already ended, so
    /// the row leaving was right even though the signal never went out.
    AlreadyGone,
    /// No answer: the host is unreachable, or too old to know the frame (it
    /// ignores what it can't decode, §3). Nothing was signalled and the session
    /// is still running — the one outcome an optimistic hide must unwind.
    Unreachable,
}

impl Backend {
    pub(crate) fn local() -> Self {
        Backend::Local(Box::new(LocalHost {
            inner: LocalBackend::new(),
            changed: Arc::new(AtomicBool::new(false)),
            watcher: None,
            pool: PoolWatch::default(),
        }))
    }

    /// The host this backend manages — `local` for in-process, the configured
    /// label for a remote. The dashboard stamps it onto each session it reads.
    pub(crate) fn host_id(&self) -> HostId {
        match self {
            Backend::Local(_) => HostId::local(),
            Backend::Remote(b) => b.host.clone(),
        }
    }

    /// Connection health, for the header surface. A local backend is always
    /// connected; a remote reflects its background connection task's state.
    pub(crate) fn conn_state(&self) -> ConnState {
        match self {
            Backend::Local(_) => ConnState::Connected,
            Backend::Remote(b) => b.conn_state(),
        }
    }

    /// Whether this host's sessions are still on their way: it is dialing, or
    /// the link is up but its first `Snapshot` has yet to land. Either way the
    /// host contributes no rows *yet*, which is what the session table's
    /// trailing "loading" line stands in for (`draw::connecting_row_label`).
    ///
    /// The second half is not a refinement but the larger half of the window:
    /// `Connected` is stored the moment the socket answers, a full round trip
    /// before the handshake and subscribe that actually fetch the sessions. Read
    /// off `conn_state` alone, the line therefore vanished while the rows were
    /// still in flight — briefly presenting a host with nothing to show as a
    /// host with nothing on it.
    ///
    /// A `Failed` or `Disconnected` host is deliberately **not** loading: it has
    /// stopped, not started, and the header tally plus the hosts panel are where
    /// that is answered (§9). The in-process backend is never loading — it reads
    /// its state files synchronously.
    pub(crate) fn awaiting_sessions(&self) -> bool {
        match self {
            Backend::Local(_) => false,
            Backend::Remote(b) => {
                matches!(b.conn_state(), ConnState::Connecting | ConnState::Connected)
                    && !b.mirrored.load(Ordering::Relaxed)
            }
        }
    }

    /// What the connection task did and what came back, for the hosts panel's
    /// `l` view. Empty for the in-process backend, which never dials anything —
    /// the panel says so rather than showing a blank box.
    pub(crate) fn conn_log(&self) -> Vec<ConnLogEntry> {
        match self {
            Backend::Local(_) => Vec::new(),
            Backend::Remote(b) => b.conn_log(),
        }
    }

    /// What this host supports, so app code branches on the capability rather
    /// than on locality (§1's load-bearing principle).
    pub(crate) fn capabilities(&self) -> BackendCaps {
        match self {
            Backend::Local(_) => BackendCaps {
                pooled: false,
                shell: true,
            },
            Backend::Remote(b) => BackendCaps {
                pooled: true,
                // Reached over ssh → an `ssh -t` shell tab. Reached over a
                // *local* socket (pooled-localhost) → there's no ssh target,
                // but the host is this machine, so the shell is in-process.
                shell: b.attach_target.is_some() || b.transport_is_local,
            },
        }
    }

    /// Start (or fetch) this backend's change signal. Called once per backend
    /// at startup and after a hosts-panel reconnect; a local backend lazily
    /// creates its `sessions/` + agent-path watcher on the first call.
    pub(crate) fn subscribe(&mut self) -> BackendEvents {
        match self {
            Backend::Local(h) => {
                if h.watcher.is_none() {
                    h.watcher = start_local_watcher(h.changed.clone());
                    // Whatever the watcher's fate, the first pass must reload.
                    h.changed.store(true, Ordering::Relaxed);
                }
                BackendEvents {
                    changed: h.changed.clone(),
                    vitals: None,
                }
            }
            Backend::Remote(b) => BackendEvents {
                changed: b.dirty.clone(),
                vitals: Some(b.vitals.changed.clone()),
            },
        }
    }

    /// The daemon version this host reported at handshake, for the hosts panel.
    /// `None` for a local backend (it *is* this build) or before a handshake.
    pub(crate) fn daemon_version(&self) -> Option<String> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => b.server_version.lock().unwrap().clone(),
        }
    }

    /// What restarting this host's daemon would deploy, when that differs from
    /// what it is running — the hosts panel's upgrade affordance, and the payload
    /// the upgrade itself stages.
    ///
    /// `None` on a local backend, on a disconnected host, and — importantly — on
    /// every connected host a restart would bring back on the same bytes.
    pub(crate) fn upgrade_offer(&self) -> Option<UpgradeOffer> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => b.upgrade.lock().unwrap().clone(),
        }
    }

    /// Round-trip time to this host, sampled opportunistically from real
    /// request/response traffic — there is deliberately **no `Ping` frame**
    /// (§9): every reply is already matched by `req_id`, so timing one costs
    /// nothing. `None` for local, or before any request has been answered.
    pub(crate) fn latency(&self) -> Option<Duration> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => *b.latency.lock().unwrap(),
        }
    }

    /// Where this host's CPU/memory figures stand — see [`VitalsView`].
    ///
    /// `None` for a host that has none coming: a local backend (there is no
    /// daemon on this side of the seam, and the dashboard deliberately measures
    /// nothing itself — a host reports its own utilisation or none is shown),
    /// or one that isn't connected, which is waiting on its link rather than on
    /// a reading and says so on its own row.
    pub(crate) fn vitals(&self) -> Option<VitalsView> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => b.conn_state().is_connected().then(|| b.vitals.get()),
        }
    }

    /// Drop this host's figures and wait for fresh ones (see
    /// [`VitalsCell::invalidate`]). Called on every hosts-panel open, because a
    /// reading taken the last time the panel was up describes whenever that
    /// was.
    pub(crate) fn invalidate_vitals(&self) {
        if let Backend::Remote(b) = self {
            b.vitals.invalidate();
        }
    }

    /// Ask this host for a fresh reading, at most once per [`VITALS_POLL`] and
    /// never twice at once. Returns immediately: the round trip runs on a
    /// background task and lands in the cell, because the caller is the UI
    /// thread and the far end is across an ssh link.
    ///
    /// Called only while the hosts panel is open — that is the entire reason
    /// this is a poll rather than a subscription. Utilisation is displayed
    /// nowhere else, and the panel is open for seconds at a time, so nothing is
    /// measured, sent, or woken for the hours it isn't.
    pub(crate) fn poll_vitals(&self) {
        self.poll_vitals_paced(VITALS_POLL, VITALS_TIMEOUT);
    }

    /// [`poll_vitals`] with its cadence injected, so a test can drive the
    /// throttle and the give-up path without waiting out the real intervals.
    ///
    /// [`poll_vitals`]: Self::poll_vitals
    fn poll_vitals_paced(&self, interval: Duration, timeout: Duration) {
        let Backend::Remote(b) = self else { return };
        if !b.vitals.claim_poll(interval) {
            return;
        }
        let backend = b.clone();
        tokio::spawn(async move {
            let reply = backend
                .request_within(timeout, |req_id| ClientFrame::GetVitals { req_id })
                .await;
            backend.vitals.settle(match reply {
                Some(ServerFrame::Vitals { vitals, .. }) => Some(vitals),
                // Unreachable, or a daemon too old to know the frame — it
                // ignores what it can't decode, so the answer is silence and
                // the deadline is what ends the wait.
                _ => None,
            });
        });
    }

    /// Live sessions on this host (those with a current state file).
    pub(crate) fn list_sessions(&self) -> Vec<LauncherState> {
        match self {
            Backend::Local(h) => h.list_sessions(),
            Backend::Remote(b) => b.list_sessions(),
        }
    }

    /// Merge each agent backend's session-name shard into one index (today only
    /// Claude's manifest scan contributes — Codex titles arrive on
    /// `LauncherState.name` via the per-host overlay).
    pub(crate) fn session_index(&mut self) -> SessionIndex {
        match self {
            Backend::Local(h) => h.inner.session_index(),
            Backend::Remote(b) => b.session_index(),
        }
    }

    /// Resumable sessions across every agent backend, most-recent first, capped
    /// at `limit`. Returns the merged list plus any per-agent errors (the caller
    /// decides how to surface them). The walk reads file tails synchronously
    /// (local) or makes a blocking round-trip (remote), so an async caller
    /// should wrap this in `block_in_place`.
    pub(crate) fn list_resumable(&self, limit: usize) -> (Vec<ResumeCandidate>, Vec<String>) {
        match self {
            Backend::Local(h) => h.inner.list_resumable(limit),
            Backend::Remote(b) => b.list_resumable(limit),
        }
    }

    /// Tear the session down, naming it by its opaque [`SessionKey`]. The
    /// *owning host* resolves the key to a live pid immediately before
    /// signalling, so a mirror lagging the session's exit can't make it SIGTERM
    /// a recycled pid (§3). May block on a round-trip for a remote host, so an
    /// async caller should wrap this in `block_in_place`.
    pub(crate) fn kill_session(&self, key: &SessionKey) -> KillOutcome {
        match self {
            // In-process `libc::kill`: the only way to fail is to find no live
            // session under the key, so there is no unreachable case here.
            Backend::Local(h) => match h.inner.kill_session(key) {
                true => KillOutcome::Signalled,
                false => KillOutcome::AlreadyGone,
            },
            Backend::Remote(b) => b.kill_session(key),
        }
    }

    /// Treat `key` as already gone, before the host has been asked — the
    /// optimistic half of a kill. The row leaves the table on the next reload
    /// rather than a round trip later; [`unpresume_killed`] puts it back if the
    /// host turns out never to have heard the request.
    ///
    /// A no-op for a plain local backend, which has nothing to be optimistic
    /// about: its kill is an in-process signal and its `sessions/` watcher takes
    /// the row away within the settle. Under pooled-localhost the daemon is
    /// still on the far side of a socket, and that backend is a `Remote` — which
    /// is why this branches on the backend, not on locality.
    ///
    /// [`unpresume_killed`]: Self::unpresume_killed
    pub(crate) fn presume_killed(&self, key: &SessionKey) {
        if let Backend::Remote(b) = self {
            b.presume_dead(key);
        }
    }

    /// Undo a [`presume_killed`]: nothing was signalled after all, so the
    /// session is still running and its row belongs back in the table.
    ///
    /// [`presume_killed`]: Self::presume_killed
    pub(crate) fn unpresume_killed(&self, key: &SessionKey) {
        if let Backend::Remote(b) = self {
            b.unpresume_dead(key);
        }
    }

    /// Treat `key` as held by another terminal — the reading a refused attach
    /// just came back with. Corrects the row now rather than on the host's own
    /// account of the same fact, which follows a round trip behind it.
    ///
    /// A no-op for a plain local backend, and that is a statement about
    /// *distance*, not about capability: [`PoolWatch`] gives it a real attached
    /// bit, but the daemon serving it is a unix socket away rather than an ssh
    /// round trip, and its correcting `Delta` arrives on the same hook-driven
    /// wake as the attach that provoked this. A presumption has nothing to
    /// bridge there, so there is none to make.
    pub(crate) fn presume_attached(&self, key: &SessionKey) {
        if let Backend::Remote(b) = self {
            b.presume_attached(key);
        }
    }

    /// Treat `key` as held by nobody — an attach of this dashboard's own has
    /// just ended, and the pool is one client at a time, so the bit the host is
    /// still serving is the one *we* set on the way in.
    ///
    /// Same seam and same no-op-when-local rule as [`presume_attached`]; the
    /// evidence and the direction are what differ.
    ///
    /// [`presume_attached`]: Self::presume_attached
    pub(crate) fn presume_detached(&self, key: &SessionKey) {
        if let Backend::Remote(b) = self {
            b.presume_detached(key);
        }
    }

    /// Record the host-owned flags for a session, so every dashboard watching
    /// that host agrees (§9). `false` when the host doesn't serve flags — a
    /// plain local backend, whose flags are the dashboard's own
    /// `dashboard-overrides.json` — which is the caller's signal to persist
    /// them locally instead. Blocks on a round-trip for a remote host.
    pub(crate) fn set_session_flags(&self, key: &SessionKey, flags: SessionFlags) -> bool {
        match self {
            Backend::Local(_) => false,
            Backend::Remote(b) => b.set_session_flags(key, flags),
        }
    }

    /// Plan how to open a session on this host (a fresh launch or a resume/fork).
    /// Local returns the argv for a Kitty window directly — pure metadata, no
    /// process starts until the client spawns the window. Remote RPCs the server
    /// to start the launcher inside its pty pool and returns an `AttachRemote`
    /// plan (an `ssh … attach` window). May block on the round-trip, so an async
    /// caller of the remote path should wrap this in `block_in_place`.
    pub(crate) fn open_session(&self, spec: &OpenSpec) -> anyhow::Result<LaunchPlan> {
        match self {
            Backend::Local(h) => h.inner.open_session(spec),
            Backend::Remote(b) => b.open_session(spec),
        }
    }

    /// How to open a window onto an *already-running* pooled session on this
    /// host. `force` steals it from whatever client currently holds it (the
    /// pool is one client at a time — §10.2).
    ///
    /// A `Result`, not an `Option` (§5): the old signature could only say
    /// "no", so every caller invented its own message for a case it couldn't
    /// distinguish. Now the host explains itself.
    pub(crate) fn attach_plan(
        &self,
        session_name: &str,
        force: bool,
    ) -> anyhow::Result<AttachPlan> {
        match self {
            // A direct-local backend pools nothing *itself*, but the machine it
            // runs on may still hold a pool — the daemon serving a laptop's
            // dashboard puts every session it launches there, and those rows
            // reach this dashboard through the same `sessions/` dir. So the
            // question this arm answers is not "is this host pooled" (it isn't)
            // but "can this machine reach its own pool", and the caller has
            // already established that the row carries a `pool_session` at all.
            Backend::Local(_) => Ok(AttachPlan {
                argv: local_attach_argv(session_name, force)?,
            }),
            Backend::Remote(b) => Ok(AttachPlan {
                argv: attach_argv(
                    b.attach_target.as_deref(),
                    &b.ssh_options,
                    &b.remote_exe.lock().unwrap(),
                    session_name,
                    force,
                    // No `[remote] inherit_env` here, and not an oversight:
                    // this is the *reattach* path. libshpool injects a
                    // forwarded value into the shell's environment only where
                    // it spawns that shell (`build_shell_env`, reached from
                    // `spawn_subshell`), which happens once, when the session
                    // is created — so a name passed here changes nothing about
                    // the session. What it *would* still do is make the daemon
                    // re-serialize every forwarded name and value to
                    // `$SHPOOL_SESSION_DIR/forward.env` in cleartext, which it
                    // does on every attach: pure exposure, zero effect. The
                    // create path ([`RemoteBackend::open_session`]) is the only
                    // one that passes them.
                    &[],
                ),
            }),
        }
    }

    /// How to open an interactive login shell on this host in `cwd` (the `w`
    /// work tab): in process for this machine, over ssh for a remote.
    pub(crate) fn shell_plan(&self, cwd: &str) -> anyhow::Result<ShellPlan> {
        match self {
            Backend::Local(h) => Ok(ShellPlan::InProcess {
                // The row's cwd is host-canonical; a local chdir needs the real
                // path, and this backend's own home is the one to expand it by.
                cwd: cm_core::paths::expand_home(cwd, h.inner.home()),
            }),
            Backend::Remote(b) => match b.attach_target.as_deref() {
                Some(target) => Ok(ShellPlan::Spawn {
                    argv: remote_shell_argv(target, &b.ssh_options, cwd),
                }),
                // Pooled localhost: the "remote" host is this machine, so the
                // shell is the ordinary local one. `$HOME` never crosses the
                // wire, so the expansion uses *our* home — correct precisely
                // because this transport is local-only by contract.
                None if b.transport_is_local => Ok(ShellPlan::InProcess {
                    cwd: cm_core::paths::expand_home(cwd, &cm_core::paths::host_home()),
                }),
                None => anyhow::bail!(
                    "cannot open a shell on {}: it is reached over a socket with no ssh target",
                    b.host.0
                ),
            },
        }
    }

    /// This host's recent working dirs, host-canonical (§3 — no `$HOME` on the
    /// wire, so what comes back is what the picker displays and submits). The
    /// remote path blocks on a round-trip, so wrap async callers in
    /// `block_in_place`.
    pub(crate) fn recent_dirs(&self) -> Vec<String> {
        match self {
            Backend::Local(h) => h.inner.recent_dirs(),
            Backend::Remote(b) => b.recent_dirs(),
        }
    }

    /// Drop `cwd` from this host's recent working dirs — the picker's `Ctrl-d`.
    ///
    /// **Fire-and-forget, unlike every other query here.** The caller has
    /// already taken the row off the list it is drawing, because the one rule
    /// the picker holds to is that no round trip sits between a keystroke and
    /// its echo (§9); waiting on a distant box to confirm a deletion the user
    /// can see would trade that for nothing they'd read. What comes back is a
    /// log line, and a daemon too old to know the frame answers with silence
    /// the deadline ends.
    pub(crate) fn forget_recent_dir(&self, cwd: &str) {
        match self {
            Backend::Local(h) => {
                h.inner.forget_recent_cwd(cwd);
            }
            Backend::Remote(b) => b.forget_recent_dir(cwd),
        }
    }

    /// Directory completions for `prefix` on this host's filesystem
    /// (host-canonical, trailing `/`). Remote blocks — wrap in
    /// `block_in_place`.
    pub(crate) fn complete_path(&self, prefix: &str) -> Vec<String> {
        match self {
            Backend::Local(h) => h.inner.complete_path(prefix),
            Backend::Remote(b) => b.complete_path(prefix),
        }
    }

    /// Whether `path` is a directory on this host. Remote blocks — wrap in
    /// `block_in_place`.
    pub(crate) fn dir_exists(&self, path: &str) -> bool {
        match self {
            Backend::Local(h) => h.inner.dir_exists(path),
            Backend::Remote(b) => b.dir_exists(path),
        }
    }
}

/// Watch this host's session state for changes, feeding `changed`. Owned by the
/// local backend (§5), not the app: the `sessions/` dir where launchers write,
/// plus each agent backend's own nominated paths (Claude's session-name store,
/// Codex's title-store WAL — the wake for the throttled title overlay).
///
/// Best-effort throughout: a missing path simply isn't watched, and a watcher
/// that can't be created at all leaves the dashboard on its reload cadence
/// rather than failing to start.
fn start_local_watcher(changed: Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher as _;
    let sink = changed.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Skip Access (open/close/read): our own reads would otherwise wake us.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        sink.store(true, Ordering::Relaxed);
    })
    .ok()?;
    let dir = state::sessions_dir();
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("could not watch {}: {e}", dir.display());
        return None;
    }
    for &agent in crate::agent::AgentControl::ALL {
        for path in agent.watch_paths() {
            let _ = watcher.watch(&path, notify::RecursiveMode::NonRecursive);
        }
    }
    Some(watcher)
}

// =============================================================================
// Remote backend (RPC to a `miao-server` over a socket)
// =============================================================================

/// How a [`RemoteBackend`] reaches its server.
pub(crate) enum Transport {
    /// Connect straight to a daemon socket **on this same machine** — no ssh
    /// hop. Local-only is part of the contract, not an accident: this is the
    /// pooled-localhost transport (§10.1), where the "remote" host is the
    /// machine the dashboard runs on, so an attach needs no ssh and a `w` shell
    /// is opened in process. (It doubles as the manual-forward / test path.)
    LocalSocket(PathBuf),
    /// Set up an ssh forward to `target`'s daemon and connect via `local_sock`:
    /// ensure the daemon is running + learn its socket path (`daemon ensure`),
    /// then run a forward-only `ssh -N -L <local_sock>:<remote_sock> target`
    /// child (the tunnel, killed when this backend drops; the daemon persists).
    Ssh {
        target: String,
        local_sock: PathBuf,
        /// The host's connection options, as the user typed them: ssh arguments,
        /// verbatim, in order. Split by [`split_connection_options`] into the
        /// options every ssh call for this host carries and the port forwards,
        /// which exactly one call may.
        options: Vec<String>,
        /// Offer this host the dashboard machine's clipboard — one synthesized
        /// `-R` alongside the user's own forwards. See
        /// [`clipboard_forward_for_home`].
        clipboard: bool,
    },
}

/// One port forward lifted out of a host's connection options — the flag and its
/// argument, kept apart so `-O cancel` can name the same forward later.
///
/// A forward is the one ssh argument that cannot simply ride
/// [`ssh_common_opts`] with the rest. An option is a property of the connection
/// and repeating it is free; a forward is a *resource the connection holds*, and
/// repeating it collides:
///
/// * within [`setup_ssh`], the probe opens the master and registers it, and
///   `daemon ensure` then re-requests it against a master that already has it;
/// * the transport's own housekeeping is `ssh <opts> -O cancel -L <sock> target`,
///   and `-O cancel` cancels **every** forward named on its command line — so a
///   `-L` living in `opts` would be torn down by us, once per reconnect;
/// * every attach window would ask for it again, one collision per window.
///
/// So it goes on the `ssh -N -L` tunnel child and nowhere else. That is also the
/// child whose lifetime the user means by "while I'm connected to this host".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Forward {
    flag: String,
    spec: String,
}

impl std::fmt::Display for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.flag, self.spec)
    }
}

/// Split a host's connection options into what every ssh call carries and the
/// forwards, which only the tunnel child may (see [`Forward`]).
///
/// The only thing recognised is `-L`/`-R`/`-D`, glued or with its argument in
/// the next token; everything else passes through untouched and unvalidated,
/// which is the point of the field. Case matters — `-L` is a local forward,
/// `-l` is the login name.
///
/// The glued form is normalised apart (`-D1080` → `-D` + `1080`) so the cancel
/// names a forward the same way however it was typed. A trailing flag with no
/// argument is **dropped**: it is a usage error on every ssh call that would
/// carry it, and these reach `attach` and the `w` shell too. Pure.
pub(crate) fn split_connection_options(args: &[String]) -> (Vec<String>, Vec<Forward>) {
    let mut opts = Vec::new();
    let mut forwards = Vec::new();
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        // `get` rather than a slice: a token can be any UTF-8 the user typed, and
        // byte 2 need not be a char boundary.
        let glued = a.len() > 2 && matches!(a.get(..2), Some("-L" | "-R" | "-D"));
        if glued {
            forwards.push(Forward {
                flag: a[..2].to_string(),
                spec: a[2..].to_string(),
            });
        } else if matches!(a.as_str(), "-L" | "-R" | "-D") {
            if let Some(spec) = rest.next() {
                forwards.push(Forward {
                    flag: a.clone(),
                    spec: spec.clone(),
                });
            }
        } else {
            opts.push(a.clone());
        }
    }
    (opts, forwards)
}

/// One in-flight request the connection task must answer by `req_id`.
struct PendingRequest {
    req_id: u64,
    frame: ClientFrame,
    reply: oneshot::Sender<ServerFrame>,
}

/// Backend for a session running on another host, reached over a (possibly
/// ssh-forwarded) unix socket. A background task owns the connection: it keeps
/// an in-memory **mirror** of the host's sessions current (driven by the
/// server's `Snapshot`/`Delta`/`Removed` push), and pumps request/response by
/// `req_id`. The synchronous [`Backend`] methods read the mirror (no round-trip)
/// or block on a oneshot for a reply — so callers should be inside
/// `block_in_place` when they might block (resume list, kill).
pub(crate) struct RemoteBackend {
    /// The host this backend speaks for; stamped onto every session it returns.
    host: HostId,
    /// ssh target for the attach window, learned from the transport: `Some` for
    /// an ssh host (`ssh -t <target> miao-server attach <name>`), `None` for a
    /// direct socket transport (a same-host `miao-server attach <name>`).
    attach_target: Option<String>,
    /// The host's connection options minus its forwards — everything that is
    /// safe to repeat, which is what lets an attach window and the `w` shell
    /// carry them too. Empty for a socket transport, which runs no ssh.
    ssh_options: Vec<String>,
    /// Whether this backend's transport is [`Transport::LocalSocket`], i.e. the
    /// daemon is on *this* machine. Distinguishes pooled-localhost (where a
    /// missing ssh target is correct and a shell is in-process) from a
    /// misconfigured remote.
    transport_is_local: bool,
    /// Latest known sessions on the remote host, keyed by their opaque
    /// [`SessionKey`] — the wire's only session identifier (§3).
    mirror: Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    /// Sessions the dashboard has asked this host to end, hidden from
    /// [`list_sessions`] from the moment the request goes *out* rather than when
    /// its answer comes back. Each holds the instant it was hidden, so a
    /// presumption the host never confirms lapses on its own after
    /// [`PRESUMED_DEAD_FOR`] and the row comes back.
    ///
    /// This is what makes `x` (and the window-close policy behind it) feel
    /// instant on a remote host: the kill is an ssh round trip, and every
    /// millisecond of it used to be a row sitting there looking alive.
    ///
    /// Deliberately *not* a removal from the mirror. The mirror is the host's
    /// account of itself, and overwriting it with a guess would leave nothing to
    /// correct against: the server pushes only what *changed*, so a session that
    /// survived a kill it never heard about would never be re-sent.
    presumed_dead: Arc<Mutex<HashMap<SessionKey, Instant>>>,
    /// The attached bit as the dashboard's own attaches have last proved it,
    /// stamped over the host's on the way out of [`list_sessions`].
    ///
    /// The evidence is always an attach, in one direction or the other, because
    /// an attach is the only operation that actually takes the pty's lock: its
    /// answer is a *transaction's* rather than an observation's, authoritative
    /// for the instant it happened, in a way no query about the same session can
    /// be. `ATTACH_EXIT_BUSY` proves someone else holds it (`true`); an attach of
    /// *ours* that has ended — its window closed, `D` pressed — proves the client
    /// that held it is gone (`false`). Either way this carries the answer to the
    /// row in the frame it arrives, instead of waiting for the host to say the
    /// same thing a round trip later (which it will: both fire a pool hook there,
    /// which wakes a push of exactly this bit).
    ///
    /// Unlike [`presumed_dead`] this needs no lapse, and must not have one. A
    /// dead presumption has no natural terminator — a session that survived a
    /// kill never changes, so the host has no reason to re-send it — whereas
    /// every `Delta` carries the attached bit, so the host's own account ends
    /// this presumption the moment it disagrees *or* agrees. Lapsing it on a
    /// timer would instead go back to showing the stale value it corrected,
    /// since a session stays attached for as long as its user is working.
    ///
    /// [`presumed_dead`]: Self::presumed_dead
    /// [`list_sessions`]: Self::list_sessions
    presumed_attached: Arc<Mutex<HashMap<SessionKey, bool>>>,
    /// Requests to the connection task; `None` once the task has exited.
    requests: mpsc::UnboundedSender<PendingRequest>,
    next_req_id: AtomicU64,
    /// The command to invoke the remote daemon, resolved at connect by
    /// `setup_ssh` (PATH `miao-server`, or a deployed cache path — see
    /// `docs/crate-split.md`). Defaults to `miao-server`, so before the task
    /// resolves it (or for a socket transport) the attach argv is unchanged.
    /// Never the dashboard binary (`miao`) — the remote runs the headless server.
    remote_exe: Arc<Mutex<String>>,
    /// Connection health the connection task updates as it dials / connects /
    /// loses the link, read by the header + hosts panel. Carries the `Failed`
    /// reason, so a diagnosable problem (server missing, version mismatch, ssh
    /// refused) is nameable rather than a silent ⚠ (§4).
    conn: Arc<Mutex<ConnState>>,
    /// The daemon version from `Welcome`, for the hosts panel.
    server_version: Arc<Mutex<Option<String>>>,
    /// What restarting this host's daemon would deploy, when that is newer than
    /// what it is serving. Re-decided by the connection task on every pass.
    upgrade: Arc<Mutex<Option<UpgradeOffer>>>,
    /// Most recent request→reply round-trip. Sampled from ordinary traffic —
    /// there is no `Ping` frame, because every reply is already `req_id`-matched
    /// and timing one is free (§9).
    latency: Arc<Mutex<Option<Duration>>>,
    /// The host's last pushed CPU/memory sample, beside the latency it is read
    /// with: together they say whether a host is reachable *and* whether it has
    /// room for more work.
    vitals: Arc<VitalsCell>,
    /// Set by the connection task whenever the mirror or connection state
    /// changes (a pushed `Snapshot`/`Delta`/`Removed`, or a connect/disconnect).
    /// Read through [`BackendEvents`], the same handle a local backend's fs
    /// watcher feeds — these off-thread updates fire no filesystem event.
    dirty: Arc<AtomicBool>,
    /// Whether the mirror holds this host's *account of itself* — set when a
    /// `Snapshot` lands, cleared with the mirror when the link drops.
    ///
    /// Distinct from `conn == Connected`, which is stored a full round trip
    /// earlier (before the handshake, let alone the subscribe). In that window a
    /// host has an empty mirror and nothing saying so, which is what the
    /// dashboard's trailing "loading" line exists to prevent — see
    /// [`Backend::awaiting_sessions`].
    mirrored: Arc<AtomicBool>,
    /// Bumped on each `Disconnected → Connected` transition. The dashboard
    /// compares it against what it last saw to fire the auto-reattach sweep
    /// (§7) exactly once per reconnect.
    reconnect_epoch: Arc<AtomicU64>,
    /// Everything the connection task did and what came back, for the hosts
    /// panel's `l` view. See [`ConnLog`].
    log: Arc<ConnLog>,
}

/// How long a session stays presumed dead on the strength of the dashboard's own
/// kill, with the host neither confirming it (a `Removed` push, which drops the
/// presumption early) nor being found unreachable (which withdraws it at once).
///
/// It is a backstop for the one gap the two exact answers leave: a host that
/// takes the request, answers `Killed{ok:true}`, and then never removes the
/// session — an agent that ignores SIGTERM, a launcher wedged mid-teardown. The
/// host has no reason to re-send a session that never changed, so without a
/// lapse the row would stay hidden until the next reconnect, and a session
/// nobody can see is worse than one that took a while to die.
///
/// Generous on purpose: a window that expires *during* a slow but successful
/// kill flickers the row back moments before it goes for real, which reads as a
/// glitch. Erring long only delays the honest reappearance of a session that
/// refused to die.
const PRESUMED_DEAD_FOR: Duration = Duration::from_secs(10);

/// The rows to show for a host: everything it has told us about, minus what the
/// dashboard is presuming it just killed, with the attached bit corrected to
/// whatever its own attaches last proved.
///
/// Takes `presumed_dead` by `&mut` because reading is when stale presumptions
/// are noticed: an entry past [`PRESUMED_DEAD_FOR`] is dropped here, which both
/// bounds the map and is what brings a survivor's row back. Pure apart from
/// that, with `now` injected, so the lapse is testable without waiting one out.
///
/// `presumed_attached` needs no such sweep — the host's own frames end those
/// (see the field), and an entry only exists while the dashboard knows something
/// the host has yet to say.
fn live_rows(
    mirror: &HashMap<SessionKey, LauncherState>,
    presumed_dead: &mut HashMap<SessionKey, Instant>,
    presumed_attached: &HashMap<SessionKey, bool>,
    now: Instant,
) -> Vec<LauncherState> {
    presumed_dead.retain(|_, since| now.duration_since(*since) < PRESUMED_DEAD_FOR);
    mirror
        .iter()
        .filter(|(key, _)| !presumed_dead.contains_key(*key))
        .map(|(key, state)| {
            let mut state = state.clone();
            if let Some(attached) = presumed_attached.get(key) {
                state.attached = Some(*attached);
            }
            state
        })
        .collect()
}

impl RemoteBackend {
    /// Start mirroring a server over `transport`. Returns immediately; the
    /// mirror fills once the background task connects and receives the snapshot.
    /// Connection failure leaves an empty mirror (host shows as having no
    /// sessions); the task then retries with backoff, re-snapshotting on each
    /// reconnect, until the backend is dropped.
    pub(crate) fn connect(transport: Transport, host: HostId) -> Arc<Self> {
        let (backend, shared, requests) = Self::build(&transport, host);
        tokio::spawn(connection_task(transport, shared, requests));
        backend
    }

    /// A backend for a host that never answers — the same construction
    /// [`connect`] does, minus the connection task, with `sessions` seeded into
    /// the mirror as if a `Snapshot` had brought them.
    ///
    /// Dropping the task's half of the handles is the point: nothing arrives to
    /// confirm or refute what the dashboard presumes, so a test of a presumption
    /// observes it alone. Requests fail fast against the closed channel, as they
    /// do for any host that is down.
    #[cfg(test)]
    pub(crate) fn unconnected_for_tests(host: HostId, sessions: Vec<LauncherState>) -> Arc<Self> {
        let (backend, _shared, _requests) =
            Self::build(&Transport::LocalSocket(PathBuf::new()), host);
        let mut mirror = backend.mirror.lock().unwrap();
        for s in sessions {
            mirror.insert(s.key(), s);
        }
        drop(mirror);
        // The seed stands in for a delivered `Snapshot`, so the host counts as
        // having reported — otherwise every test backend would read as still
        // loading and sit under the table's "loading" line forever.
        backend.mirrored.store(true, Ordering::Relaxed);
        backend
    }

    /// Put the two fields [`Backend::awaiting_sessions`] reads wherever a test
    /// needs them. One knob rather than two, because they only mean anything
    /// together: the connection task moves them in step, and a test that sets
    /// one without the other is describing a state that cannot occur.
    #[cfg(test)]
    pub(crate) fn simulate_link_for_tests(&self, conn: ConnState, mirrored: bool) {
        *self.conn.lock().unwrap() = conn;
        self.mirrored.store(mirrored, Ordering::Relaxed);
    }

    /// Everything a [`RemoteBackend`] and its connection task share, built but
    /// not yet wired to one. Split out of [`connect`] so a test can hold the
    /// task's half instead of running one.
    fn build(
        transport: &Transport,
        host: HostId,
    ) -> (
        Arc<Self>,
        ConnectionShared,
        mpsc::UnboundedReceiver<PendingRequest>,
    ) {
        // Capture the ssh target before the transport is moved into the task —
        // `open_session` needs it to build the attach window's argv.
        let attach_target = match &transport {
            Transport::Ssh { target, .. } => Some(target.clone()),
            Transport::LocalSocket(_) => None,
        };
        // Forwards are dropped here on purpose: an attach window must not ask
        // for one (see [`Forward`]). The tunnel child is the only carrier.
        let ssh_options = match &transport {
            Transport::Ssh { options, .. } => split_connection_options(options).0,
            Transport::LocalSocket(_) => Vec::new(),
        };
        let transport_is_local = matches!(transport, Transport::LocalSocket(_));
        let mirror = Arc::new(Mutex::new(HashMap::new()));
        let presumed_dead = Arc::new(Mutex::new(HashMap::new()));
        let presumed_attached = Arc::new(Mutex::new(HashMap::new()));
        let remote_exe = Arc::new(Mutex::new("miao-server".to_string()));
        let conn = Arc::new(Mutex::new(ConnState::Connecting));
        let dirty = Arc::new(AtomicBool::new(false));
        let mirrored = Arc::new(AtomicBool::new(false));
        let server_version = Arc::new(Mutex::new(None));
        let upgrade = Arc::new(Mutex::new(None));
        let latency = Arc::new(Mutex::new(None));
        let vitals = Arc::new(VitalsCell::default());
        let reconnect_epoch = Arc::new(AtomicU64::new(0));
        let log = Arc::new(ConnLog::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = ConnectionShared {
            host: host.clone(),
            mirror: mirror.clone(),
            presumed_dead: presumed_dead.clone(),
            presumed_attached: presumed_attached.clone(),
            remote_exe: remote_exe.clone(),
            conn: conn.clone(),
            dirty: dirty.clone(),
            mirrored: mirrored.clone(),
            server_version: server_version.clone(),
            upgrade: upgrade.clone(),
            latency: latency.clone(),
            vitals: vitals.clone(),
            reconnect_epoch: reconnect_epoch.clone(),
            log: log.clone(),
        };
        // `Arc` because the dashboard hands clones to background tasks: a
        // blocking round trip (the resume list) must not be made from the UI
        // thread, and a task can't borrow the `App` that owns the backend.
        let backend = Arc::new(Self {
            host,
            attach_target,
            ssh_options,
            transport_is_local,
            mirror,
            presumed_dead,
            presumed_attached,
            requests: tx,
            next_req_id: AtomicU64::new(1),
            remote_exe,
            conn,
            server_version,
            upgrade,
            latency,
            vitals,
            dirty,
            mirrored,
            reconnect_epoch,
            log,
        });
        (backend, shared, rx)
    }

    /// Current connection health, for the header surface.
    fn conn_state(&self) -> ConnState {
        self.conn.lock().unwrap().clone()
    }

    /// This host's connection narrative, oldest first.
    fn conn_log(&self) -> Vec<ConnLogEntry> {
        self.log.entries()
    }

    /// Hand a request to the connection task. Returns the reply channel and the
    /// send instant, or `None` if the host is known-down or the task has exited.
    ///
    /// Split out of [`request`] so the async sibling shares one enqueue path —
    /// including the fail-fast, which matters most there: queueing against a
    /// down host would otherwise park the caller through the whole reconnect
    /// backoff. While merely dialing (`Connecting`) we still queue, so the very
    /// first request right after `connect()` rides the pending connection.
    ///
    /// [`request`]: Self::request
    fn enqueue(
        &self,
        make: impl FnOnce(u64) -> ClientFrame,
    ) -> Option<(oneshot::Receiver<ServerFrame>, Instant)> {
        if matches!(
            self.conn_state(),
            ConnState::Disconnected | ConnState::Failed(_)
        ) {
            return None;
        }
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.requests
            .send(PendingRequest {
                req_id,
                frame: make(req_id),
                reply,
            })
            .ok()?;
        Some((rx, Instant::now()))
    }

    /// Send a request and block until its reply (or the task is gone). Returns
    /// `None` if the connection task has exited. Samples the round-trip time on
    /// the way through — the hosts panel's latency, with no dedicated frame.
    fn request(&self, make: impl FnOnce(u64) -> ClientFrame) -> Option<ServerFrame> {
        let (rx, sent_at) = self.enqueue(make)?;
        let reply = rx.blocking_recv().ok();
        if reply.is_some() {
            *self.latency.lock().unwrap() = Some(sent_at.elapsed());
        }
        reply
    }

    /// [`request`] for a caller already on the runtime, with a deadline.
    ///
    /// The deadline is not belt-and-braces: a peer that doesn't *know* a frame
    /// ignores it (the v4 forward-tolerance contract), so a request this build
    /// added is answered by silence on any older daemon. Without a timeout that
    /// silence would park the caller until the connection ended.
    ///
    /// [`request`]: Self::request
    async fn request_within(
        &self,
        within: Duration,
        make: impl FnOnce(u64) -> ClientFrame,
    ) -> Option<ServerFrame> {
        let (rx, sent_at) = self.enqueue(make)?;
        let reply = tokio::time::timeout(within, rx).await.ok()?.ok();
        if reply.is_some() {
            *self.latency.lock().unwrap() = Some(sent_at.elapsed());
        }
        reply
    }

    /// The reconnect counter behind the auto-reattach sweep (§7).
    pub(crate) fn reconnect_epoch(&self) -> u64 {
        self.reconnect_epoch.load(Ordering::Relaxed)
    }

    fn list_sessions(&self) -> Vec<LauncherState> {
        live_rows(
            &self.mirror.lock().unwrap(),
            &mut self.presumed_dead.lock().unwrap(),
            &self.presumed_attached.lock().unwrap(),
            Instant::now(),
        )
    }

    /// Hide `key`'s row now, on the strength of a kill we are only about to
    /// send. Flips `dirty` so the dashboard re-reads and the row goes on the
    /// next frame rather than whenever something else happens to wake it.
    ///
    /// `pub(crate)` for the same reason [`list_resumable`] is: the dashboard
    /// pairs this with a [`kill_session`] made *off* the UI thread through an
    /// `Arc<RemoteBackend>` clone, bypassing the `Backend` seam — see
    /// `run::start_kill`. [`Backend::presume_killed`] is the seam-level spelling.
    ///
    /// [`list_resumable`]: Self::list_resumable
    /// [`kill_session`]: Self::kill_session
    pub(crate) fn presume_dead(&self, key: &SessionKey) {
        self.presumed_dead
            .lock()
            .unwrap()
            .insert(key.clone(), Instant::now());
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Withdraw the presumption — the session is alive after all, so put its row
    /// back. Idempotent: a presumption already dropped by a confirming `Removed`
    /// (or lapsed) leaves nothing to undo.
    fn unpresume_dead(&self, key: &SessionKey) {
        if self.presumed_dead.lock().unwrap().remove(key).is_some() {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Show `key`'s row as held by another terminal, on the strength of an
    /// attach this host refused as busy. See [`presumed_attached`] for why the
    /// refusal is worth more than a query, and why this presumption never
    /// lapses.
    ///
    /// [`presumed_attached`]: Self::presumed_attached
    fn presume_attached(&self, key: &SessionKey) {
        self.presume_attached_bit(key, true);
    }

    /// The other direction: an attach of *ours* has ended, so nobody holds the
    /// pty until someone attaches again. See [`presumed_attached`].
    ///
    /// [`presumed_attached`]: Self::presumed_attached
    fn presume_detached(&self, key: &SessionKey) {
        self.presume_attached_bit(key, false);
    }

    fn presume_attached_bit(&self, key: &SessionKey, attached: bool) {
        if self
            .presumed_attached
            .lock()
            .unwrap()
            .insert(key.clone(), attached)
            != Some(attached)
        {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// The remote Claude name-manifest index isn't served; remote rows get
    /// their titles from `name`/`first_prompt`, which the remote server stamps
    /// onto every session it pushes. So the index is empty for a remote host.
    fn session_index(&self) -> SessionIndex {
        SessionIndex::default()
    }

    /// `pub(crate)` because the dashboard calls it *off* the UI thread through
    /// an `Arc<RemoteBackend>` clone, bypassing the `Backend` seam — see
    /// `run::start_resume_load`. Blocking: it is an ssh round trip.
    pub(crate) fn list_resumable(&self, limit: usize) -> (Vec<ResumeCandidate>, Vec<String>) {
        match self.request(|req_id| ClientFrame::ListResumable { req_id, limit }) {
            Some(ServerFrame::Resumable {
                candidates, errors, ..
            }) => (candidates, errors),
            _ => (Vec::new(), vec!["remote host unreachable".to_string()]),
        }
    }

    /// Blocking: an ssh round trip, so the dashboard makes it from a pool thread
    /// through an `Arc` clone (hence `pub(crate)`, as for [`list_resumable`]).
    /// Callers hold the row's [`presume_dead`] over this, so nothing about the
    /// wait is visible in the table.
    ///
    /// [`list_resumable`]: Self::list_resumable
    /// [`presume_dead`]: Self::presume_dead
    pub(crate) fn kill_session(&self, key: &SessionKey) -> KillOutcome {
        let key = key.clone();
        match self.request(|req_id| ClientFrame::KillSession { req_id, key }) {
            Some(ServerFrame::Killed { ok: true, .. }) => KillOutcome::Signalled,
            Some(ServerFrame::Killed { ok: false, .. }) => KillOutcome::AlreadyGone,
            // No reply at all — `request` fails fast on a known-down host and
            // otherwise waits out the connection, and a daemon too old to decode
            // the frame simply never answers.
            _ => KillOutcome::Unreachable,
        }
    }

    fn set_session_flags(&self, key: &SessionKey, flags: SessionFlags) -> bool {
        let key = key.clone();
        matches!(
            self.request(|req_id| ClientFrame::SetSessionFlags { req_id, key, flags }),
            Some(ServerFrame::FlagsSet { ok: true, .. })
        )
    }

    /// Ask the server to start a launcher inside its pty pool, then build the
    /// plan for a *local* window that attaches to it. Blocks on the round-trip,
    /// so an async caller should wrap this in `block_in_place`.
    fn open_session(&self, spec: &OpenSpec) -> anyhow::Result<LaunchPlan> {
        let spec = spec.clone();
        let inherit_env = crate::config::get().remote.inherit_env.clone();
        match self.request(|req_id| ClientFrame::OpenSession { req_id, spec }) {
            Some(ServerFrame::Opened {
                session_name: Some(name),
                ..
            }) => Ok(LaunchPlan::AttachRemote {
                argv: attach_argv(
                    self.attach_target.as_deref(),
                    &self.ssh_options,
                    &self.remote_exe.lock().unwrap(),
                    &name,
                    // A session we just created can't already have a client, so
                    // the create path never steals.
                    false,
                    &inherit_env,
                ),
                session_name: name,
            }),
            Some(ServerFrame::Opened { error: Some(e), .. }) => anyhow::bail!(e),
            _ => anyhow::bail!("remote host unreachable"),
        }
    }

    /// The remote host's recent dirs, host-canonical. Blocks on the round-trip;
    /// empty if unreachable.
    fn recent_dirs(&self) -> Vec<String> {
        match self.request(|req_id| ClientFrame::ListRecentDirs { req_id }) {
            Some(ServerFrame::RecentDirs { cwds, .. }) => cwds,
            _ => Vec::new(),
        }
    }

    /// Ask the remote host to forget `cwd`. Returns at once: the round trip runs
    /// on a background task, because the caller is the UI thread mid-keystroke
    /// and the far end is across an ssh link. See [`Backend::forget_recent_dir`]
    /// for why nothing waits on the answer.
    fn forget_recent_dir(self: &Arc<Self>, cwd: &str) {
        let backend = self.clone();
        let cwd = cwd.to_string();
        tokio::spawn(async move {
            let host = backend.host.0.clone();
            match backend
                .request_within(FORGET_RECENT_DIR_TIMEOUT, |req_id| {
                    ClientFrame::ForgetRecentDir { req_id, cwd }
                })
                .await
            {
                Some(ServerFrame::RecentDirForgotten { ok: true, .. }) => {}
                Some(ServerFrame::RecentDirForgotten { ok: false, .. }) => {
                    tracing::debug!("{host} had no such recent dir to forget");
                }
                // Unreachable, or a daemon too old to know the frame — it
                // ignores what it can't decode, so the answer is silence and
                // the deadline is what ends the wait. The entry is already off
                // this dashboard's list and comes back on the next re-seed.
                _ => tracing::debug!("{host} did not confirm forgetting a recent dir"),
            }
        });
    }

    /// Directory completions on the remote fs. Blocks; empty if unreachable.
    fn complete_path(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_string();
        match self.request(|req_id| ClientFrame::CompletePath { req_id, prefix }) {
            Some(ServerFrame::PathCompletions { matches, .. }) => matches,
            _ => Vec::new(),
        }
    }

    /// Whether `path` is a directory on the remote fs. Blocks; `false` if
    /// unreachable (the picker surfaces the disconnect separately).
    fn dir_exists(&self, path: &str) -> bool {
        let path = path.to_string();
        matches!(
            self.request(|req_id| ClientFrame::CheckDir { req_id, path }),
            Some(ServerFrame::DirChecked { exists: true, .. })
        )
    }
}

/// The shell script that wraps an attach so the window reports its own end —
/// **and holds the window itself when the attach was refused on arrival.**
///
/// Positional parameters (`sh -c SCRIPT sh <exe> <host> <token> <grace> <argv…>`)
/// rather than interpolation: the attach argv holds ssh options and a session
/// name, and splicing any of that into a script is how quoting bugs become
/// command injection. Nothing here is substituted — the text is a constant.
///
/// **`HUP` reports 129 outright instead of `$?`, and that is what makes
/// `[remote] on_window_close` work at all.** A terminal can end a window two
/// ways and only one of them signals the attach: it may `killpg(SIGHUP)` the
/// foreground group — the child dies of the signal, so `$?` is 129 — or it may
/// just close the pty master, which SIGHUPs the *session leader alone* (POSIX'
/// controlling process). This wrapper is that leader. ssh is then never
/// signalled; it finds its tty gone and exits **255**, the very status a dropped
/// link produces. Inheriting `$?` therefore reported a deliberate window close
/// as a network failure on any terminal taking the second route, and the session
/// was detached instead of ended. The signal *this* process receives is the one
/// fact both routes agree on, so `HUP` names its own status and `$?` is left to
/// the ends that really are the attach's own.
///
/// The handler still runs late — a shell defers a trap until the foreground
/// command it is waiting on returns — but it runs *first*, ahead of the `r "$c"`
/// on the normal path, and the `$d` latch makes whoever reports first the only
/// one who reports. That latch is equally what stops the surviving `EXIT` trap
/// from sending a second report as the script unwinds.
///
/// `r $?` passes the attach's exit status as the handler's *first* expansion,
/// before anything else can overwrite it. The dashboard uses it to tell an
/// attach that ran and ended from one that was refused on arrival.
///
/// **The hold is the wrapper's job, not the terminal's** — a `--hold` window on
/// Kitty is not a frozen corpse: kitty rewrites the command to `kitten run-shell
/// … -- <cmd>` and **runs the user's login shell** once it exits (`--hold`'s own
/// documentation: "at a shell prompt. The shell will be run after the launched
/// command exits"). So every ended attach turned into a live local shell wearing
/// a session's title — a fish prompt where an agent used to be, most visibly
/// after a laptop sleep drops every ssh at once. Holding here instead makes the
/// window's fate a property of the attach on all three backends: refused → stay
/// with the error on screen and an obviously dead window, anything else → exit
/// and let the window close.
///
/// The refusal test mirrors `app::attach_window_is_spent`, whose doc carries the
/// reasoning (ssh reports a mid-session drop and a failure to connect with the
/// same 255, so status alone can't decide). `$g` is that function's grace,
/// passed in rather than duplicated as a literal. The elapsed seconds are wall
/// clock — `date`, not the dashboard's monotonic binding age, which stops during
/// a suspend and would read an overnight attach as a refusal.
const ATTACH_REPORT_SCRIPT: &str = "e=$1; h=$2; t=$3; g=$4; shift 4; \
     s=$(date +%s); \
     r() { q=$1; if [ -z \"$d\" ]; then d=1; if [ -n \"$e\" ]; then \
     \"$e\" attach-exited --host \"$h\" --token \"$t\" --status \"$q\" \
     --held-secs \"$(( $(date +%s) - s ))\"; fi; fi; }; \
     trap 'r 129' HUP; trap 'r $?' EXIT INT TERM; \
     \"$@\"; c=$?; r \"$c\"; n=$(( $(date +%s) - s )); \
     if [ \"$c\" -ne 0 ] && [ \"$c\" -ne 129 ] && [ \"$c\" -ne 130 ] \
     && [ \"$c\" -ne 143 ] && [ \"$n\" -lt \"$g\" ]; then \
     printf '\\n[captain-miao] attach to %s exited with status %s. \
Press Enter to close this window.\\n' \"$t\" \"$c\"; read x; fi; \
     exit \"$c\"";

/// This dashboard's own binary, for the attach wrapper to re-invoke as
/// `miao attach-exited`. `None` when it can't be named, which costs the report
/// and nothing else — the attach spawns unwrapped and the periodic prune covers
/// it.
pub(crate) fn reporter_exe() -> Option<String> {
    resolve_reporter_exe(std::env::current_exe().ok()?, |p| p.exists())
}

/// The `(deleted)` guard, split out from the environment so it is testable.
///
/// `/proc/self/exe` resolves to the running *inode*, so the moment the binary on
/// disk is replaced — every `cargo build` while the dashboard is up, i.e. the
/// entire dev loop — Linux reports the original path with a literal
/// `" (deleted)"` appended (documented on `std::env::current_exe`). Handing that
/// to the wrapper produces a path that cannot be executed, and the report would
/// then silently never arrive: the exact configuration in which someone is most
/// likely to be *testing* the report.
///
/// Stripping the suffix is right rather than merely convenient: the path is
/// re-executed at trap time, minutes or hours later, so what matters is what
/// lives there *then* — which after a rebuild is the new binary, carrying the
/// same subcommand. The existence check is what keeps a genuinely deleted binary
/// (a moved install, a `cargo clean`) from being spliced into the wrapper.
fn resolve_reporter_exe(exe: PathBuf, exists: impl Fn(&Path) -> bool) -> Option<String> {
    // A non-UTF-8 path can't ride in the argv we build; treat it as unnameable.
    let raw = exe.to_str()?;
    // The literal path wins when it is really there — the suffix is only ever a
    // *guess* that the kernel appended it, and a file may legitimately carry it.
    if exists(Path::new(raw)) {
        return Some(raw.to_string());
    }
    let stripped = raw.strip_suffix(" (deleted)")?;
    exists(Path::new(stripped)).then(|| stripped.to_string())
}

/// Wrap an attach argv in [`ATTACH_REPORT_SCRIPT`], so the window reports back
/// when the attach ends — giving the dashboard an *event* for detachment instead
/// of a periodic window-tree snapshot (`cm_core::state::DetachReport`) — and
/// holds itself open when the attach was refused on arrival.
///
/// `exe` is this dashboard's own binary ([`reporter_exe`]), re-invoked as
/// `miao attach-exited`; it is passed rather than re-derived so the caller owns
/// the failure case. `None` reaches the script as an **empty** `$e`, which skips
/// the report and nothing else: the wrapper still has to run, because it is what
/// keeps a refused attach's error on screen now that the terminal's own `--hold`
/// is not used. The periodic prune covers the missing report.
pub(crate) fn report_on_exit_argv(
    argv: Vec<String>,
    exe: Option<&str>,
    host: &str,
    token: &str,
) -> Vec<String> {
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        ATTACH_REPORT_SCRIPT.to_string(),
        // `$0`. Names the wrapper in `ps`, and is never executed.
        "miao-attach".to_string(),
        exe.unwrap_or_default().to_string(),
        host.to_string(),
        token.to_string(),
        crate::app::ATTACH_STARTUP_GRACE.as_secs().to_string(),
    ];
    wrapped.extend(argv);
    wrapped
}

/// The argv for the window that attaches to a pool session: over ssh for a
/// remote host (`ssh -t <target> miao-server attach <name>`), or directly for
/// a same-host socket transport (`miao-server attach <name>`). `-t` forces a
/// pty so the agent's TUI renders. `force` steals the session from whatever
/// client currently holds it (§10.2).
///
/// The ssh form rides the **same `ControlMaster`** the connection task already
/// established (§4), so opening an attach window skips authentication entirely
/// — instant, and no 2FA re-prompt. The deliberate cost is shared fate: OpenSSH
/// multiplexes every channel over the master's single TCP connection, so if the
/// master dies all of this host's attach windows detach at once. That's benign
/// (the pooled sessions survive; each window is one `Enter` to reattach) and
/// worth the latency.
fn attach_argv(
    target: Option<&str>,
    options: &[String],
    remote_exe: &str,
    session_name: &str,
    force: bool,
    inherit_env: &[String],
) -> Vec<String> {
    let mut argv = match target {
        Some(t) => {
            let mut v = vec!["ssh".to_string(), "-t".to_string()];
            v.extend(ssh_common_opts(&state::ssh_control_path(t), options));
            v.push(t.to_string());
            v.push(remote_exe.to_string());
            v
        }
        None => vec![remote_exe.to_string()],
    };
    argv.push("attach".to_string());
    if force {
        argv.push("--force".to_string());
    }
    for name in inherit_env {
        argv.push("--inherit-env".to_string());
        argv.push(name.clone());
    }
    argv.push(session_name.to_string());
    argv
}

/// Binaries able to attach a terminal to *this machine's* pty pool, best first.
///
/// Only ever a *fallback* — see [`local_attach_exe`], which normally names the
/// binary exactly. They are interchangeable when it comes to that: `miao-client`
/// exists for this, `miao-server attach` is the same primitive over the same
/// socket, and both go through the same stale/busy pre-guards and exit with the
/// same `ATTACH_EXIT_*` codes the dashboard reads back off the wrapper
/// (`App::refused_attach`). `miao-client` is preferred only because it is the
/// smaller thing to have installed.
const LOCAL_ATTACH_EXES: [&str; 2] = ["miao-client", "miao-server"];

/// The argv for a window that attaches to a pool session on **this** machine,
/// for a dashboard whose own backend is the direct-local one.
///
/// Same shape as the pooled-localhost attach ([`attach_argv`] with no ssh
/// target), because it is the same operation: the pool is a per-user,
/// per-machine socket, so what reaches it is a question about the binaries on
/// this box and not about which backend happens to be drawing the row.
fn local_attach_argv(session_name: &str, force: bool) -> anyhow::Result<Vec<String>> {
    let exe = local_attach_exe(session_name).ok_or_else(|| {
        anyhow::anyhow!(
            "no pool client on this machine — install {} beside `miao` or on PATH",
            LOCAL_ATTACH_EXES
                .iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    })?;
    // Never any `--inherit-env`: this is a reattach (which cannot forward
    // anything — see [`Backend::attach_plan`]), and `exe` here may well be
    // `miao-client`, whose `attach` subcommand has no such flag and would exit
    // 2 on a clap usage error rather than opening the window.
    Ok(attach_argv(None, &[], &exe, session_name, force, &[]))
}

/// The binary to run for an attach to `session_name` on this machine.
///
/// **Ask the pool name.** A pool session is `cm-<agent>-<daemon-pid>-<seq>`
/// ([`state::pool_session_daemon_pid`]), and that pid is not decoration: the
/// pool lives *inside* that process, so the daemon it names is the one holding
/// the pty, minting the names, and writing the state files the attach guards
/// read back. `/proc/<pid>/exe` is therefore not a good guess at the right
/// binary — it *is* the right binary, by construction, and no search can be
/// more authoritative than that.
///
/// The searched fallback below is a guess, and the bug that put this function
/// in this shape is what a stale guess costs. A dev tree had a current `miao`
/// and a nine-day-old `miao-server` beside it (cargo rebuilds what you ask for,
/// not the workspace), so "the binaries install together" — true of an install,
/// false of `target/` — picked a daemon that predated an agent backend. Its
/// `LauncherState` no longer parsed, and `read_all_launcher_states` **skips a
/// row it cannot parse**: every session vanished at once and the stale-name
/// guard refused a perfectly live session, blaming the session. Reading the
/// daemon's own `/proc` entry cannot go wrong that way, because there is
/// nothing left to be wrong about.
///
/// The fallback still earns its place: a hand-set `--pool-session` encodes no
/// pid, and `/proc` is Linux's. Both are cases where nothing better exists.
fn local_attach_exe(session_name: &str) -> Option<String> {
    daemon_exe_for_pool_session(session_name).or_else(search_local_attach_exe)
}

/// The executable behind the daemon whose pid `session_name` carries, via
/// `/proc/<pid>/exe`.
///
/// Runs the answer through [`resolve_reporter_exe`] for the same `(deleted)`
/// reason it exists: `/proc/<pid>/exe` resolves to the running *inode*, so a
/// daemon whose binary has since been replaced (every upgrade, which lands on a
/// fresh inode by design) reads back with a literal `" (deleted)"` appended.
/// What is at that path *now* is what the window will execute, so the suffix is
/// stripped and the path re-checked rather than handed on unusable.
fn daemon_exe_for_pool_session(session_name: &str) -> Option<String> {
    let pid = state::pool_session_daemon_pid(session_name)?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    resolve_reporter_exe(exe, |p| p.is_file())
}

/// Look for a [`LOCAL_ATTACH_EXES`] entry beside the dashboard, then on `PATH`.
fn search_local_attach_exe() -> Option<String> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    resolve_local_attach_exe(sibling.iter().chain(path.iter()), |p| p.is_file())
}

/// The search, split out from the environment so it is testable.
///
/// The absolute path is returned rather than the bare name so the window's argv
/// says which binary it ran — a `PATH` the terminal spawns with need not be the
/// one the dashboard was started under, and when an attach does go wrong the
/// first question is which binary answered.
fn resolve_local_attach_exe<'a>(
    dirs: impl IntoIterator<Item = &'a PathBuf>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<String> {
    for dir in dirs {
        for name in LOCAL_ATTACH_EXES {
            let candidate = dir.join(name);
            if is_file(&candidate) {
                return candidate.to_str().map(str::to_string);
            }
        }
    }
    None
}

/// The argv for a window that opens an interactive login shell on a remote host
/// in `cwd`, over ssh (the `w` work tab), sharing the ControlMaster like
/// [`attach_argv`]. `-t` forces a pty so the shell is interactive; the `cd`
/// lands in the session's workdir, then we hand off to the user's login shell
/// (falling back to `/bin/sh`).
///
/// **The body must go through [`login_shell_safe`]**, and getting that wrong is
/// what made `w` on a remote row flash a window open and shut. `ssh host <cmd>`
/// hands the string to the *account's* login shell, so the `${SHELL:-/bin/sh}`
/// default-expansion this needs is a syntax error the moment that shell is
/// `fish` — the window dies before it draws. Wrapping in `/bin/sh -c '…'` puts
/// the expansion in front of a shell that speaks it.
///
/// The wrapped script may contain no single quote and no backslash (see
/// [`login_shell_safe`]), which is why the directory is *not* interpolated into
/// it: `shell_quote_host_path` emits `'…'`. It rides as a positional argument
/// **outside** the wrapper — where single quotes are literal in every dialect —
/// and the script reads it as `$0`. That also keeps the tilde working: `cwd` is
/// **host-canonical** (§3), so a `~` form reaches the remote as a `"$HOME"` the
/// login shell expands, where plain `'…'` quoting would render it inert. An
/// empty `cwd` just drops the `cd`. Pure + unit-tested.
fn remote_shell_argv(target: &str, options: &[String], cwd: &str) -> Vec<String> {
    let remote_cmd = if cwd.is_empty() {
        login_shell_safe("exec \"${SHELL:-/bin/sh}\" -l")
    } else {
        format!(
            "{} {}",
            login_shell_safe("cd \"$0\" && exec \"${SHELL:-/bin/sh}\" -l"),
            cm_core::paths::shell_quote_host_path(cwd)
        )
    };
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    argv.extend(ssh_common_opts(&state::ssh_control_path(target), options));
    argv.push(target.to_string());
    argv.push(remote_cmd);
    argv
}

/// Backoff bounds for reconnecting a dropped remote connection.
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// A connection must have lasted at least this long to count as "healthy" and
/// reset the backoff. Without this gate a link that drops right after subscribe
/// (a flapping tunnel, a crash-looping daemon) would reset to 500ms every cycle
/// and hammer the host with a reconnect storm (~4 ssh subprocesses per attempt).
const RECONNECT_HEALTHY: Duration = Duration::from_secs(20);

/// How one `serve` session ended, telling [`connection_task`] how to proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServeOutcome {
    /// The `RemoteBackend` was dropped (request channel closed) — stop for good.
    BackendDropped,
    /// A subscribed connection was lost (EOF / read / write error) — reconnect
    /// promptly (a healthy link just dropped; reset the backoff).
    ConnectionLost,
    /// The handshake/subscribe never completed — reconnect, but keep backing off
    /// (an incompatible or absent server shouldn't hot-loop). A `Some` reason is
    /// diagnosable and becomes the host's `ConnState::Failed` text.
    HandshakeFailed(Option<String>),
}

/// The handles [`connection_task`] shares with its [`RemoteBackend`]. Grouped
/// into a struct rather than passed as seven positional `Arc`s, where the two
/// `Arc<Mutex<Option<String>>>`s would be swappable at a call site.
struct ConnectionShared {
    /// Which host this task serves. Needed so a download prompt can name it —
    /// the user may have several, and "may I download a server?" is not a
    /// question worth asking without saying for whom.
    host: HostId,
    mirror: Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    presumed_dead: Arc<Mutex<HashMap<SessionKey, Instant>>>,
    presumed_attached: Arc<Mutex<HashMap<SessionKey, bool>>>,
    remote_exe: Arc<Mutex<String>>,
    conn: Arc<Mutex<ConnState>>,
    dirty: Arc<AtomicBool>,
    mirrored: Arc<AtomicBool>,
    server_version: Arc<Mutex<Option<String>>>,
    upgrade: Arc<Mutex<Option<UpgradeOffer>>>,
    latency: Arc<Mutex<Option<Duration>>>,
    vitals: Arc<VitalsCell>,
    reconnect_epoch: Arc<AtomicU64>,
    log: Arc<ConnLog>,
}

/// Own a [`RemoteBackend`]'s connection for its whole lifetime, reconnecting on
/// loss. Each iteration establishes the transport, then [`serve`]s one connection
/// (handshake → subscribe → multiplex the pushed stream into the mirror with
/// request/response by `req_id`). On loss it clears the mirror, marks the host
/// disconnected, and retries with exponential backoff — until [`serve`] reports
/// the `RemoteBackend` was dropped, when the task exits.
async fn connection_task(
    transport: Transport,
    shared: ConnectionShared,
    mut requests: mpsc::UnboundedReceiver<PendingRequest>,
) {
    let ConnectionShared {
        host,
        mirror,
        presumed_dead,
        presumed_attached,
        remote_exe,
        conn,
        dirty,
        mirrored,
        server_version,
        upgrade,
        latency,
        vitals,
        reconnect_epoch,
        log,
    } = shared;
    // A connection-state change flips `dirty` alongside `conn` so the dashboard
    // reloads + redraws the header promptly on connect/disconnect, not only when
    // the mirror later changes.
    let store = |s: ConnState| {
        *conn.lock().unwrap() = s;
        dirty.store(true, Ordering::Relaxed);
    };
    let mut backoff = RECONNECT_INITIAL;
    let mut was_connected = false;
    // Lives for the whole task, so one host's refusal to accept a deployed
    // server isn't re-litigated (at multiple megabytes a go) on every reconnect.
    let mut upload_gate = UploadGate::default();
    // A separate gate for downloads, keyed by URL rather than digest — a
    // payload we have not fetched has no digest yet. Same shape and the same
    // reason: a decline or a 404 must not be re-attempted every backoff tick.
    let mut download_gate = UploadGate::default();
    // Deliberately absent from the `clear()` below — see `Provisioning::terminfo`.
    let mut terminfo_gate = UploadGate::default();
    // The probe answer a reconnect may reuse instead of re-interrogating a host
    // we were talking to a moment ago (`PROBE_REUSE_WINDOW`). Dropped by every
    // path below that did *not* reach a served connection: the probe is how a
    // host that has started failing gets diagnosed, so anything short of
    // "this worked" has to ask again.
    let mut probe_cache = None;
    // The diagnosis the last attempt reached, held across the wait *and* the
    // next attempt. Retrying doesn't make "no miao-server on the host" any less
    // true, so blinking the sentence off to `connecting` once per backoff tick
    // only makes it unreadable — the reason stands until an attempt concludes
    // something else. Every path that loops sets this beside the state it
    // stores, so at the top of each pass `Some` means the stored state is
    // already the matching `Failed` — which is why the re-dial can skip its own
    // store rather than re-announce the same sentence.
    let mut standing_failure: Option<String> = None;
    loop {
        if standing_failure.is_none() {
            store(ConnState::Connecting);
        }
        // Establish the transport; for ssh, (re)stand up the forward+server
        // child. Re-running `setup_ssh` on each attempt is deliberate: it also
        // re-cancels any stale ControlMaster forward, which is what makes a
        // reconnect actually bind its socket.
        let mut failure: Option<String> = None;
        let established = match &transport {
            Transport::LocalSocket(p) => {
                log.info(format!("connecting to local socket {}", p.display()));
                Some((p.clone(), None))
            }
            Transport::Ssh {
                target,
                local_sock,
                options,
                clipboard,
            } => {
                log.info(format!("connecting to {target} over ssh"));
                match setup_ssh(
                    SshLink {
                        target,
                        local_sock,
                        options,
                        clipboard: *clipboard,
                    },
                    &remote_exe,
                    &upgrade,
                    &mut failure,
                    &mut Provisioning {
                        upload: &mut upload_gate,
                        download: &mut download_gate,
                        terminfo: &mut terminfo_gate,
                        probe: &mut probe_cache,
                        host: &host,
                    },
                    &log,
                )
                .await
                {
                    Some(child) => Some((local_sock.clone(), Some(child))),
                    None => {
                        tracing::warn!(target: "captain_miao::ssh", "{target}: ssh setup failed — will retry");
                        None
                    }
                }
            }
        };
        let Some((sock_path, ssh_child)) = established else {
            // A diagnosable cause (server missing, version mismatch, host
            // unreachable) is surfaced verbatim instead of a bare ⚠ (§4). The
            // task keeps retrying either way — `Failed` is a *label*, not a
            // terminal state, since deploying the binary should heal it without
            // the user restarting anything.
            standing_failure = failure;
            probe_cache = None;
            match &standing_failure {
                Some(reason) => log.error(format!("could not set the connection up: {reason}")),
                None => log.error("could not set the connection up (no diagnosis)"),
            }
            store(match &standing_failure {
                Some(reason) => ConnState::Failed(reason.clone()),
                None => ConnState::Disconnected,
            });
            log.info(format!("retrying in {}s", backoff.as_secs_f32().round()));
            if !wait_before_retry(&mut requests, &mut backoff).await {
                return;
            }
            continue;
        };
        // The ssh server binds its socket a beat after the forward is up, so
        // retry the first connect; a direct socket needs only a couple of tries.
        let attempts = if ssh_child.is_some() { 16 } else { 3 };
        let Some(stream) = connect_with_retry(&sock_path, attempts).await else {
            drop(ssh_child); // kill_on_drop tears ssh down
            // Setup got this far without a diagnosis, so an older one is stale.
            standing_failure = None;
            probe_cache = None;
            log.error(format!(
                "the daemon socket never answered at {} ({attempts} attempts)",
                sock_path.display()
            ));
            store(ConnState::Disconnected);
            log.info(format!("retrying in {}s", backoff.as_secs_f32().round()));
            if !wait_before_retry(&mut requests, &mut backoff).await {
                return;
            }
            continue;
        };
        tracing::debug!(target: "captain_miao::ssh", "connected to {}; serving", sock_path.display());
        // A Disconnected → Connected edge bumps the epoch, which is what the
        // dashboard's auto-reattach sweep watches (§7): after a laptop sleep or
        // a broken pipe, every session that *had* an attach window gets one
        // again, without the user re-Entering each row.
        if was_connected {
            reconnect_epoch.fetch_add(1, Ordering::Relaxed);
        }
        was_connected = true;
        log.info("connected");
        store(ConnState::Connected);
        let connected_at = Instant::now();
        let outcome = serve(
            stream,
            MirrorCells {
                mirror: &mirror,
                presumed_dead: &presumed_dead,
                presumed_attached: &presumed_attached,
                dirty: &dirty,
                mirrored: &mirrored,
                server_version: &server_version,
            },
            &mut requests,
        )
        .await;
        // Forget remembered deploy failures and refusals only once the host has
        // *demonstrably* worked — which means the handshake and subscribe both
        // succeeded, not merely that a socket accepted us. Clearing at connect
        // time wiped a recorded decline on every connect-then-handshake-refused
        // cycle, which is exactly the host that keeps re-prompting.
        if matches!(outcome, ServeOutcome::ConnectionLost) {
            upload_gate.clear();
            download_gate.clear();
        } else {
            // A refused handshake is the one thing the probe would explain — a
            // server below the protocol floor is exactly what a fresh probe
            // turns into a deploy — so this is the last pass that may reuse one.
            probe_cache = None;
        }
        drop(ssh_child); // explicit: kill the ssh child once the connection ends
        // The mirror is now stale; clear it so the host shows no (misleading)
        // rows while disconnected. A fresh `Snapshot` refills it on reconnect.
        // `store(Disconnected)` below flips `dirty` so the cleared rows redraw.
        mirror.lock().unwrap().clear();
        // Cleared *with* the mirror, always: the two answer the same question,
        // and a `mirrored` left standing over an empty mirror is a host claiming
        // to have reported no sessions when it has reported nothing at all.
        mirrored.store(false, Ordering::Relaxed);
        // And with it every presumption: each one says "this row is on its way
        // out", which only means anything against rows we still have. Carrying
        // them across the gap would let one hide a session the reconnect's
        // snapshot reports as alive.
        presumed_dead.lock().unwrap().clear();
        presumed_attached.lock().unwrap().clear();
        *latency.lock().unwrap() = None;
        vitals.invalidate();
        standing_failure = match &outcome {
            ServeOutcome::HandshakeFailed(Some(reason)) => Some(reason.clone()),
            _ => None,
        };
        match (&standing_failure, &outcome) {
            (Some(reason), _) => log.error(format!("handshake refused: {reason}")),
            (None, ServeOutcome::BackendDropped) => log.info("host removed; stopping"),
            (None, _) => log.error(format!(
                "connection lost after {}s",
                connected_at.elapsed().as_secs()
            )),
        }
        store(match &standing_failure {
            Some(reason) => ConnState::Failed(reason.clone()),
            None => ConnState::Disconnected,
        });
        tracing::debug!(
            target: "captain_miao::ssh",
            "serve loop ended for {} ({outcome:?})", sock_path.display()
        );
        match outcome {
            ServeOutcome::BackendDropped => return,
            // Reset the backoff only if the connection was actually healthy for a
            // while — a link that dropped seconds after subscribing keeps backing
            // off, so a flapping host doesn't trigger a reconnect storm.
            ServeOutcome::ConnectionLost if connected_at.elapsed() >= RECONNECT_HEALTHY => {
                backoff = RECONNECT_INITIAL;
            }
            ServeOutcome::ConnectionLost | ServeOutcome::HandshakeFailed(_) => {}
        }
        if !wait_before_retry(&mut requests, &mut backoff).await {
            return;
        }
    }
}

/// Wait out the current `backoff` before the next reconnect, then double it
/// (capped at [`RECONNECT_MAX`]). Returns `false` if the backend was dropped
/// meanwhile (its request channel closed) — the caller should terminate. Any
/// request that races in while we wait is failed immediately (its reply sender
/// is dropped → the caller sees the host as unreachable) rather than left to
/// hang for the whole backoff.
async fn wait_before_retry(
    requests: &mut mpsc::UnboundedReceiver<PendingRequest>,
    backoff: &mut Duration,
) -> bool {
    let this = *backoff;
    *backoff = (*backoff * 2).min(RECONNECT_MAX);
    let sleep = tokio::time::sleep(this);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            req = requests.recv() => {
                // A request racing in while we're down is failed immediately:
                // taking it here (`req` then drops) closes its reply sender, so
                // the caller sees the host as unreachable instead of blocking for
                // the whole backoff. `None` means the backend itself was dropped.
                if req.is_none() {
                    return false;
                }
            }
        }
    }
}

/// Bounds on [`connect_with_retry`]'s ramp. The floor is short because a
/// `connect` to a path that isn't there yet is an instant `ENOENT` — the cost of
/// asking early is nil, and the socket usually appears within a round trip of
/// the master binding the forward. The ceiling is where a flat wait is the right
/// shape anyway, once "not yet" has stopped meaning "any moment now".
const CONNECT_RETRY_FLOOR: Duration = Duration::from_millis(25);
const CONNECT_RETRY_CEILING: Duration = Duration::from_millis(400);

/// The waits between [`connect_with_retry`]'s attempts: one fewer than the
/// attempts themselves, since the last one has nothing to wait for.
///
/// Pure, so the schedule's shape *and* its total budget are pinned without
/// sleeping through them — the budget being the number a reader actually wants,
/// because it is what every failing attempt adds to the reconnect backoff before
/// the backoff has even started.
fn connect_retry_delays(attempts: u32) -> impl Iterator<Item = Duration> {
    (0..attempts.saturating_sub(1)).scan(CONNECT_RETRY_FLOOR, |d, _| {
        let this = *d;
        *d = (*d * 2).min(CONNECT_RETRY_CEILING);
        Some(this)
    })
}

/// Try to connect to `sock` a few times, sleeping between attempts.
///
/// Ramped rather than flat. The socket answers on the first attempt or two in
/// the ordinary case, so a flat wait paid its full price exactly where it was
/// least needed; and where it *is* needed the ramp still reaches the same
/// ceiling within five attempts.
async fn connect_with_retry(sock: &Path, attempts: u32) -> Option<UnixStream> {
    let mut last_err = None;
    let mut delays = connect_retry_delays(attempts);
    for _ in 0..attempts {
        match UnixStream::connect(sock).await {
            Ok(s) => return Some(s),
            Err(e) => last_err = Some(e),
        }
        if let Some(delay) = delays.next() {
            tokio::time::sleep(delay).await;
        }
    }
    tracing::warn!(
        target: "captain_miao::ssh",
        "could not connect to forwarded socket {} after {attempts} attempts: {:?}",
        sock.display(), last_err
    );
    None
}

/// The [`Transport::Ssh`] fields [`setup_ssh`] dials with, borrowed as a group.
/// Grouped for the same reason as [`ConnectionShared`]: they would otherwise be
/// three more positional parameters, two of them `&str`-ish enough to swap
/// silently at the one call site.
struct SshLink<'a> {
    target: &'a str,
    local_sock: &'a Path,
    /// The host's connection options as typed, split on arrival — see
    /// [`split_connection_options`].
    options: &'a [String],
    /// Offer this host the clipboard — see [`hosts::HostConfig::clipboard`].
    clipboard: bool,
}

/// Stand up an ssh host: ensure the remote daemon is running (and learn its
/// socket path) with `daemon ensure`, then spawn a **forward-only** `ssh -N -L
/// <local>:<remote> target` child that just holds the tunnel. The daemon is
/// self-daemonizing and persistent, so it's fully decoupled from this child —
/// dropping the backend (or a reconnect) kills only the tunnel, never the daemon
/// or its sessions. The returned child is `kill_on_drop`. Returns None if ssh or
/// the remote binary fails. Requires key/agent auth (BatchMode).
///
/// The forwards in the host's `options` ride that same child, so they are up for
/// exactly as long as the host is connected and come back with it on a
/// reconnect. `ExitOnForwardFailure` stays at its default `no` on purpose: a
/// port already in use must cost the user that one forward, not the dashboard's
/// link to the host.
async fn setup_ssh(
    link: SshLink<'_>,
    remote_exe: &Arc<Mutex<String>>,
    upgrade: &Arc<Mutex<Option<UpgradeOffer>>>,
    failure: &mut Option<String>,
    prov: &mut Provisioning<'_>,
    log: &ConnLog,
) -> Option<tokio::process::Child> {
    let SshLink {
        target,
        local_sock,
        options,
        clipboard,
    } = link;
    // `forwards` stays owned until the probe reports the host's `$HOME`: the
    // clipboard forward joins the user's own set, which is what gets it the
    // cancel-then-request on reconnect, the retirement when the host leaves the
    // ssh set, and the `port forwards:` log line for free.
    let (extra, mut forwards) = split_connection_options(options);
    let ctl = crate::state::ssh_control_path(target);
    // ssh's ControlMaster won't create ControlPath's parent dir, and the first
    // ssh below (the probe) already needs it — so ensure the short ssh-socket
    // dir exists (0700, so another user can't hijack the control socket).
    if let Some(dir) = ctl.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // Every ssh call below carries the host's options; only the tunnel child
    // carries its forwards.
    let opts = ssh_common_opts(&ctl, &extra);
    if !options.is_empty() {
        log.info(format!("connection options: {}", options.join(" ")));
    }
    // Ahead of the probe, because the probe is what would otherwise re-join the
    // stale master and make the new options inert — see
    // [`options_changed_since_last_dial`]. Best-effort: a master that is already
    // gone (or was never up) makes this a quiet no-op, and a failure only leaves
    // us where we were.
    if options_changed_since_last_dial(prov.host, target, &extra) {
        log.info(
            "connection options changed — retiring the shared ssh connection so they take effect",
        );
        let mut exit = detached("ssh");
        exit.args(&opts).arg("-O").arg("exit").arg(target);
        bounded_status(exit, MUX_CONTROL_TIMEOUT).await;
    }

    // Probe the host, auto-provision our binary if it's missing/stale and our
    // build can run there (`docs/crate-split.md`), and resolve the command to
    // invoke. Non-fatal: a failure resolves to `miao-server` on PATH, the prior
    // default. A reconnect inside `PROBE_REUSE_WINDOW` answers from the last
    // clean probe instead — which is also why nothing here counts on it having
    // primed the ControlMaster: every ssh below carries `ControlMaster=auto`, so
    // whichever runs first opens it.
    let provisioned = resolve_remote_exe(target, &opts, prov, log).await;
    let exe = provisioned.exe;
    *remote_exe.lock().unwrap() = exe.clone();
    // Re-published on every pass, `None` included: an offer that has since been
    // taken (or a host that stopped being upgradable) must stop being advertised,
    // and a stale `Some` here is a keystroke that kills sessions for nothing.
    *upgrade.lock().unwrap() = provisioned.upgrade;
    // Carry the diagnosis out even when we go on to try the fallback: if the
    // `daemon ensure` below fails, *this* is the reason the user needs, not
    // "connection failed".
    *failure = provisioned.failure;

    // The clipboard bridge: one more `-R`, joining the user's forwards above.
    // Needs the host's `$HOME`, because ssh does no `~`/`$HOME` expansion in a
    // forward spec — so a host whose probe failed gets no offer, which costs
    // nothing since it is about to fail to connect anyway.
    let clipboard_home = clipboard.then_some(provisioned.home.as_deref()).flatten();
    if let Some(home) = clipboard_home {
        let clip_sock = cm_core::clipboard::paths::local_socket_path();
        log.info(format!(
            "offering this machine's clipboard at {} on the host",
            cm_core::clipboard::paths::remote_socket_for_home(home)
        ));
        // The one host where the offer cannot become a `Ctrl+V`: an agent reads a
        // macOS clipboard through `osascript`, which no shim intercepts. Said here
        // because the alternative is a key that silently does nothing, with the
        // panel reporting a forward that is genuinely up.
        if provisioned.darwin {
            log.info(
                "this host is a Mac, so Ctrl+V there cannot reach it — \
                 run `clipboard-paste` in the session instead",
            );
        }
        forwards.push(clipboard_forward(home, &clip_sock));
    } else if clipboard {
        log.error("cannot offer the clipboard: the probe never reported the host's home");
    } else if let Some(home) = provisioned.home.as_deref() {
        // Revoking must not wait for a *clean* connect. `cancel_user_forwards`
        // below would take this down, but it sits under `daemon ensure`'s early
        // return — so a host whose server is broken while its ControlMaster is
        // still up (any open attach window keeps one) would go on reading this
        // machine's clipboard until some later reconnect got further, while the
        // panel says the toggle took effect. Gated on having asked for it, so the
        // ordinary host that never wanted the clipboard pays nothing.
        let stale = clipboard_forward(home, &cm_core::clipboard::paths::local_socket_path());
        if forward_was_requested(prov.host, target, &stale) {
            log.info("no longer offering this machine's clipboard");
            cancel_forwards(target, &opts, &[stale]).await;
        }
    }
    let forwards = forwards.as_slice();

    // Ensure the remote daemon is running AND learn its socket path in one call:
    // `daemon ensure` self-daemonizes if needed (idempotent — a no-op against a
    // live one) and prints the socket path on its first stdout line. This starts
    // the persistent daemon; the separate `-N -L` child below only forwards.
    let child = Command::new("ssh")
        .args(&opts)
        .arg(target)
        .arg(&exe)
        .args(["daemon", "ensure"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    // Bounded on both axes, exactly like the probe that ran a moment ago
    // against this same host: `ensure` answers with one socket path, and
    // nothing about a remote host obliges it to stop talking.
    let (status, stdout, stderr) =
        tokio::time::timeout(ENSURE_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .ok()?
            .ok()?;
    if !status.success() {
        tracing::warn!(
            target: "captain_miao::ssh",
            "{target}: `{exe} daemon ensure` failed (rc={:?}): {}",
            status.code(),
            stderr.trim()
        );
        // The log gets it whole and unelided — this is the sentence the panel
        // row has to cut, and reading all of it is the entire reason `l` exists.
        log.error(format!(
            "`{exe} daemon ensure` failed (rc={:?}):\n{}",
            status.code(),
            stderr.trim()
        ));
        // Keep a provisioning diagnosis if we have one (it's the root cause);
        // otherwise report what the remote actually said.
        if failure.is_none() {
            let detail: String = stderr.trim().chars().take(160).collect();
            *failure = Some(if detail.is_empty() {
                format!(
                    "`daemon ensure` failed on the host (rc={:?})",
                    status.code()
                )
            } else {
                format!("`daemon ensure` failed on the host: {detail}")
            });
        }
        return None;
    }
    // The daemon answered, so nothing is wrong with the install after all.
    *failure = None;
    let remote_sock = stdout.lines().next().unwrap_or_default().trim().to_string();
    if remote_sock.is_empty() {
        tracing::warn!(target: "captain_miao::ssh", "{target}: daemon ensure returned no socket path");
        log.error("`daemon ensure` succeeded but printed no socket path");
        return None;
    }
    log.info(format!("daemon is up, socket {remote_sock}"));
    tracing::debug!(target: "captain_miao::ssh", "{target}: remote daemon socket = {remote_sock}");

    // The persistent ControlMaster can retain a *stale forward* for this local
    // socket path from an earlier connection whose slave was SIGKILL'd (the
    // forward child's `kill_on_drop`), so it never told the master to tear the
    // forward down. A fresh `-L` request for an already-registered path is a
    // silent no-op — the master binds nothing — so every reconnect then fails
    // with ENOENT, self-perpetuating once the first disconnect poisons the
    // master. Cancel any such stale forward first; it's a quiet no-op when none
    // exists (or no master is up). Verified against a real host: without this the
    // forward socket never appears; with it, it binds on the first try.
    let mut cancel = detached("ssh");
    cancel
        .args(&opts)
        .arg("-O")
        .arg("cancel")
        .arg("-L")
        .arg(format!("{}:{}", local_sock.display(), remote_sock))
        .arg(target);
    bounded_status(cancel, MUX_CONTROL_TIMEOUT).await;
    cancel_user_forwards(prov.host, target, &opts, forwards).await;

    // Clear any stale local socket and ensure its parent dir exists.
    if let Some(parent) = local_sock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(local_sock);

    // The same hazard at the far end, and there we cannot run code in the
    // binding process — so it takes a round trip of its own, after the cancel
    // above released the path and before the tunnel asks for it.
    if clipboard_home.is_some() {
        let script = login_shell_safe(CLIPBOARD_PREP_SCRIPT);
        let mut prep = detached("ssh");
        prep.args(&opts).arg(target).arg(&script);
        let prepared = bounded_status(prep, CLIPBOARD_PREP_TIMEOUT).await;
        if !prepared {
            // Not fatal: ssh may still bind (nothing was holding the path), and
            // if it doesn't, its own stderr file names the listen path. Logged
            // because this is the step that explains a bridge that never comes
            // up on an otherwise healthy host.
            log.error(
                "could not prepare the host's clipboard socket path; \
                 the clipboard forward may fail to bind",
            );
        }
    }

    // A forward-ONLY child: `-N` runs no remote command, it just holds the `-L`
    // tunnel open (the daemon is already running and persistent — the tunnel and
    // the daemon are now independent). Killed when the backend drops / on
    // reconnect, with no effect on the daemon. Detached stdin + stdout (must never
    // touch the TUI's terminal), but stderr → a per-host log file: ssh's
    // diagnostics for a failed forward are the only clue when the local socket
    // never appears, and a file (unlike the inherited terminal) can't corrupt the
    // display.
    let safe: String = target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let err_path = state::state_dir()
        .join("logs")
        .join(format!("ssh-forward-{safe}.log"));
    let stderr = std::fs::File::create(&err_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = detached("ssh");
    cmd.args(&opts)
        .arg("-N")
        .arg("-L")
        .arg(format!("{}:{}", local_sock.display(), remote_sock));
    if !forwards.is_empty() {
        // Logged, because a forward that fails to bind only says so in ssh's
        // stderr file — the panel's `l` view should at least show what was asked
        // for, so "why is nothing on :8080" starts from the right spec.
        log.info(format!(
            "port forwards: {}",
            forwards
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for f in forwards {
            cmd.arg(&f.flag).arg(&f.spec);
        }
    }
    cmd.arg(target)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .ok()
}

/// Make the host's clipboard socket path bindable: create its parent dir and
/// remove anything already there.
///
/// **Both halves are load-bearing and neither is optional.** ssh creates no
/// parent for a forward's listen path — `ControlPath` taught us that — and sshd
/// will not rebind a path that already exists. `StreamLocalBindUnlink` defaults
/// to `no`, and setting it on *our* side is inert for `-R`: the
/// `streamlocal-forward@openssh.com` request carries only a path, so the decision
/// belongs to a remote sshd_config we don't control. Measured against a live
/// sshd: without this, a reconnect fails with `remote port forwarding failed for
/// listen path`, the mux client degrades with `disabling multiplexing`, and the
/// bridge is dead — and *with* it, the same sequence binds.
///
/// Run from the **dashboard** rather than from `daemon ensure`, so it works
/// against whatever server version is already deployed: a host still running an
/// older binary would otherwise never get the dir, the forward would never bind,
/// and every diagnosis would point at the wrong thing.
///
/// `"$HOME"` rather than the probed home, even though the forward spec below has
/// to splice that home in: this script is evaluated by a shell, so it can expand
/// the variable itself and survive a home with a space in it, while a forward
/// spec gets no expansion at all. They cannot disagree — the probe's first line
/// *is* `echo "$HOME"` from inside the same `/bin/sh -c`.
///
/// No single quote and no backslash, so it is [`login_shell_safe`]-clean.
const CLIPBOARD_PREP_SCRIPT: &str = concat!(
    "mkdir -p \"$HOME\"/.cache/captain-miao && ",
    "rm -f \"$HOME\"/.cache/captain-miao/clipboard.sock"
);

/// The clipboard bridge as a [`Forward`]: `-R <remote socket>:<local socket>`.
///
/// A synthesized forward rather than a mechanism of its own, because everything a
/// forward needs is already written — it rides the tunnel child, so it is up for
/// exactly as long as the host is connected; it gets cancel-then-request on
/// reconnect from [`cancel_user_forwards`]; it is retired by
/// [`retire_unlisted_forwards`] when the host is suspended, renamed, deleted or
/// switched to a socket transport; and it shows up in the `port forwards:` log
/// line the panel's `l` view already renders.
///
/// The remote path is absolute because ssh expands nothing in a forward spec, and
/// `$HOME`-based because the pool must outlive a login: `/run/user/<uid>` is
/// reaped when a non-lingering user's last session ends. Pure.
fn clipboard_forward(remote_home: &str, local_sock: &Path) -> Forward {
    Forward {
        flag: "-R".to_string(),
        spec: format!(
            "{}:{}",
            cm_core::clipboard::paths::remote_socket_for_home(remote_home),
            local_sock.display()
        ),
    }
}

/// The connection options this process has dialled with — the memo behind
/// [`options_changed_since_last_dial`], from two angles, because one row's
/// history and one target's master answer different halves of the question.
#[derive(Default)]
struct ConnOptionsMemo {
    /// `(host label, ssh target)` → what that panel row last dialled with.
    by_row: HashMap<ForwardKey, Vec<String>>,
    /// ssh target → what the most recent dial to it used, which is the best
    /// available account of what the master there was minted with.
    by_target: HashMap<String, Vec<String>>,
}

static LAST_CONN_OPTIONS: LazyLock<Mutex<ConnOptionsMemo>> = LazyLock::new(Mutex::default);

/// Whether the shared `ControlMaster` for `target` has to be retired before this
/// dial, recording the options either way.
///
/// The `extra` half only, never the forwards: a `-L`/`-R` is requested through
/// the master per connection and [`cancel_user_forwards`] already re-asks for it
/// on every pass, so an edit that only moves a forward needs no teardown.
/// Everything left in `extra` is the opposite — `Port`, `User`, `IdentityFile`,
/// `ProxyJump`, the ciphers, and the `ConnectTimeout`/`ServerAliveInterval`/
/// `ControlPersist` that [`ssh_common_opts`] puts `extra` first to make settable
/// — and every one of them is decided when the TCP connection is *established*.
///
/// **Re-dialling is not enough, which is the whole reason this exists.** An
/// options edit is a genuine drop-and-dial (`ConnIdentity` carries them), and
/// dropping the backend does kill the `-N -L` tunnel — but that child is a mux
/// *slave*. The master was minted by the first ssh of the previous
/// [`setup_ssh`], the probe, which backgrounded itself under `ControlPersist`;
/// the fresh dial's probe then finds that socket still there and joins it as a
/// slave, and every connection-scoped option it was given is inert because the
/// connection it would configure already exists. Not merely for the persist
/// window either: the re-dial is immediate, and an open attach window refreshes
/// the master indefinitely. Only `-O exit` takes it down, so the edit reads as
/// simply not working.
///
/// **A row we have dialled before is judged by its own history; one we have not,
/// by the master's.** Neither angle can be the only one.
/// [`ssh_control_path`](state::ssh_control_path) hashes the target alone, so two
/// panel rows naming the same machine share a master — judge a known row by the
/// master and each sees the other's options as a change, exits the master it is
/// connected through, drops the other's tunnel, and the pair flaps forever. But
/// judge an *unknown* row by its own (empty) history and a rename reads as a
/// first sighting, which the hosts panel makes ordinary: one `Enter` commits
/// every field of a row at once, so changing the label and the options together
/// is a single edit, and the master would keep the options the user just
/// replaced — indefinitely, with the panel showing them as live. Deleting a host
/// and re-adding it under another label is the same shape.
///
/// So a genuinely new row costs at most one extra exit, never a flap, and a
/// first sighting with nothing recorded for the target stays a no-op — the memo
/// is per-process, and exiting on every startup would drop the attach windows a
/// restart means to keep.
///
/// Retiring a master takes down *everyone* on that target, not just the row that
/// asked: another row's attach and `w` windows go with it. That is benign and
/// recoverable — ssh exits 255, which reads as a dropped link rather than a user
/// close, so no session is ended, and the reconnect epoch's re-attach sweep puts
/// the windows back — but it is the reason this is gated as tightly as it is.
fn options_changed_since_last_dial(host: &HostId, target: &str, extra: &[String]) -> bool {
    let mut memo = LAST_CONN_OPTIONS.lock().unwrap();
    let row = memo
        .by_row
        .insert((host.0.clone(), target.to_string()), extra.to_vec());
    let minted = memo.by_target.insert(target.to_string(), extra.to_vec());
    match row {
        Some(row) => row.as_slice() != extra,
        None => minted.is_some_and(|minted| minted.as_slice() != extra),
    }
}

/// Every port-forward spec this process has asked a given ssh target's
/// ControlMaster for, so the next connect can take them back down.
///
/// A forward requested by a multiplexed *client* is registered with the
/// **master**, not with the client's own session — which is why the transport's
/// `-L` needs its own `-O cancel` above, and why a forward the user has since
/// deleted would otherwise hold its port until the master itself expires
/// (`ControlPersist`, refreshed by every attach window). Nothing enumerates a
/// master's live forwards, so remembering what we asked for is the only way to
/// name them again.
///
/// Keyed by `(host label, ssh target)` rather than by target alone: two panel
/// rows may name the same machine, and each has to manage its own set — keyed
/// only by target, connecting one would tear down the other's forwards.
static REQUESTED_FORWARDS: LazyLock<Mutex<HashMap<ForwardKey, Vec<Forward>>>> =
    LazyLock::new(Mutex::default);

/// `(host label, ssh target)` — which panel row's forwards these are, and where
/// they were asked for. See [`REQUESTED_FORWARDS`].
type ForwardKey = (String, String);

/// Whether this process has asked *this* host's master for exactly this forward.
///
/// For cancelling one remembered forward on its own, ahead of the wholesale
/// cancel-then-request in [`cancel_user_forwards`] — which is too late for
/// anything that must come down even when the connect goes on to fail. The memo
/// is left alone: the next successful connect replaces it, and until then a cancel
/// that didn't take is worth retrying.
fn forward_was_requested(host: &HostId, target: &str, f: &Forward) -> bool {
    REQUESTED_FORWARDS
        .lock()
        .unwrap()
        .get(&(host.0.clone(), target.to_string()))
        .is_some_and(|seen| seen.contains(f))
}

/// Cancel every forward this process has requested for this host, including the
/// ones about to be re-requested.
///
/// Cancelling the *current* set too is not waste: a re-request of a forward the
/// master already holds fails, and unlike the transport's unix socket (where the
/// master quietly binds nothing) a mux client treats a refused forward request
/// as fatal — so leaving a live one in place is how a reconnect would kill the
/// very child it just spawned. Cancel-then-request is idempotent; the forward is
/// down only for the moment the connection is being re-established anyway.
///
/// Every failure here is expected and ignorable: no master up yet (the first
/// connect), no such forward (the common case), or an ssh too old to cancel a
/// dynamic one. `detached` swallows the diagnostics.
async fn cancel_user_forwards(host: &HostId, target: &str, opts: &[String], forwards: &[Forward]) {
    let stale = {
        let mut memo = REQUESTED_FORWARDS.lock().unwrap();
        let seen = memo
            .entry((host.0.clone(), target.to_string()))
            .or_default();
        let mut all = std::mem::replace(seen, forwards.to_vec());
        for f in forwards {
            if !all.contains(f) {
                all.push(f.clone());
            }
        }
        all
    };
    cancel_forwards(target, opts, &stale).await;
}

/// Retire the forwards of every host that is no longer asking for one — it was
/// deleted, suspended, renamed, or switched to a socket transport.
///
/// Dropping the backend kills its ssh child, but a forward outlives that child
/// by construction (it belongs to the master, see [`REQUESTED_FORWARDS`]), and
/// any open attach window keeps that master alive indefinitely. Without this,
/// suspending a host would leave its ports answered by a machine the panel says
/// is disconnected — with nothing left running to name the forward and take it
/// back down.
///
/// `live` is `(label, target)` for every host that will still get an ssh
/// backend. Fire-and-forget: the caller is committing a panel edit and must not
/// block on an ssh round trip per forward.
pub(crate) fn retire_unlisted_forwards(live: &[ForwardKey]) {
    let retired: Vec<(ForwardKey, Vec<Forward>)> = {
        let mut memo = REQUESTED_FORWARDS.lock().unwrap();
        let gone: Vec<ForwardKey> = memo.keys().filter(|k| !live.contains(k)).cloned().collect();
        gone.into_iter()
            .filter_map(|k| memo.remove(&k).map(|f| (k, f)))
            .collect()
    };
    for ((_, target), forwards) in retired {
        if forwards.is_empty() {
            continue;
        }
        tokio::spawn(async move {
            let opts = ssh_common_opts(&state::ssh_control_path(&target), &[]);
            cancel_forwards(&target, &opts, &forwards).await;
        });
    }
}

/// One `-O cancel` per spec rather than one command carrying all of them: a
/// single unsupported flag would fail the whole batch, taking the forwards that
/// *could* have been cancelled down with it.
async fn cancel_forwards(target: &str, opts: &[String], forwards: &[Forward]) {
    for f in forwards {
        let mut cmd = detached("ssh");
        cmd.args(opts)
            .arg("-O")
            .arg("cancel")
            .arg(&f.flag)
            .arg(&f.spec)
            .arg(target);
        bounded_status(cmd, MUX_CONTROL_TIMEOUT).await;
    }
}

/// The cells [`serve`] writes as frames arrive, borrowed as a group. Grouped for
/// the same reason as [`SshLink`]: they would otherwise be six more positional
/// parameters, four of them `&Arc<Mutex<…>>` and two `&Arc<AtomicBool>` — types
/// that say nothing at a call site about which is which.
struct MirrorCells<'a> {
    mirror: &'a Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    presumed_dead: &'a Arc<Mutex<HashMap<SessionKey, Instant>>>,
    presumed_attached: &'a Arc<Mutex<HashMap<SessionKey, bool>>>,
    dirty: &'a Arc<AtomicBool>,
    /// Set by the first `Snapshot` — see [`Backend::awaiting_sessions`].
    mirrored: &'a Arc<AtomicBool>,
    server_version: &'a Arc<Mutex<Option<String>>>,
}

/// Handshake, subscribe, then multiplex the pushed session stream into the
/// mirror with request/response, until the peer hangs up or the backend drops.
/// The [`ServeOutcome`] tells the caller whether to reconnect and how fast.
async fn serve(
    stream: UnixStream,
    cells: MirrorCells<'_>,
    requests: &mut mpsc::UnboundedReceiver<PendingRequest>,
) -> ServeOutcome {
    let MirrorCells {
        mirror,
        presumed_dead,
        presumed_attached,
        dirty,
        mirrored,
        server_version,
    } = cells;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // Handshake + subscribe.
    let hello = ClientFrame::Hello {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
    };
    if write_frame(&mut wr, &hello).await.is_err() {
        tracing::warn!(target: "captain_miao::ssh", "failed to send Hello");
        return ServeOutcome::HandshakeFailed(None);
    }
    match read_frame::<_, ServerFrame>(&mut rd).await {
        Ok(Some(ServerFrame::Welcome {
            protocol,
            server_version: sv,
            ..
        })) => {
            // Only a server *below* the floor is refused — a newer one is fine,
            // since both sides decode unknown frames/fields tolerantly (§3).
            if !protocol_compatible(protocol) {
                tracing::warn!(
                    target: "captain_miao::ssh",
                    "server speaks protocol {protocol}, below our floor {PROTOCOL_MIN}"
                );
                return ServeOutcome::HandshakeFailed(Some(incompatible_daemon_reason(
                    &sv, protocol,
                )));
            }
            tracing::debug!(target: "captain_miao::ssh", "handshake ok (protocol {protocol}, server {sv})");
            *server_version.lock().unwrap() = Some(sv);
        }
        // No usable Welcome at all: something is answering the socket that
        // isn't our daemon, or it hung up mid-handshake.
        other => {
            tracing::warn!(target: "captain_miao::ssh", "handshake failed, no usable Welcome: {other:?}");
            return ServeOutcome::HandshakeFailed(None);
        }
    }
    if write_frame(&mut wr, &ClientFrame::Subscribe).await.is_err() {
        tracing::warn!(target: "captain_miao::ssh", "failed to send Subscribe");
        return ServeOutcome::HandshakeFailed(None);
    }

    let mut pending: HashMap<u64, oneshot::Sender<ServerFrame>> = HashMap::new();
    loop {
        tokio::select! {
            frame = read_frame::<_, ServerFrame>(&mut rd) => {
                let frame = match frame {
                    Ok(Some(f)) => f,
                    Ok(None) => { tracing::debug!(target: "captain_miao::ssh", "server closed the stream (EOF)"); return ServeOutcome::ConnectionLost; }
                    Err(e) => { tracing::warn!(target: "captain_miao::ssh", "frame read/parse error: {e}"); return ServeOutcome::ConnectionLost; }
                };
                match frame {
                    ServerFrame::Snapshot { sessions } => {
                        tracing::debug!(target: "captain_miao::ssh", "snapshot: {} sessions", sessions.len());
                        let mut m = mirror.lock().unwrap();
                        m.clear();
                        for s in sessions {
                            m.insert(s.key(), s);
                        }
                        // A full account of the host supersedes every guess we
                        // were making about it (see `presumed_dead`).
                        presumed_dead.lock().unwrap().clear();
                        presumed_attached.lock().unwrap().clear();
                        // The mirror now *is* the host's account of itself, so
                        // the dashboard can stop saying rows are on their way.
                        mirrored.store(true, Ordering::Relaxed);
                        // The mirror changed off-thread; wake the dashboard loop.
                        dirty.store(true, Ordering::Relaxed);
                    }
                    // Deliberately does *not* withdraw a *dead* presumption. A
                    // delta says only that the state file moved, which a session
                    // on its way out can still do — a last hook, a status
                    // mirrored from the agent's own file as it exits. `Removed`
                    // is the frame that means gone; treating a delta as evidence
                    // of life would flash the row back for the frame or two
                    // before one arrives.
                    //
                    // It does end an *attached* presumption, and that asymmetry
                    // is the point: a delta carries the attached bit, so it is
                    // the host's own account of the very thing being presumed,
                    // arriving whether it agrees or not. Nothing else ends one —
                    // a session stays attached for as long as its user is
                    // working, so a timer would only restore the stale value the
                    // presumption was correcting.
                    ServerFrame::Delta { state } => {
                        let key = state.key();
                        presumed_attached.lock().unwrap().remove(&key);
                        mirror.lock().unwrap().insert(key, *state);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Removed { key } => {
                        mirror.lock().unwrap().remove(&key);
                        // The host has now said what we were presuming, so the
                        // presumption has nothing left to do. Dropping it here
                        // rather than letting it lapse keeps a recycled key (a
                        // launcher pid the host reuses within the window) from
                        // inheriting the hide meant for its predecessor.
                        presumed_dead.lock().unwrap().remove(&key);
                        // Nothing to hold a bit about any more, and a launcher
                        // pid the host recycles must not inherit it.
                        presumed_attached.lock().unwrap().remove(&key);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    // Every reply routes by `req_id` through one accessor, so a
                    // future reply variant needs no change here (§3 tolerance).
                    // `None` covers the pushed stream and an unknown frame from
                    // a newer peer, both of which are simply ignored.
                    _ => {
                        if let Some(tx) = frame.req_id().and_then(|id| pending.remove(&id)) {
                            let _ = tx.send(frame);
                        }
                    }
                }
            }
            req = requests.recv() => {
                let Some(req) = req else { return ServeOutcome::BackendDropped };
                // Drop the entries whose caller has given up (a `request_within`
                // that timed out). A server that never answers a frame it can't
                // decode would otherwise leave one behind per attempt, for as
                // long as the connection lasts.
                pending.retain(|_, tx| !tx.is_closed());
                pending.insert(req.req_id, req.reply);
                if write_frame(&mut wr, &req.frame).await.is_err() {
                    return ServeOutcome::ConnectionLost;
                }
            }
        }
    }
}

#[cfg(test)]
/// A throwaway `$HOME` for a test that runs a real shell script. Lives here
/// rather than in either test module because both use it: the attach-wrapper
/// tests below and `provision`'s deploy-script tests.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cm-upload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;
    use std::time::Duration;
    use tokio::net::UnixListener;

    /// The session table's "loading" line has to cover the *whole* window in
    /// which a host has yet to report, not just the dial. `Connected` is stored
    /// the moment the socket answers — a full round trip before the handshake
    /// and subscribe that fetch the sessions — so keying the line on connection
    /// state alone dropped it while the rows were still in flight, briefly
    /// presenting a host that had said nothing as a host with nothing on it.
    #[test]
    fn a_host_is_loading_until_its_first_snapshot_lands() {
        let backend = Backend::Remote(RemoteBackend::unconnected_for_tests(
            HostId("box".into()),
            Vec::new(),
        ));
        let Backend::Remote(remote) = &backend else {
            unreachable!("built as a remote")
        };
        // Dialing: nothing mirrored, nothing to show.
        remote.simulate_link_for_tests(ConnState::Connecting, false);
        assert!(backend.awaiting_sessions());
        // The socket answered, but the subscribe has yet to come back.
        remote.simulate_link_for_tests(ConnState::Connected, false);
        assert!(backend.awaiting_sessions());
        // Snapshot in. An *empty* one still ends the wait: the host has now
        // answered, and "no sessions" is an answer.
        remote.simulate_link_for_tests(ConnState::Connected, true);
        assert!(!backend.awaiting_sessions());
        // A host that has stopped is not loading — it has failed, not started,
        // and the header tally plus the hosts panel are where that is said.
        remote.simulate_link_for_tests(ConnState::Disconnected, false);
        assert!(!backend.awaiting_sessions());
        remote.simulate_link_for_tests(ConnState::Failed("no miao-server".into()), false);
        assert!(!backend.awaiting_sessions());
        // This machine reads its state files synchronously; it is never loading.
        assert!(!Backend::local().awaiting_sessions());
    }

    fn test_state(pid: u32) -> LauncherState {
        LauncherState {
            launcher_pid: pid,
            session_id: Some(format!("sess-{pid}")),
            cwd: "/tmp".to_string(),
            child_pid: Some(pid + 1),
            ..LauncherState::for_test(AgentControl::Claude, SessionStatus::Idle)
        }
    }

    /// The optimistic hide behind `x` on a remote host. Its two *answered*
    /// endings — the host confirming with a `Removed`, and the presumption being
    /// withdrawn outright — run against the live mock in
    /// `remote_backend_mirrors_snapshot_and_serves_requests`; this is the third,
    /// where no answer ever settles it.
    ///
    /// The lapse is the safety property. Presuming is a *guess* made before the
    /// request goes out, and the server pushes only what changed — so a session
    /// that survived a kill it never heard about is one the host has no reason
    /// to re-send. Without the lapse its row would stay hidden until the next
    /// reconnect, and an invisible running session is worse than a slow one.
    #[test]
    fn a_presumed_kill_hides_a_row_but_never_indefinitely() {
        let now = Instant::now();
        let mirror: HashMap<SessionKey, LauncherState> = [101, 102]
            .into_iter()
            .map(|pid| (SessionKey::from_launcher_pid(pid), test_state(pid)))
            .collect();
        let pids = |rows: Vec<LauncherState>| {
            let mut p: Vec<u32> = rows.iter().map(|s| s.launcher_pid).collect();
            p.sort();
            p
        };

        // Nothing presumed: the host's account of itself, verbatim.
        let mut presumed = HashMap::new();
        let held = HashMap::new();
        assert_eq!(
            pids(live_rows(&mirror, &mut presumed, &held, now)),
            [101, 102]
        );

        // Presumed a moment ago — the row is gone while the kill is in flight,
        // with the mirror still holding it (the host hasn't answered yet).
        presumed.insert(SessionKey::from_launcher_pid(101), now);
        assert_eq!(pids(live_rows(&mirror, &mut presumed, &held, now)), [102]);
        assert_eq!(
            pids(live_rows(
                &mirror,
                &mut presumed,
                &held,
                now + PRESUMED_DEAD_FOR - Duration::from_millis(1)
            )),
            [102]
        );

        // Still there once the window is up: the session outlived the kill, so
        // the guess is withdrawn and the entry with it — no unbounded growth
        // from a host that goes quiet.
        assert_eq!(
            pids(live_rows(
                &mirror,
                &mut presumed,
                &held,
                now + PRESUMED_DEAD_FOR
            )),
            [101, 102]
        );
        assert!(presumed.is_empty());
    }

    /// The other presumption: what an attach proved about the pty's lock, over
    /// what the host is still saying about it — in both directions, since an
    /// attach can end as well as be refused.
    ///
    /// The lowering direction is the one with a visible cost when it is missing.
    /// A window this dashboard closed leaves the host serving `attached = true`
    /// for a round trip, and a row that is detached-and-attached is drawn as held
    /// by another terminal: an offer to steal a session whose only client was us.
    ///
    /// No lapse either way, deliberately — see `presumed_attached`. What ends one
    /// is the host's own account (a `Delta` carries the bit, a `Removed` takes the
    /// row), and a session stays attached for as long as its user is working, so
    /// a timer would only put the stale reading back.
    #[test]
    fn an_attach_settles_the_bit_the_host_is_still_catching_up_on() {
        let now = Instant::now();
        let key = SessionKey::from_launcher_pid(101);
        let mut free = test_state(101);
        free.attached = Some(false);
        let mirror: HashMap<SessionKey, LauncherState> =
            [(key.clone(), free)].into_iter().collect();
        let mut presumed = HashMap::new();

        let bit = |held: &HashMap<SessionKey, bool>, presumed: &mut HashMap<_, _>| {
            live_rows(&mirror, presumed, held, now)[0].attached
        };
        assert_eq!(
            bit(&HashMap::new(), &mut presumed),
            Some(false),
            "the host's own reading, untouched"
        );
        assert_eq!(
            bit(&HashMap::from([(key.clone(), true)]), &mut presumed),
            Some(true),
            "raised by a refused attach — someone else holds it"
        );

        // And lowered by one of ours ending: the host still says attached,
        // because the client it is talking about is the one that just went.
        let mut caught_up = test_state(101);
        caught_up.attached = Some(true);
        let mirror: HashMap<SessionKey, LauncherState> =
            [(key.clone(), caught_up)].into_iter().collect();
        let bit = |held: &HashMap<SessionKey, bool>, presumed: &mut HashMap<_, _>| {
            live_rows(&mirror, presumed, held, now)[0].attached
        };
        assert_eq!(
            bit(&HashMap::from([(key.clone(), false)]), &mut presumed),
            Some(false),
            "lowered by our own attach ending"
        );
        // A presumption the host has caught up on changes nothing, so the two
        // can't fight over a row they already agree about.
        assert_eq!(
            bit(&HashMap::from([(key, true)]), &mut presumed),
            Some(true)
        );
    }

    /// A host's options are passed through untouched except for its forwards,
    /// which have to be told apart because only one ssh call may carry them.
    #[test]
    fn connection_options_keep_everything_but_their_forwards() {
        let split = |text: &str| {
            let args: Vec<String> = text.split_whitespace().map(str::to_string).collect();
            let (opts, fwd) = split_connection_options(&args);
            (opts, fwd.iter().map(|f| f.to_string()).collect::<Vec<_>>())
        };

        // Ordinary options pass through in order, with nothing lifted out.
        let (opts, fwd) = split("-C -o ServerAliveInterval=30");
        assert_eq!(opts, ["-C", "-o", "ServerAliveInterval=30"]);
        assert!(fwd.is_empty());

        // A forward is taken out from between them, separated or glued, and the
        // glued form is normalised so `-O cancel` names it the same either way.
        let (opts, fwd) = split("-C -L 8080:localhost:3000 -4 -D1080");
        assert_eq!(opts, ["-C", "-4"]);
        assert_eq!(fwd, ["-L 8080:localhost:3000", "-D 1080"]);

        // Case matters: `-l` is the login name, and eating it would drop the
        // user the connection is made as.
        let (opts, fwd) = split("-l deploy");
        assert_eq!(opts, ["-l", "deploy"]);
        assert!(fwd.is_empty());

        // A trailing flag with no argument is dropped rather than passed on: it
        // is a usage error on every call that would carry it, and these reach
        // the attach window and the `w` shell too.
        let (opts, fwd) = split("-C -L");
        assert_eq!(opts, ["-C"]);
        assert!(fwd.is_empty());
    }

    /// The socket answers on the first attempt or two in the ordinary case, so
    /// what the ramp is for is spending almost nothing to find that out — while
    /// still reaching the ceiling early enough that a genuinely slow bind is
    /// waited on, and inside a budget a failing attempt adds to the reconnect
    /// backoff before the backoff has started.
    #[test]
    fn the_connect_retry_ramp_is_cheap_early_and_bounded_overall() {
        let ssh: Vec<Duration> = connect_retry_delays(16).collect();
        // One fewer wait than attempts: the last attempt has nothing to wait for.
        assert_eq!(ssh.len(), 15);
        // The first two attempts together cost less than one old flat wait.
        assert!(ssh[0] + ssh[1] < CONNECT_RETRY_CEILING, "{ssh:?}");
        // Doubling, then pinned at the ceiling — reached by the fifth wait.
        assert_eq!(
            &ssh[..5],
            &[
                Duration::from_millis(25),
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
            ]
        );
        assert!(ssh[5..].iter().all(|d| *d == CONNECT_RETRY_CEILING));
        // Which fixes the budget exactly — the number a reader wants, and well
        // under the 6s the flat 400ms schedule spent to reach the same place.
        let total: Duration = ssh.iter().sum();
        assert_eq!(total, Duration::from_millis(4775));

        // The socket transport's three attempts are now a rounding error.
        let direct: Duration = connect_retry_delays(3).sum();
        assert_eq!(direct, Duration::from_millis(75));

        // No attempts means no waits, rather than an underflow.
        assert_eq!(connect_retry_delays(0).count(), 0);
    }

    /// A change to a row's **connection** options has to retire the shared
    /// master, because re-dialling alone re-joins it and the new options never
    /// take effect. Two things must not trip it: a forwards-only edit (those are
    /// re-requested through the master every pass), and a second row on the same
    /// target — that pair would exit each other's master forever.
    #[test]
    fn only_a_rows_own_connection_options_retire_its_master() {
        let opt = |v: &str| vec!["-o".to_string(), v.to_string()];
        let (a, b) = (HostId("opts-a".into()), HostId("opts-b".into()));
        let target = "build-box";

        // A first sighting is not a change: the memo is per-process, so a
        // dashboard restart has nothing to compare against, and exiting the
        // master then would drop the attach windows it means to keep.
        assert!(!options_changed_since_last_dial(
            &a,
            target,
            &opt("Port=2222")
        ));
        // Neither is an ordinary reconnect.
        assert!(!options_changed_since_last_dial(
            &a,
            target,
            &opt("Port=2222")
        ));
        // A second row on the same machine, dialling with options the master
        // was not minted with, retires it — **once**. It is a row we have never
        // seen, so it may be `a` renamed, and the master is the only account of
        // what is actually live.
        assert!(options_changed_since_last_dial(
            &b,
            target,
            &opt("Port=2200")
        ));
        // From here neither row may ever retire the other's master again, or the
        // two would exit each other's forever: `b`'s reconnects are judged by
        // `b`'s own history…
        assert!(!options_changed_since_last_dial(
            &b,
            target,
            &opt("Port=2200")
        ));
        // …and `a`'s by `a`'s, even though the master now holds `b`'s options.
        assert!(!options_changed_since_last_dial(
            &a,
            target,
            &opt("Port=2222")
        ));
        assert!(!options_changed_since_last_dial(
            &b,
            target,
            &opt("Port=2200")
        ));

        // An actual edit to `a`'s row is — once.
        assert!(options_changed_since_last_dial(
            &a,
            target,
            &opt("Port=2200")
        ));
        assert!(!options_changed_since_last_dial(
            &a,
            target,
            &opt("Port=2200")
        ));

        // A forwards-only edit reaches this as the same `extra`, so it isn't a
        // change: the forward is cancelled and re-requested on every pass.
        let c = HostId("opts-c".into());
        let split = |port: &str| {
            split_connection_options(&[
                "-o".to_string(),
                "Port=2200".to_string(),
                "-L".to_string(),
                format!("{port}:localhost:{port}"),
            ])
            .0
        };
        assert!(!options_changed_since_last_dial(&c, target, &split("9000")));
        assert!(!options_changed_since_last_dial(&c, target, &split("9001")));
    }

    /// The hosts panel commits every field of a row on one `Enter`, so renaming
    /// a host *and* changing its options is a single ordinary edit — and the new
    /// label makes it a row we have never dialled. Judged on its own history
    /// that reads as a first sighting, and the master keeps serving the options
    /// the user just replaced, for as long as anything holds it open. What the
    /// master was minted with is what has to decide for a row we don't know.
    #[test]
    fn a_rename_in_the_same_edit_still_retires_the_master() {
        let opt = |v: &str| vec!["-o".to_string(), v.to_string()];
        let target = "rename-box";
        let before = HostId("rename-before".into());

        assert!(!options_changed_since_last_dial(
            &before,
            target,
            &opt("Port=2222")
        ));

        // Label *and* options changed together: a row we have never seen, on a
        // target whose master was minted with something else.
        let after = HostId("rename-after".into());
        assert!(options_changed_since_last_dial(
            &after,
            target,
            &opt("Port=2200")
        ));
        // …and its own reconnects are then ordinary.
        assert!(!options_changed_since_last_dial(
            &after,
            target,
            &opt("Port=2200")
        ));

        // A rename that changes *only* the label is not a change: nothing about
        // the connection moved, so retiring the master would cost the attach
        // windows for nothing.
        let kept = HostId("rename-kept".into());
        assert!(!options_changed_since_last_dial(
            &kept,
            target,
            &opt("Port=2200")
        ));

        // Nor is a brand-new row on a target this process has never dialled.
        assert!(!options_changed_since_last_dial(
            &HostId("rename-fresh".into()),
            "untouched-box",
            &opt("Port=22")
        ));
    }

    /// The clipboard bridge is a synthesized forward, so it has to be one the
    /// rest of the forward machinery can name again: `-O cancel` takes the flag
    /// and the spec back verbatim, and `Forward`'s `Display` is what the log line
    /// shows.
    #[test]
    fn the_clipboard_forward_names_a_home_relative_remote_path() {
        let f = clipboard_forward("/home/miao", Path::new("/run/user/1000/x/clipboard.sock"));
        assert_eq!(f.flag, "-R");
        assert_eq!(
            f.spec,
            "/home/miao/.cache/captain-miao/clipboard.sock:/run/user/1000/x/clipboard.sock"
        );
        assert_eq!(
            f.to_string(),
            "-R /home/miao/.cache/captain-miao/clipboard.sock:/run/user/1000/x/clipboard.sock"
        );
        // Absolute on the remote side, because ssh expands nothing in a forward
        // spec — a `~` or a `$HOME` here would be taken literally as a path.
        let (remote, _) = f.spec.split_once(':').unwrap();
        assert!(remote.starts_with('/'), "{remote} is not absolute");
        assert!(!remote.contains('~') && !remote.contains('$'));
        // And it round-trips through the splitter that lifts a *user's* forwards
        // onto the same child, so nothing about it is a special case downstream.
        let (opts, forwards) = split_connection_options(&[f.flag.clone(), f.spec.clone()]);
        assert!(opts.is_empty());
        assert_eq!(forwards, vec![f]);
    }

    /// The remote path has to be *removed* before ssh can rebind it, and its
    /// parent has to exist. Both were established against a live sshd; see
    /// [`CLIPBOARD_PREP_SCRIPT`].
    #[test]
    fn the_clipboard_prep_script_survives_a_login_shell() {
        // The wrapper's rule, which is what makes this work on a fish account:
        // a single-quoted string is literal in every dialect, but only fish
        // honours escapes inside one — so no quote and no backslash.
        assert!(!CLIPBOARD_PREP_SCRIPT.contains('\''));
        assert!(!CLIPBOARD_PREP_SCRIPT.contains('\\'));
        // `"$HOME"` rather than a spliced path, so a home with a space in it
        // still works — the script is evaluated by a shell, unlike the spec.
        assert!(CLIPBOARD_PREP_SCRIPT.contains("\"$HOME\""));
        assert!(CLIPBOARD_PREP_SCRIPT.contains("mkdir -p"));
        assert!(CLIPBOARD_PREP_SCRIPT.contains("rm -f"));
        // It prepares exactly the path the forward asks for.
        let home = "/home/miao";
        let spec = clipboard_forward(home, Path::new("/tmp/x.sock")).spec;
        let (remote, _) = spec.split_once(':').unwrap();
        let expanded = CLIPBOARD_PREP_SCRIPT.replace("\"$HOME\"", home);
        assert!(
            expanded.contains(remote),
            "prep script does not cover {remote}: {expanded}"
        );
        // And the wrapper accepts it (the `debug_assert` inside would fire
        // otherwise).
        let wrapped = login_shell_safe(CLIPBOARD_PREP_SCRIPT);
        assert!(wrapped.starts_with("/bin/sh -c '") && wrapped.ends_with('\''));
    }

    /// Turning the clipboard off has to be cancellable on its own, ahead of the
    /// wholesale pass in `cancel_user_forwards` — that one sits under `daemon
    /// ensure`'s early return, so a host whose server is broken would keep the
    /// `-R` while the panel says the toggle applied.
    #[test]
    fn a_revoked_clipboard_forward_is_nameable_on_its_own() {
        let host = HostId("revoke-probe".to_string());
        let target = "user@box";
        let home = "/home/miao";
        let f = clipboard_forward(home, Path::new("/run/user/1000/x/clipboard.sock"));
        // Nothing asked for yet: an ordinary host that never wanted the clipboard
        // must pay no `-O cancel` on any of its connects.
        assert!(!forward_was_requested(&host, target, &f));

        REQUESTED_FORWARDS
            .lock()
            .unwrap()
            .insert((host.0.clone(), target.to_string()), vec![f.clone()]);
        assert!(forward_was_requested(&host, target, &f));
        // Keyed by (label, target), so one row's forwards are never another's —
        // two panel rows may well name the same machine.
        assert!(!forward_was_requested(
            &HostId("other".to_string()),
            target,
            &f
        ));
        assert!(!forward_was_requested(&host, "user@elsewhere", &f));
        // And the spec has to match exactly, since that is what `-O cancel` names:
        // a different local socket is a different forward.
        let elsewhere = clipboard_forward(home, Path::new("/run/user/1000/y/clipboard.sock"));
        assert!(!forward_was_requested(&host, target, &elsewhere));

        REQUESTED_FORWARDS
            .lock()
            .unwrap()
            .remove(&(host.0.clone(), target.to_string()));
    }

    /// The tail of an ssh argv after the `-o` option block, so the assertions
    /// stay about *shape* rather than restating `ssh_common_opts`.
    fn ssh_tail(argv: &[String]) -> Vec<String> {
        let start = argv
            .iter()
            .rposition(|a| a == "-o")
            .map(|i| i + 2)
            .unwrap_or(0);
        argv[start..].to_vec()
    }

    #[test]
    fn attach_argv_ssh_vs_direct() {
        let ssh = attach_argv(Some("user@box"), &[], "miao-server", "s1", false, &[]);
        assert_eq!(ssh[0], "ssh");
        assert_eq!(ssh[1], "-t");
        assert_eq!(ssh_tail(&ssh), ["user@box", "miao-server", "attach", "s1"]);
        // Attach windows ride the connection task's ControlMaster (§4), so they
        // skip authentication entirely — that's the whole point of the options.
        assert!(ssh.iter().any(|a| a.starts_with("ControlPath=")));
        assert!(ssh.iter().any(|a| a == "ControlMaster=auto"));

        // A socket transport (pooled localhost) needs no ssh hop at all.
        assert_eq!(
            attach_argv(None, &[], "miao-server", "s1", false, &[]),
            ["miao-server", "attach", "s1"]
        );
        // The steal is a flag on the attach, never on the create path.
        assert_eq!(
            attach_argv(None, &[], "miao-server", "s1", true, &[]),
            ["miao-server", "attach", "--force", "s1"]
        );
        // A deployed cache path is invoked in place of `miao-server`.
        let cache = "/home/u/.cache/captain-miao/bin/miao-server";
        let ssh = attach_argv(Some("user@box"), &[], cache, "s1", false, &[]);
        assert_eq!(ssh_tail(&ssh), ["user@box", cache, "attach", "s1"]);
    }

    /// `[remote] inherit_env` runs as one `--inherit-env <NAME>` pair per name on
    /// the attach, pushed before the session name so the session stays the last
    /// positional arg.
    #[test]
    fn attach_argv_appends_inherit_env() {
        let inherit = ["ANTHROPIC_API_KEY".to_string()];
        let ssh = attach_argv(Some("box"), &[], "miao-server", "s1", false, &inherit);
        let tail = ssh_tail(&ssh);
        assert!(
            tail.windows(2)
                .any(|w| w == ["--inherit-env", "ANTHROPIC_API_KEY"]),
            "expected contiguous [--inherit-env, ANTHROPIC_API_KEY], got {tail:?}"
        );
        // The session name stays the last positional arg, behind the flag pairs.
        assert_eq!(tail.last(), Some(&"s1".to_string()));
    }

    /// The *reattach* path forwards nothing, however `[remote] inherit_env` is
    /// configured — and this is the assertion that keeps it that way.
    ///
    /// Two independent reasons, either of which is sufficient. libshpool injects
    /// a forwarded value only into a shell it is spawning, which happens once at
    /// session creation, so a name passed here cannot reach the session; what it
    /// *can* still do is make the daemon rewrite every forwarded name and value
    /// to `forward.env` in cleartext, which it does on every attach. And the
    /// binary on the direct-local path may be `miao-client`, whose `attach` has
    /// no `--inherit-env` at all, so the flag would exit 2 on a clap usage error
    /// instead of opening a window.
    ///
    /// Only the remote arm is exercised here; the direct-local one is pinned by
    /// construction, since [`local_attach_argv`] no longer takes names at all
    /// (it needs a pool client installed to run, which a test host has no
    /// reason to have).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reattach_path_never_forwards_env() {
        let backend = Backend::Remote(RemoteBackend::connect(
            Transport::LocalSocket(PathBuf::from("/nonexistent/cm-test.sock")),
            HostId("mock".into()),
        ));
        // An *exact* argv, which is the strong form of the assertion: it holds
        // only because `attach_plan` reads no config at all, so no machine's own
        // `[remote] inherit_env` can slip a flag pair in here. (The create path's
        // plan still reads config and is asserted loosely — see
        // `remote_open_session_returns_attach_plan`.)
        let plan = backend.attach_plan("cm-claude-9-1", false).unwrap();
        assert_eq!(plan.argv, ["miao-server", "attach", "cm-claude-9-1"]);
    }

    /// The searched *fallback* for the direct-local attach — reached only when
    /// the pool name carries no daemon pid to ask (see `local_attach_exe`).
    ///
    /// Order is the assertion. The dashboard's own directory outranks `PATH`,
    /// and within a directory `miao-client` outranks `miao-server` — they are
    /// the same primitive over the same socket, so the tie is broken on which is
    /// the smaller thing to have installed. Both preferences are guesses, which
    /// is exactly why they are no longer the primary answer.
    #[test]
    fn local_attach_exe_prefers_the_dashboards_own_dir_then_path() {
        let dirs = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        let here = dirs(&["/opt/miao/bin", "/usr/bin"]);
        let present = |set: &'static [&'static str]| {
            move |p: &Path| set.contains(&p.to_str().unwrap_or_default())
        };

        // Both dirs stocked → the dashboard's own wins, `miao-client` first.
        assert_eq!(
            resolve_local_attach_exe(
                &here,
                present(&["/opt/miao/bin/miao-client", "/usr/bin/miao-client"]),
            ),
            Some("/opt/miao/bin/miao-client".to_string()),
        );
        // Only the daemon is installed beside us: same primitive, same socket.
        assert_eq!(
            resolve_local_attach_exe(
                &here,
                present(&["/opt/miao/bin/miao-server", "/usr/bin/miao-client"]),
            ),
            Some("/opt/miao/bin/miao-server".to_string()),
        );
        // Nothing beside us → fall through to PATH rather than give up.
        assert_eq!(
            resolve_local_attach_exe(&here, present(&["/usr/bin/miao-server"])),
            Some("/usr/bin/miao-server".to_string()),
        );
        // Neither, anywhere: the caller turns this into an explained refusal.
        assert_eq!(resolve_local_attach_exe(&here, present(&[])), None);
    }

    /// A local attach is the pooled-localhost attach with the exe resolved here:
    /// no ssh hop, `--force` on the attach and never on the create path.
    #[test]
    fn local_attach_argv_is_the_socket_shape() {
        let exe = "/opt/miao/bin/miao-client";
        assert_eq!(
            attach_argv(None, &[], exe, "cm-claude-9-1", false, &[]),
            [exe, "attach", "cm-claude-9-1"]
        );
        assert_eq!(
            attach_argv(None, &[], exe, "cm-claude-9-1", true, &[]),
            [exe, "attach", "--force", "cm-claude-9-1"]
        );
    }

    /// With no pool client anywhere the host explains itself rather than
    /// spawning a window that opens onto a `command not found` — the whole
    /// reason `attach_plan` returns a `Result` (§5).
    #[test]
    fn local_attach_names_what_is_missing() {
        // Drive the pure resolver, since the real one reads this machine.
        assert!(resolve_local_attach_exe(&[PathBuf::from("/nowhere")], |_| false).is_none());
        let msg = local_attach_argv("cm-claude-9-1", false)
            .err()
            .map(|e| e.to_string());
        // This machine may genuinely have one installed; only assert the text
        // when it doesn't, so the test says the same thing on CI and a dev box.
        if let Some(msg) = msg {
            assert!(msg.contains("miao-client"), "{msg}");
            assert!(msg.contains("miao-server"), "{msg}");
        }
    }

    /// The attach runs the binary the *pool name names*, not one found by
    /// searching. A pool session is `cm-<agent>-<daemon-pid>-<seq>` and the pool
    /// lives inside that process, so `/proc/<pid>/exe` is the binary that owns
    /// the pty by construction.
    ///
    /// The regression it closes: a dev tree with a current `miao` and a stale
    /// `miao-server` beside it ran the stale one, which could no longer parse
    /// the state files — and `read_all_launcher_states` skips what it cannot
    /// parse, so the attach guard saw *no* sessions and refused a live one as a
    /// dead name. Nothing in that failure pointed at the binary.
    #[test]
    fn the_attach_exe_comes_from_the_daemon_that_minted_the_name() {
        // This test process stands in for a daemon: it is a live pid, and the
        // name encodes it exactly as the server would have minted it.
        let me = std::process::id();
        let mine = daemon_exe_for_pool_session(&format!("cm-claude-{me}-1"));
        if cfg!(target_os = "linux") {
            let mine = mine.expect("/proc names this process's own executable");
            assert_eq!(
                mine,
                std::env::current_exe().unwrap().to_str().unwrap(),
                "the daemon's own exe, not a search result"
            );
        }

        // A name in no recognisable shape (a hand-set `--pool-session`) carries
        // no pid to ask, so the search is all there is.
        assert_eq!(daemon_exe_for_pool_session("hand-set"), None);
        // Nor does a dead pid resolve — `/proc` simply has no entry.
        assert_eq!(daemon_exe_for_pool_session("cm-claude-4294967294-1"), None);
    }

    #[test]
    fn remote_shell_argv_cds_and_execs_login_shell() {
        let argv = remote_shell_argv("user@box", &[], "/home/u/proj");
        assert_eq!(
            ssh_tail(&argv),
            [
                "user@box",
                "/bin/sh -c 'cd \"$0\" && exec \"${SHELL:-/bin/sh}\" -l' '/home/u/proj'"
            ]
        );
        // The landmine (§3): a host-canonical `~` path must reach the remote as
        // something the *remote* shell expands. Single-quoting it — the obvious
        // thing — would make `cd '~/proj'` fail on every host. It rides outside
        // the `sh -c` wrapper precisely so it can keep its quotes.
        let argv = remote_shell_argv("box", &[], "~/proj");
        assert_eq!(
            ssh_tail(&argv),
            [
                "box",
                "/bin/sh -c 'cd \"$0\" && exec \"${SHELL:-/bin/sh}\" -l' \"$HOME\"/'proj'"
            ]
        );
        // Empty cwd drops the `cd` and just opens a login shell.
        let argv = remote_shell_argv("box", &[], "");
        assert_eq!(
            ssh_tail(&argv),
            ["box", "/bin/sh -c 'exec \"${SHELL:-/bin/sh}\" -l'"]
        );
    }

    /// The regression that made `w` unusable on a remote row: the command ssh
    /// sends is parsed by the *account's login shell*, and a bare
    /// `${SHELL:-/bin/sh}` is a syntax error in fish — the window opened and
    /// died before it drew anything. Parse the real string under every shell
    /// installed here, the way the deploy script's test does.
    ///
    /// Parse-only (`-n` / `--no-execute`), because *running* it would fork an
    /// interactive login shell. That rules csh out (no syntax-check mode with
    /// `-c`); its share of the guarantee rides on the same literal-single-quote
    /// rule [`login_shell_safe`] documents.
    #[test]
    fn remote_shell_command_parses_under_every_login_shell() {
        let cmd = remote_shell_argv("box", &[], "~/proj").pop().unwrap();
        let mut checked = 0;
        for (shell, check) in [
            ("/bin/sh", "-n"),
            ("bash", "-n"),
            ("zsh", "-n"),
            ("dash", "-n"),
            ("ksh", "-n"),
            ("fish", "--no-execute"),
        ] {
            let Ok(out) = std::process::Command::new(shell)
                .args([check, "-c", &cmd])
                .output()
            else {
                continue; // not installed here
            };
            assert!(
                out.status.success(),
                "{shell} rejected the work-tab command: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            checked += 1;
        }
        assert!(checked > 0, "no shell available to parse-check with");
    }

    /// A remote host is under no obligation to stop talking, and a flood is not
    /// a hung link ssh will notice: the peer answers keepalives, `ConnectTimeout`
    /// is long past, and the only thing that grows is the dashboard's memory.
    /// The cap is what makes that a bounded read rather than an open one — and
    /// it holds while the child is *still writing*, which is the case an
    /// after-the-fact truncation misses entirely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flooding_host_cannot_grow_the_buffer_without_bound() {
        // 64 MiB of `y`, of which we agree to hold 1 KiB.
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("head -c 67108864 /dev/zero | tr \\\\0 y; echo done")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning the flood");
        let (_, stdout, _) =
            tokio::time::timeout(Duration::from_secs(20), capped_output(child, 1024))
                .await
                .expect("the cap must not depend on the child finishing")
                .expect("reading the flood");
        assert_eq!(stdout.len(), 1024, "held {} bytes", stdout.len());
        // Closing our read end is what ends it: the writer takes SIGPIPE rather
        // than filling a pipe we've stopped draining, so this returns long
        // before the timeout — the cap, not the clock, is doing the work.
    }

    /// …and the wall-clock bound is real too: a host that connects and then says
    /// nothing must not park a connection task forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_host_hits_the_wall_clock_bound() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning the sleeper");
        let start = Instant::now();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(300),
                capped_output(child, REMOTE_OUTPUT_CAP)
            )
            .await
            .is_err(),
            "a silent child must time out rather than be waited on"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// The connect path's fire-and-forget ssh children are bounded too. Nothing
    /// downstream of a hung `-O cancel` runs, so an unbounded one parks the
    /// connection task in `Connecting` with no retry left to rescue it — and the
    /// child has to actually die, or every timed-out attempt leaks an ssh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_control_command_gives_up_and_takes_its_child_with_it() {
        let mut cmd = detached("/bin/sh");
        // A sleeper that reports its own pid first, so the test can ask the OS
        // whether the kill actually happened rather than trusting `kill_on_drop`.
        let pidfile = std::env::temp_dir().join(format!("cm-bounded-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        cmd.arg("-c")
            // `exec`, so the pid recorded is the sleeper's under any `/bin/sh`:
            // dash and bash tail-exec the last command anyway, but a shell that
            // forked instead would have this test watch the shell die and pass
            // while leaking the sleeper it is supposed to be pinning.
            .arg(format!("echo $$ > {}; exec sleep 60", pidfile.display()));
        let start = Instant::now();
        assert!(
            !bounded_status(cmd, Duration::from_millis(300)).await,
            "a child that never exits cannot report success"
        );
        assert!(start.elapsed() < Duration::from_secs(5), "it must give up");
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the sleeper wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        let _ = std::fs::remove_file(&pidfile);
        // `kill_on_drop` signals and then reaps on tokio's orphan queue, and a
        // zombie still answers signal 0 — so this is waiting on the reap, which
        // is the strictly later event. Polled with an `await` between, which is
        // what lets the reaper run at all.
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the ssh child outlived its deadline");
    }

    /// The log quotes a remote host's stderr verbatim. ratatui is what stops an
    /// `ESC` in it from reaching the emulator; this keeps the same text
    /// *legible* — and holds the line if that ever stops being true. Line
    /// structure survives; nothing else in the control classes does.
    #[test]
    fn quoted_host_output_cannot_drive_the_terminal() {
        // A clear-screen and a cursor move, as a hostile (or just broken) host
        // could emit them: 7-bit ESC-introduced, and the 8-bit C1 CSI that a
        // filter thinking only in ESC would sail straight past.
        let hostile = "tic: \u{1b}[2Jbad\u{9b}31mred\u{1b}]0;retitle\u{7}";
        let safe = host_text_safe(hostile);
        assert!(!safe.contains('\u{1b}'), "{safe:?}");
        assert!(!safe.contains('\u{9b}'), "{safe:?}");
        assert!(!safe.contains('\u{7}'), "{safe:?}");
        // The words survive — the log is still a diagnosis.
        assert!(safe.contains("bad") && safe.contains("red"), "{safe:?}");
        // Line structure is load-bearing (`host_log_lines` splits on it) and a
        // tab would paint as one cell, so it becomes a space.
        assert_eq!(host_text_safe("a\nb\tc"), "a\nb c");
        // A `\r` alone would return the cursor and overwrite the line.
        assert_eq!(host_text_safe("done\r"), "done\u{FFFD}");
        // Ordinary text, including non-ASCII, is untouched.
        assert_eq!(host_text_safe("no such file: café"), "no such file: café");
    }

    /// …and it happens where the bytes arrive, so a consumer that never heard
    /// of it — `tracing`, which writes files a user may later `cat`, or a
    /// `ConnState::Failed` reason — cannot receive raw ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escapes_are_stripped_where_remote_output_enters() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            // Printed by the "host": a clear-screen on stdout, a retitle on
            // stderr. `printf` so the bytes are real control characters.
            .arg("printf 'a\\033[2Jb\\n'; printf 'e\\033]0;x\\007f\\n' >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning the noisy child");
        let (_, stdout, stderr) = capped_output(child, REMOTE_OUTPUT_CAP)
            .await
            .expect("reading it");
        assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
        assert!(
            !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
            "{stderr:?}"
        );
        // Still readable as a diagnosis.
        assert!(stdout.contains('a') && stdout.contains('b'), "{stdout:?}");
        assert!(stderr.contains('e') && stderr.contains('f'), "{stderr:?}");
    }

    #[test]
    fn the_conn_log_keeps_the_newest_lines_in_order() {
        let log = ConnLog::default();
        for i in 0..CONN_LOG_CAP + 10 {
            log.info(format!("line {i}"));
        }
        log.error("it broke");
        let entries = log.entries();
        // Bounded, oldest-first, and the tail is what survived — a host that has
        // been flapping for a week must not grow without limit, and what you
        // want when you open it is the most recent attempt.
        assert_eq!(entries.len(), CONN_LOG_CAP);
        assert_eq!(entries.last().unwrap().text, "it broke");
        assert!(entries.last().unwrap().error);
        assert_eq!(entries[0].text, format!("line {}", 11));
        assert!(!entries[0].error);
    }

    /// The attach wrapper has to do two things and no more: run the attach
    /// unchanged, and report exactly once when it ends. Run for real, because
    /// the failure modes here are shell semantics — a trap that never fires, a
    /// double report, an argv mangled by quoting — none of which a string
    /// comparison would catch.
    #[test]
    fn the_attach_wrapper_runs_the_attach_and_reports_its_end() {
        let dir = scratch_home("attach-wrapper");
        let reporter = dir.join("reporter.sh");
        // Stands in for `miao attach-exited`, appending its argv so a second
        // report would be visible as a second line.
        std::fs::write(
            &reporter,
            format!("#!/bin/sh\necho \"$@\" >> {}/reports\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&reporter, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The "attach": a payload that records the argv it was handed, spaces
        // and all, so quoting damage shows up.
        let payload = dir.join("attach.sh");
        std::fs::write(
            &payload,
            format!("#!/bin/sh\necho \"$@\" > {}/attached\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).unwrap();

        let argv = report_on_exit_argv(
            vec![
                payload.display().to_string(),
                "attach".into(),
                "cm-claude 7".into(), // a space, to catch splatted quoting
            ],
            Some(reporter.to_str().unwrap()),
            "box",
            "cm-claude-7-1",
        );
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(
            std::fs::read_to_string(dir.join("attached")).unwrap(),
            "attach cm-claude 7\n",
            "the attach argv must reach the command untouched"
        );
        // Exactly one report, carrying the binding's identity. Two lines would
        // mean the EXIT/HUP trap pair fired twice — the latch is what stops it.
        assert_eq!(
            reported_once(&dir),
            "attach-exited --host box --token cm-claude-7-1 --status 0"
        );
    }

    /// The single report line, minus its `--held-secs` tail. The duration is
    /// wall clock off `date +%s`, so a test that finishes in microseconds still
    /// reports 1 whenever it straddles a second boundary — pinning the number
    /// would be a flake, and every caller here cares about the identity and the
    /// status. Asserting a lone line is the part that matters: two would mean
    /// the EXIT/HUP trap pair reported twice.
    fn reported_once(dir: &Path) -> String {
        let reports = std::fs::read_to_string(dir.join("reports")).unwrap();
        let line = reports.strip_suffix('\n').expect("one terminated line");
        assert!(!line.contains('\n'), "reported more than once: {reports:?}");
        let (head, secs) = line.split_once(" --held-secs ").expect("a held-secs tail");
        assert!(
            secs.parse::<u64>().is_ok_and(|s| s <= 2),
            "the wrapper must report the attach's own short duration: {secs:?}"
        );
        head.to_string()
    }

    /// The other half of the wrapper's job: a *refused* attach keeps its window,
    /// because the error it printed exists nowhere else. Run for real — the
    /// whole point is that the script blocks rather than returning, which no
    /// string comparison can show.
    ///
    /// The two exits are the pair `attach_window_is_spent` separates, and they
    /// are checked the same way: run the wrapper with its stdin closed, so the
    /// `read` that holds the window hits EOF instead of hanging the test, and
    /// look at whether the prompt was printed at all.
    #[test]
    fn the_attach_wrapper_holds_the_window_only_for_a_refused_attach() {
        let dir = scratch_home("attach-wrapper-hold");
        let reporter = dir.join("reporter.sh");
        std::fs::write(&reporter, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&reporter, std::fs::Permissions::from_mode(0o755)).unwrap();

        // `sh -c 'exit N'` stands in for the attach: 255 is what ssh reports for
        // both a refusal and a mid-session drop, so it is the status that has to
        // be told apart by how long it took.
        let run = |args: Vec<String>| -> String {
            let argv = report_on_exit_argv(args, Some(reporter.to_str().unwrap()), "box", "cm-1");
            let out = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        // Refused on arrival: non-zero, immediately. The window stays, and says
        // why it is still there.
        let refused = run(vec!["sh".into(), "-c".into(), "exit 255".into()]);
        assert!(
            refused.contains("attach to cm-1 exited with status 255"),
            "a refused attach must hold its window: {refused:?}"
        );
        // Ran and ended: the window closes, so nothing is printed. `sleep` for
        // longer than the grace would make this test that slow, so the case is
        // covered by the statuses that are spent whatever the duration — a clean
        // exit and the signals the wrapper traps.
        for spent in ["exit 0", "exit 129", "exit 143"] {
            let out = run(vec!["sh".into(), "-c".into(), spent.into()]);
            assert!(
                out.is_empty(),
                "a spent attach must let its window close ({spent}): {out:?}"
            );
        }
    }

    /// Closing the window is the case the whole mechanism exists for, and it
    /// arrives as a SIGHUP mid-attach rather than as a clean exit.
    #[test]
    fn the_attach_wrapper_reports_when_the_window_is_closed() {
        let dir = scratch_home("attach-wrapper-hup");
        let reporter = dir.join("reporter.sh");
        std::fs::write(
            &reporter,
            format!("#!/bin/sh\necho \"$@\" >> {}/reports\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&reporter, std::fs::Permissions::from_mode(0o755)).unwrap();

        use std::os::unix::process::CommandExt as _;
        // A long-lived "attach", killed the way one kind of closing window kills
        // one: the terminal SIGHUPs the whole foreground process *group*, so the
        // wrapper and the attach under it die together. The other kind — only
        // the wrapper signalled — is its own test below, and is the harder one.
        let argv = report_on_exit_argv(
            vec!["sleep".into(), "30".into()],
            Some(reporter.to_str().unwrap()),
            "box",
            "cm-1",
        );
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .process_group(0)
            .spawn()
            .unwrap();
        // Let the wrapper install its trap and start the payload.
        std::thread::sleep(std::time::Duration::from_millis(300));
        // SAFETY: a plain `kill(2)` on a group this test created and owns.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGHUP) };
        child.wait().unwrap();
        // The trap spawns the reporter; give it a moment to land.
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert_eq!(
            reported_once(&dir),
            // 129 = 128 + SIGHUP: the status the dashboard reads as "the window
            // went away", never as a refused attach.
            "attach-exited --host box --token cm-1 --status 129",
            "a closed window must report exactly once, with the signal status"
        );
    }

    /// The other way a window ends, and the one that used to be reported wrong.
    ///
    /// A terminal that merely closes the pty master signals the *session leader
    /// alone* — this wrapper — and never touches ssh, which then finds its tty
    /// gone and exits 255 all by itself. 255 is also what a dropped link gives,
    /// so a wrapper inheriting `$?` called a deliberate close a network failure
    /// and the session was detached rather than ended. The SIGHUP the wrapper
    /// itself took is the fact that separates them, and it must win over the
    /// status of a payload nobody signalled.
    #[test]
    fn a_window_close_outranks_the_status_of_an_unsignalled_attach() {
        let dir = scratch_home("attach-wrapper-leader-hup");
        let reporter = dir.join("reporter.sh");
        std::fs::write(
            &reporter,
            format!("#!/bin/sh\necho \"$@\" >> {}/reports\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&reporter, std::fs::Permissions::from_mode(0o755)).unwrap();

        use std::os::unix::process::CommandExt as _;
        // Stands in for ssh outliving the hangup by a moment and then exiting
        // with the status that means "this connection is unusable".
        let argv = report_on_exit_argv(
            vec!["sh".into(), "-c".into(), "sleep 1; exit 255".into()],
            Some(reporter.to_str().unwrap()),
            "box",
            "cm-1",
        );
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            // The hold reads from stdin when it thinks an attach was refused;
            // the real window is gone by then, so give it the same EOF.
            .stdin(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Positive pid, not the negated group: only the leader is signalled,
        // which is the whole point of the case.
        // SAFETY: a plain `kill(2)` on a child this test spawned and owns.
        unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
        child.wait().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert_eq!(
            reported_once(&dir),
            "attach-exited --host box --token cm-1 --status 129",
            "a hung-up wrapper must report the close, not the attach's own 255"
        );
    }

    /// `cargo build` while the dashboard is running replaces the inode
    /// `/proc/self/exe` points at, and Linux then reports the path with a
    /// literal `" (deleted)"` glued on. Splicing that into the wrapper yields a
    /// path that cannot be executed, so the report silently never arrives — in
    /// the one configuration where someone is most likely to be testing it.
    #[test]
    fn the_reporter_path_survives_a_rebuild_under_a_running_dashboard() {
        let real = PathBuf::from("/opt/miao/bin/miao");
        let present = |p: &Path| p == real;

        assert_eq!(
            resolve_reporter_exe(real.clone(), present).as_deref(),
            Some("/opt/miao/bin/miao")
        );
        // The rebuilt case: same path, new binary behind it by the time the trap
        // runs — which is what the wrapper re-executes.
        assert_eq!(
            resolve_reporter_exe(PathBuf::from("/opt/miao/bin/miao (deleted)"), present).as_deref(),
            Some("/opt/miao/bin/miao")
        );
        // Genuinely gone (moved install, `cargo clean`): nothing to report with,
        // so the attach runs unwrapped rather than carrying a dead path.
        assert_eq!(
            resolve_reporter_exe(PathBuf::from("/opt/miao/bin/miao"), |_: &Path| false),
            None
        );
        // A path that really does end in " (deleted)" and exists is left alone.
        let odd = PathBuf::from("/opt/miao (deleted)");
        assert_eq!(
            resolve_reporter_exe(odd.clone(), |p: &Path| p == odd).as_deref(),
            Some("/opt/miao (deleted)")
        );
    }

    /// With no resolvable exe there is nothing to report *with* — but the
    /// wrapper still runs, because it also owns the window's hold. So the argv
    /// is wrapped either way, with an empty `$e` standing for "don't report",
    /// and the attach itself must still reach the command untouched.
    #[test]
    fn the_attach_wrapper_runs_unreported_without_a_reporter() {
        let dir = scratch_home("attach-wrapper-noexe");
        let payload = dir.join("attach.sh");
        std::fs::write(
            &payload,
            format!("#!/bin/sh\necho \"$@\" > {}/attached\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).unwrap();

        let argv = report_on_exit_argv(
            vec![
                payload.display().to_string(),
                "attach".into(),
                "cm-1".into(),
            ],
            None,
            "box",
            "cm-1",
        );
        // The empty exe rides in the reporter's slot rather than collapsing the
        // positional parameters, which would shift the attach argv left by one.
        assert_eq!(argv[4], "");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the attach must run without a reporter");
        assert_eq!(
            std::fs::read_to_string(dir.join("attached")).unwrap(),
            "attach cm-1\n"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_open_session_errs_when_unreachable() {
        // No server on the socket → the request never gets a reply, so
        // open_session reports the host as unreachable rather than hanging.
        let remote = RemoteBackend::connect(
            Transport::LocalSocket(PathBuf::from("/nonexistent/captain-miao.sock")),
            HostId::local(),
        );
        let spec = OpenSpec {
            agent: AgentControl::Claude,
            cwd: "/work".to_string(),
            resume: None,
            worktree: None,
        };
        let backend = Backend::Remote(remote);
        assert!(tokio::task::block_in_place(|| backend.open_session(&spec)).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_open_session_returns_attach_plan() {
        let sock = std::env::temp_dir().join(format!("cm-test-open-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(listener, vec![]));
        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        let spec = OpenSpec {
            agent: AgentControl::Claude,
            cwd: "/work".to_string(),
            resume: None,
            worktree: None,
        };
        let plan = tokio::task::block_in_place(|| backend.open_session(&spec)).unwrap();
        match plan {
            // A socket transport (no ssh target) yields a direct attach window.
            LaunchPlan::AttachRemote { argv, session_name } => {
                // Only the fixed shape is asserted: the exe name is whichever
                // pool client this machine has, and `[remote] inherit_env`
                // threads `--inherit-env NAME` pairs between `attach` and the
                // session (pinned with explicit values by
                // `attach_argv_appends_inherit_env`), so an exact argv here
                // would fail on any machine whose real config sets it.
                assert_eq!(argv.first().map(String::as_str), Some("miao-server"));
                assert_eq!(argv.get(1).map(String::as_str), Some("attach"));
                assert_eq!(argv.last().map(String::as_str), Some("pool-claude"));
                assert_eq!(session_name, "pool-claude");
            }
            LaunchPlan::SpawnLocal { .. } => panic!("expected AttachRemote from a remote backend"),
        }
        let _ = std::fs::remove_file(&sock);
    }

    /// The attached bit a direct-local dashboard cannot read off a file: it is
    /// maintained in the daemon's memory from libshpool's hooks and only ever
    /// pushed, so the watch has to *subscribe* — a poll would both miss the
    /// transitions and sample a lock that is only ever true in passing (§10.2).
    ///
    /// What is pinned here is the whole life of one bit: a snapshot brings it,
    /// a delta moves it, a removal takes it away — and each of those wakes the
    /// dashboard, because none of them touches anything under `sessions/` and
    /// the notify watcher would therefore never fire for one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pool_watch_subscribes_to_this_machines_attached_bit() {
        let sock =
            std::env::temp_dir().join(format!("cm-test-poolwatch-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let pooled = |pid: u32, attached: Option<bool>| LauncherState {
            pool_session: Some(format!("cm-claude-1-{pid}")),
            attached,
            ..test_state(pid)
        };
        // The daemon serves every state file, pooled or not; an unpooled row has
        // no bit to contribute and must not land in the map.
        let unpooled = test_state(9);
        let key = pooled(1, None).key();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut rd = BufReader::new(rd);
            let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(
                &mut wr,
                &ServerFrame::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                    host: "mock".into(),
                },
            )
            .await
            .unwrap();
            let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(
                &mut wr,
                &ServerFrame::Snapshot {
                    sessions: vec![pooled(1, Some(true)), unpooled],
                },
            )
            .await
            .unwrap();
            // Each push is spaced so the client observes the states in turn:
            // back-to-back frames collapse into one reading and the middle of
            // the sequence — the whole subject of the test — goes untested.
            tokio::time::sleep(Duration::from_millis(150)).await;
            // The detach hook's push: same session, bit flipped.
            write_frame(
                &mut wr,
                &ServerFrame::Delta {
                    state: Box::new(pooled(1, Some(false))),
                },
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            write_frame(&mut wr, &ServerFrame::Removed { key })
                .await
                .unwrap();
            // Hold the connection open so the read loop ends on the test's terms.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let rows: Arc<Mutex<HashMap<SessionKey, PoolRow>>> = Arc::new(Mutex::new(HashMap::new()));
        let changed = Arc::new(AtomicBool::new(false));
        let stream = UnixStream::connect(&sock).await.unwrap();
        tokio::spawn({
            let (rows, changed) = (rows.clone(), changed.clone());
            async move { pool_watch_serve(stream, &rows, &changed).await }
        });

        // Wait for a reading rather than sleeping a fixed span: the assertion is
        // about what arrives, not about how fast this machine is.
        let settle = |want: Option<bool>| {
            let rows = rows.clone();
            async move {
                for _ in 0..100 {
                    let got = rows
                        .lock()
                        .unwrap()
                        .values()
                        .find(|r| r.pool_session == "cm-claude-1-1")
                        .and_then(|r| r.attached);
                    if got == want {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                false
            }
        };

        assert!(settle(Some(true)).await, "the snapshot's bit never landed");
        assert_eq!(
            rows.lock().unwrap().len(),
            1,
            "an unpooled row has no bit to contribute"
        );
        assert!(
            changed.swap(false, Ordering::Relaxed),
            "a snapshot must wake the dashboard — nothing under sessions/ moved"
        );

        assert!(settle(Some(false)).await, "the delta's flip never landed");
        assert!(
            changed.swap(false, Ordering::Relaxed),
            "a detach must wake the dashboard for the same reason"
        );

        for _ in 0..100 {
            if rows.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            rows.lock().unwrap().is_empty(),
            "Removed must drop the row, not leave it asserting a stale bit"
        );
        assert!(changed.load(Ordering::Relaxed), "a removal wakes it too");
        let _ = std::fs::remove_file(&sock);
    }

    /// A pooled row with no daemon behind it reads *unknown*, not *free*. That
    /// is the one thing the overlay must not get wrong: `Some(false)` invites
    /// an attach the pool would refuse, and hides the steal that would work.
    #[test]
    fn an_unwatched_pool_leaves_the_bit_unknown() {
        let watch = PoolWatch::default();
        assert!(watch.attached_by_pool_session().is_empty());
        // A daemon that has answered but knows nothing about this session is the
        // same reading — the map is keyed by what it told us, so a miss is a
        // miss either way.
        watch.rows.lock().unwrap().insert(
            test_state(1).key(),
            PoolRow {
                pool_session: "cm-claude-1-1".into(),
                attached: None,
            },
        );
        assert!(watch.attached_by_pool_session().is_empty());
    }

    /// A protocol-speaking stand-in for `miao-server`: one connection,
    /// handshake, snapshot, then canned replies to requests.
    async fn mock_server(listener: UnixListener, sessions: Vec<LauncherState>) {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);
        let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(
            &mut wr,
            &ServerFrame::Welcome {
                server_version: "test".into(),
                protocol: PROTOCOL_VERSION,
                host: "mock".into(),
            },
        )
        .await
        .unwrap();
        let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(&mut wr, &ServerFrame::Snapshot { sessions })
            .await
            .unwrap();
        // The host's recent-dirs list, which `ForgetRecentDir` actually edits —
        // so a later `ListRecentDirs` shows whether the delete landed on the
        // *host's* list rather than only on the client's copy of it.
        let mut recent: Vec<String> = vec!["~/proj".into(), "~/other".into()];
        while let Ok(Some(frame)) = read_frame::<_, ClientFrame>(&mut rd).await {
            match frame {
                ClientFrame::ListResumable { req_id, .. } => write_frame(
                    &mut wr,
                    &ServerFrame::Resumable {
                        req_id,
                        candidates: vec![],
                        errors: vec![],
                    },
                )
                .await
                .unwrap(),
                ClientFrame::KillSession { req_id, key } => {
                    write_frame(&mut wr, &ServerFrame::Killed { req_id, ok: true })
                        .await
                        .unwrap();
                    // What a real host does moments later, once the launcher has
                    // torn down and its state file gone. The client's optimistic
                    // hide is waiting for exactly this.
                    write_frame(&mut wr, &ServerFrame::Removed { key })
                        .await
                        .unwrap()
                }
                ClientFrame::GetVitals { req_id } => write_frame(
                    &mut wr,
                    &ServerFrame::Vitals {
                        req_id,
                        vitals: HostVitals {
                            cpu_percent: Some(42.0),
                            mem_used_bytes: Some(4 << 30),
                            mem_total_bytes: Some(16 << 30),
                        },
                    },
                )
                .await
                .unwrap(),
                ClientFrame::OpenSession { req_id, spec } => {
                    // Derive the pool name from the spec so the test also
                    // confirms the spec rode the wire intact.
                    let name = format!("pool-{}", spec.agent.cli_subcommand());
                    write_frame(
                        &mut wr,
                        &ServerFrame::Opened {
                            req_id,
                            session_name: Some(name),
                            error: None,
                        },
                    )
                    .await
                    .unwrap()
                }
                ClientFrame::ListRecentDirs { req_id } => write_frame(
                    &mut wr,
                    &ServerFrame::RecentDirs {
                        req_id,
                        // Host-canonical: the wire form IS the display form,
                        // and no `$HOME` rides along (§3).
                        cwds: recent.clone(),
                    },
                )
                .await
                .unwrap(),
                ClientFrame::ForgetRecentDir { req_id, cwd } => {
                    let before = recent.len();
                    recent.retain(|c| c != &cwd);
                    write_frame(
                        &mut wr,
                        &ServerFrame::RecentDirForgotten {
                            req_id,
                            ok: recent.len() != before,
                        },
                    )
                    .await
                    .unwrap()
                }
                ClientFrame::CompletePath { req_id, prefix } => write_frame(
                    &mut wr,
                    // Echo the prefix back so the test confirms it rode the wire.
                    &ServerFrame::PathCompletions {
                        req_id,
                        matches: vec![format!("{prefix}alpha/"), format!("{prefix}apple/")],
                    },
                )
                .await
                .unwrap(),
                ClientFrame::CheckDir { req_id, path } => write_frame(
                    &mut wr,
                    // Only `/home/u/proj` "exists" on this mock host.
                    &ServerFrame::DirChecked {
                        req_id,
                        exists: path == "/home/u/proj",
                    },
                )
                .await
                .unwrap(),
                _ => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_mirrors_snapshot_and_serves_requests() {
        let sock = std::env::temp_dir().join(format!("cm-test-remote-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(
            listener,
            vec![test_state(101), test_state(102)],
        ));

        let backend = RemoteBackend::connect(
            Transport::LocalSocket(sock.clone()),
            HostId("mock".to_string()),
        );

        // The mirror fills asynchronously once the snapshot lands.
        let mut tries = 0;
        while backend.list_sessions().len() != 2 {
            tries += 1;
            assert!(tries < 100, "mirror never filled from snapshot");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut pids: Vec<u32> = backend
            .list_sessions()
            .iter()
            .map(|s| s.launcher_pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![101, 102]);

        // Blocking request/response must run off the async worker.
        let (cands, errs) = tokio::task::block_in_place(|| backend.list_resumable(5));
        assert!(cands.is_empty() && errs.is_empty());
        assert_eq!(
            tokio::task::block_in_place(
                || backend.kill_session(&SessionKey::from_launcher_pid(999))
            ),
            KillOutcome::Signalled
        );

        // The optimistic half. Presuming a session dead takes its row out of
        // `list_sessions` at once, with nothing asked of the host at all…
        let spared = SessionKey::from_launcher_pid(102);
        backend.presume_dead(&spared);
        assert_eq!(backend.list_sessions().len(), 1);
        // …and withdrawing the presumption puts it straight back, which is what
        // an unreachable host's answer does.
        backend.unpresume_dead(&spared);
        assert_eq!(backend.list_sessions().len(), 2);

        // The other ending: the host confirms with a `Removed` of its own, which
        // retires the row *and* the presumption standing in for it — so a
        // launcher pid the host later recycles can't inherit a hide meant for
        // its predecessor.
        let doomed = SessionKey::from_launcher_pid(101);
        backend.presume_dead(&doomed);
        assert_eq!(backend.list_sessions().len(), 1);
        assert_eq!(
            tokio::task::block_in_place(|| backend.kill_session(&doomed)),
            KillOutcome::Signalled
        );
        let mut tries = 0;
        while !backend.presumed_dead.lock().unwrap().is_empty() {
            tries += 1;
            assert!(tries < 100, "the host's Removed never retired the guess");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(backend.list_sessions().len(), 1);

        let _ = std::fs::remove_file(&sock);
    }

    /// A mock that serves one snapshot per connection, dropping between them on
    /// a signal so the test can drive a disconnect deterministically. The last
    /// connection is held open (reads until EOF) so the mirror stays populated
    /// while the test asserts against it.
    async fn scripted_mock(
        listener: UnixListener,
        snapshots: Vec<Vec<LauncherState>>,
        mut drop_between: mpsc::UnboundedReceiver<()>,
    ) {
        let n = snapshots.len();
        for (i, snap) in snapshots.into_iter().enumerate() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = BufReader::new(rd);
            let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(
                &mut wr,
                &ServerFrame::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                    host: "mock".into(),
                },
            )
            .await
            .unwrap();
            let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(&mut wr, &ServerFrame::Snapshot { sessions: snap })
                .await
                .unwrap();
            if i + 1 < n {
                // Hold this connection until the test says "drop now", then let
                // the stream fall out of scope → the client sees EOF and reconnects.
                let _ = drop_between.recv().await;
            } else {
                // Last connection: keep it open so the mirror stays filled.
                while matches!(read_frame::<_, ClientFrame>(&mut rd).await, Ok(Some(_))) {}
            }
        }
    }

    async fn wait_for_len(backend: &RemoteBackend, want: usize) {
        let mut tries = 0;
        while backend.list_sessions().len() != want {
            tries += 1;
            assert!(
                tries < 300,
                "mirror never reached {want} sessions (have {})",
                backend.list_sessions().len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_serves_host_fs_queries() {
        let sock = std::env::temp_dir().join(format!("cm-test-hostfs-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(listener, vec![]));
        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        // recent_dirs: the remote's list arrives host-canonical, with no home.
        let cwds = tokio::task::block_in_place(|| backend.recent_dirs());
        assert_eq!(cwds, vec!["~/proj", "~/other"]);

        // complete_path: the prefix reaches the server and matches come back.
        let matches = tokio::task::block_in_place(|| backend.complete_path("/home/u/a"));
        assert_eq!(matches, vec!["/home/u/aalpha/", "/home/u/aapple/"]);

        // dir_exists: true only for the path the mock recognizes.
        assert!(tokio::task::block_in_place(
            || backend.dir_exists("/home/u/proj")
        ));
        assert!(!tokio::task::block_in_place(
            || backend.dir_exists("/home/u/nope")
        ));

        // forget_recent_dir: the delete lands on the *host's* list, so the next
        // read no longer serves it. Fire-and-forget, so the assertion has to
        // wait for the round trip the keystroke deliberately doesn't.
        backend.forget_recent_dir("~/proj");
        let mut tries = 0;
        loop {
            let cwds = tokio::task::block_in_place(|| backend.recent_dirs());
            if cwds == ["~/other"] {
                break;
            }
            tries += 1;
            assert!(tries < 100, "the host never forgot the dir (have {cwds:?})");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = std::fs::remove_file(&sock);
    }

    /// The rule the panel's figures rest on, in the cell that enforces it: what
    /// a row shows is either an answer that has landed or visibly not a number.
    /// Nothing here ever hands back a reading from before the last
    /// invalidation.
    #[test]
    fn vitals_are_a_landed_answer_or_visibly_not_a_number() {
        let interval = Duration::from_secs(60);
        let cell = VitalsCell::default();
        // Nothing asked yet, so nothing to show but the spinner.
        assert_eq!(cell.get(), VitalsView::Loading);
        let reading = HostVitals {
            cpu_percent: Some(12.0),
            mem_used_bytes: Some(4),
            mem_total_bytes: Some(8),
        };
        // An answer is the only thing that puts figures on the row...
        assert!(cell.claim_poll(interval));
        cell.settle(Some(reading));
        assert_eq!(cell.get(), VitalsView::Reading(reading));
        // ...and a poll that comes back empty-handed takes their place rather
        // than leaving them to stand for a host that has stopped answering.
        cell.settle(None);
        assert_eq!(cell.get(), VitalsView::Unavailable);
        // A host with nothing to say still answered: that's a reading (which
        // renders as no figures), not a failed probe.
        cell.settle(Some(HostVitals::default()));
        assert_eq!(cell.get(), VitalsView::Reading(HostVitals::default()));
        // Invalidation — the panel opening, or a link coming back — drops the
        // answer rather than letting it describe a moment that has passed, and
        // re-arms the poll, so the spinner it leaves behind lasts a round trip
        // and not the rest of the interval.
        assert!(!cell.claim_poll(interval));
        cell.invalidate();
        assert_eq!(cell.get(), VitalsView::Loading);
        assert!(cell.claim_poll(interval));
    }

    /// A poll fetches the host's reading, raises the redraw-only signal, and —
    /// the part that matters — leaves the *session* signal alone, so it can
    /// never reach the reload path. The throttle then holds the next ask back,
    /// which is what keeps an open panel to one request per interval per host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_vitals_poll_fetches_a_reading_without_arming_a_reload() {
        let sock = std::env::temp_dir().join(format!("cm-test-vitals-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(listener, vec![test_state(1)]));
        let mut backend = Backend::Remote(RemoteBackend::connect(
            Transport::LocalSocket(sock.clone()),
            HostId("mock".into()),
        ));
        let events = backend.subscribe();
        // Nothing is asked until the panel asks — the point of polling. So
        // whether or not the link is up yet, there are no figures.
        assert!(!matches!(backend.vitals(), Some(VitalsView::Reading(_))));

        let mut tries = 0;
        let vitals = loop {
            if let Some(VitalsView::Reading(v)) = backend.vitals() {
                break v;
            }
            tries += 1;
            assert!(tries < 300, "no reading ever arrived");
            // Idempotent while one is in flight, so calling it every loop pass
            // (as the run loop does) is safe.
            backend.poll_vitals();
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(vitals.cpu_percent, Some(42.0));
        assert_eq!(vitals.mem_percent(), Some(25.0));
        // Fresh once, then clear until the next reply.
        assert!(events.take_vitals());
        assert!(!events.take_vitals());
        // The snapshot armed the session signal; drain it, and confirm the poll
        // didn't arm it again.
        assert!(events.take());
        assert!(!events.take());

        // Within the interval, further calls are no-ops: no new reply, so no
        // new redraw signal.
        for _ in 0..5 {
            backend.poll_vitals();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!events.take_vitals());
        // The answer stands *between* polls — the throttle is what bounds how
        // old a displayed figure gets, not a re-blank on every pass; only
        // invalidation puts a row back on the spinner.
        assert_eq!(backend.vitals(), Some(VitalsView::Reading(vitals)));
        backend.invalidate_vitals();
        assert_eq!(backend.vitals(), Some(VitalsView::Loading));

        let _ = std::fs::remove_file(&sock);
    }

    /// A daemon that predates `GetVitals` ignores it (v4 forward tolerance), so
    /// the answer is silence — the poll must give up rather than park a task on
    /// a reply that will never come, and must not leave the host looking busy
    /// with a request forever in flight. Giving up is *reported*: a host that
    /// answers nothing reads as `Unavailable` rather than spinning forever, so
    /// the row says the figures aren't coming instead of implying they are.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_poll_an_old_daemon_ignores_gives_up_and_re_arms() {
        let sock =
            std::env::temp_dir().join(format!("cm-test-vitmute-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        // `mute_mock` handshakes and snapshots, then answers nothing.
        tokio::spawn(mute_mock(listener));
        let backend = Backend::Remote(RemoteBackend::connect(
            Transport::LocalSocket(sock.clone()),
            HostId("mock".into()),
        ));
        while !matches!(backend.conn_state(), ConnState::Connected) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // A tiny deadline stands in for the real one; nothing about giving up
        // depends on its length.
        let (interval, timeout) = (Duration::from_millis(200), Duration::from_millis(100));
        backend.poll_vitals_paced(interval, timeout);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(backend.vitals(), Some(VitalsView::Unavailable));
        // And the next interval polls again rather than being stuck in flight.
        let Backend::Remote(remote) = &backend else {
            unreachable!()
        };
        assert!(remote.vitals.claim_poll(interval));

        let _ = std::fs::remove_file(&sock);
    }

    /// Handshake and snapshot, then deliberate silence — an older daemon's
    /// treatment of a frame it can't decode.
    async fn mute_mock(listener: UnixListener) {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);
        let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(
            &mut wr,
            &ServerFrame::Welcome {
                server_version: "old".into(),
                protocol: PROTOCOL_VERSION,
                host: "mock".into(),
            },
        )
        .await
        .unwrap();
        let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(&mut wr, &ServerFrame::Snapshot { sessions: vec![] })
            .await
            .unwrap();
        while matches!(read_frame::<_, ClientFrame>(&mut rd).await, Ok(Some(_))) {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_reconnects_and_resnapshots_after_a_drop() {
        let sock = std::env::temp_dir().join(format!("cm-test-reconn-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (drop_tx, drop_rx) = mpsc::unbounded_channel();
        // First connection snapshots one session; after we force a drop, the
        // second connection snapshots two — proving a re-Hello/re-Subscribe and
        // a fresh Snapshot on reconnect.
        tokio::spawn(scripted_mock(
            listener,
            vec![vec![test_state(1)], vec![test_state(1), test_state(2)]],
            drop_rx,
        ));

        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        wait_for_len(&backend, 1).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        // Force the server to drop the connection; the client must re-dial.
        drop_tx.send(()).unwrap();

        wait_for_len(&backend, 2).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        let _ = std::fs::remove_file(&sock);
    }
}
