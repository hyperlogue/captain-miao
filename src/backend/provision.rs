//! Remote binary provisioning (`docs/crate-split.md`)
//!
//! On connect, probe the remote for a version-matching `miao-server` and
//! invoke whichever copy it finds: one on PATH first (a user install — never
//! touched), else one at our cache path. If neither is usable and this build
//! carries a payload the host could run, **upload it** and use that.
//!
//! The upload is the crate split's deferred "embed + auto-deploy" work, restored
//! on the right footing. It died with the split because the dashboard stopped
//! linking the pty pool, so the only binary it could upload — itself — wouldn't
//! be a functional server. What it sends now is a real `miao-server`,
//! cross-built and embedded by `build.rs` in the same command that builds the
//! dashboard (`src/server_payload.rs`, `xtask/src/server.rs`). A dashboard built
//! without a payload — every plain `cargo build` — behaves exactly as it did
//! before: probe, don't upload, and name what's wrong.
//!
//! Ownership rule, and the reason `UsePath` sorts first: **PATH is the user's,
//! the cache path is ours.** A version-matching binary the user installed always
//! wins and is never overwritten; the cache path is refreshed to match our
//! payload exactly whenever it doesn't.
//!
//! **Everything fallible in this section returns `Result<_, String>`, not
//! `anyhow::Result`** — the one place in the dashboard that does. A provisioning
//! failure is not propagated; it is *stored and re-displayed*. `UploadGate` and
//! the terminfo gate keep it in a map to suppress the retry that would otherwise
//! repeat a multi-megabyte upload every reconnect, and `ConnLog` shows it in the
//! hosts panel verbatim. That wants a `Clone`, comparable, already-phrased-for-a-
//! human sentence, which is what these functions build and what `anyhow::Error`
//! is not. Where an error really does propagate to a caller — `open_session`,
//! `attach_plan`, `shell_plan` — it is `anyhow::Result` like everywhere else.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::PROTOCOL_MIN;
use crate::state::HostId;

#[cfg(test)]
use super::scratch_home;
use super::{
    ConnLog, REMOTE_OUTPUT_CAP, capped_output, login_shell_safe, split_connection_options,
    ssh_common_opts,
};

// =============================================================================
// Filesystem locations, and where a release lives
// =============================================================================

/// The binary's name: what it's called on the remote's `PATH`, and what a
/// `--version` line starts with.
const SERVER_BIN: &str = "miao-server";

/// The directory a deployed miao-server lives in, relative to `$HOME`.
/// The three `REMOTE_*_REL` paths have to agree; they're literals rather than
/// `concat!`-derived because `concat!` takes literals, not consts.
const REMOTE_BIN_DIR_REL: &str = ".cache/captain-miao/bin";

/// Where a deployed miao-server lives on the remote, relative to `$HOME`.
/// Shared with `redeploy.sh`, which uploads to exactly this path.
const REMOTE_CACHE_REL: &str = ".cache/captain-miao/bin/miao-server";

/// Where an in-flight upload is staged before it's verified and published,
/// relative to `$HOME`.
const REMOTE_INCOMING_REL: &str = ".cache/captain-miao/bin/miao-server.incoming";

/// Marker beside the deployed binary recording `<sha256> <target>` — the payload
/// we put there and which build of it won — relative to `$HOME`.
///
/// The digest exists because a version match is not identity: dev builds never
/// bump the version, so `0.2.1` on the host tells us nothing about *which*
/// `0.2.1`. The marker closes that — rebuild, reconnect, and the host gets the
/// new server — which is what makes `redeploy.sh`'s whole reason for existing go
/// away for payload-carrying builds.
///
/// The **target** is what makes the candidate loop terminate. With more than one
/// candidate per arch, the digest on the host is whichever one the host proved
/// it could run, which is generally *not* the one we would offer first: a NixOS
/// box settles on musl, the next connect compares its marker against our
/// preferred gnu payload, sees a mismatch, re-deploys gnu, watches the host
/// refuse it, falls back to musl — and does all of that again on every
/// reconnect, forever, at 500ms → 30s. Recording the winner makes it sticky, so
/// the candidate order is re-litigated only when there is nothing usable on the
/// host, never on one that has already answered the question.
///
/// Written with a single `echo`, so it stays free of quotes and backslashes
/// (see [`login_shell_safe`]); target triples are alphanumerics, dashes and
/// underscores, so they need no quoting.
const REMOTE_MARKER_REL: &str = ".cache/captain-miao/bin/miao-server.sha256";

/// The per-host provisioning state a connect attempt threads through.
///
/// Grouped rather than passed as three more positional parameters: two
/// `&mut UploadGate`s side by side are trivially swappable at a call site, and
/// swapping them would silently cross the upload cooldown with the download one.
pub(super) struct Provisioning<'a> {
    pub(super) upload: &'a mut UploadGate,
    pub(super) download: &'a mut UploadGate,
    /// The terminfo offer's memory. A third gate rather than a reused one
    /// because this is the only one that is **never cleared**: the other two
    /// forget once the host demonstrably works, which is right for a transient
    /// deploy failure and exactly wrong for a preference the user stated.
    pub(super) terminfo: &'a mut UploadGate,
    pub(super) host: &'a HostId,
}

/// Where a published server is downloaded from, minus the tag and filename.
///
/// The URL shape is a **three-way contract** — `xtask::server::release_url`
/// builds it, `build.yml`'s asset names produce it, and this fetches it — so
/// this copy is deliberately duplicated rather than shared through `cm-core`.
/// Sharing would hand a build-chore binary tokio, notify, tracing and a C
/// compile of the SQLite amalgamation on the path every bundled build runs
/// first, and would put a `curl`/`tar` shell-out into the portable data layer
/// that rides into `miao-server` on every host. The shared surface is a URL
/// shape and two flags — no logic — and it is already a three-way contract, so
/// sharing between two of the three never made it one implementation. Each copy
/// carries its own tests instead.
const RELEASE_BASE: &str = "https://github.com/hyperlogue/captain-miao/releases/download";

/// The published asset for one target. Mirrors `xtask::server::release_url`,
/// and pinned by a test on this side too. Pure.
fn release_url(base: &str, version: &str, target: &str) -> String {
    let version = version.trim().trim_start_matches('v');
    let base = base.trim_end_matches('/');
    format!("{base}/v{version}/{SERVER_BIN}-v{version}-{target}.tar.gz")
}

// =============================================================================
// Asking the user
// =============================================================================

/// How long the user has to answer a download prompt before the attempt gives
/// up. Bounded because the connection task is *blocked* here: a sync `Backend`
/// call from the UI thread parks in `block_in_place` waiting on this host, so an
/// unanswered popup must not wedge it indefinitely. A lapse is treated exactly
/// like a decline, and remembered the same way.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(90);

/// A pending "may I download a server?" question, on its way to the UI.
///
/// The download is the only step in this design that leaves the machine, so it
/// asks first — through the same y/N machinery `Space e` and host removal use.
pub(crate) struct ConsentPrompt {
    /// The question, already phrased by whoever is asking — the backend knows
    /// what it wants to do and the UI only renders it, so a second thing to ask
    /// about needs no new channel, no new queueing rule and no new timeout.
    pub(crate) question: String,
    /// Answered with `true` to allow. **Dropping it means no**, which is what
    /// makes every path that doesn't explicitly allow — pressing `n`, pressing
    /// Esc, closing the dashboard — decline safely. Nothing may add a
    /// reply-on-decline: the receiver treats a closed channel as a refusal.
    pub(crate) reply: oneshot::Sender<bool>,
}

/// The dashboard's end of the consent channel, set once at startup.
///
/// A process-wide `OnceLock` rather than a parameter threaded through every
/// backend constructor, mirroring `config::get()` and `terminal::get()`. Unset —
/// in tests, and anywhere there is no TUI to ask — consent is **denied**, which
/// is the safe direction: a download that nobody could have approved must not
/// happen silently.
static CONSENT: std::sync::OnceLock<mpsc::UnboundedSender<ConsentPrompt>> =
    std::sync::OnceLock::new();

/// Hand the dashboard's consent channel to the backends. Called once, from
/// `App::new`.
pub(crate) fn set_consent_channel(tx: mpsc::UnboundedSender<ConsentPrompt>) {
    let _ = CONSENT.set(tx);
}

/// How long a failed upload suppresses the next attempt for the same payload.
/// Without it, a host that accepts ssh but refuses the write (read-only `$HOME`,
/// full disk, no exec permission on the mount) would be re-sent multiple
/// megabytes on every reconnect — and the reconnect backoff caps at 30s.
const UPLOAD_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// Ceiling on one upload, so a stalled transfer can't wedge the reconnect loop
/// forever. Generous: this is multiple megabytes over whatever link the user has.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

// =============================================================================
// Probing the host
// =============================================================================

/// Which binary answered `daemon status` with a live daemon, if either did.
///
/// A daemon that is *already running* outranks the whole provisioning ladder
/// (§3.3): `daemon ensure` never restarts one — it is the pty pool — so a
/// payload uploaded while it holds the singleton `flock` cannot take effect
/// until it exits. Knowing *which* binary answered is what lets `UseRunning`
/// name an exe: the running daemon's own path is not otherwise observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunningDaemon {
    /// A daemon is up, and `miao-server` on PATH reported it.
    OnPath,
    /// A daemon is up, and the deployed cache-path binary reported it.
    InCache,
}

/// One-shot probe of a remote host: its `$HOME`, `uname -sm`, the version and
/// protocol of a miao-server on PATH / at the cache path (if any), the digest
/// marker we left beside the cached one (if any), and whether a daemon is
/// already running.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteProbe {
    home: String,
    arch: String,
    path_version: Option<String>,
    /// Wire protocol the PATH binary announced. `None` for one too old to print
    /// it, which is what makes the exact-version fallback necessary.
    path_protocol: Option<u32>,
    cache_version: Option<String>,
    cache_protocol: Option<u32>,
    cache_sha: Option<String>,
    /// The target triple of the deployed binary, from the marker's second field.
    /// `None` for a marker written before this existed, which falls back to the
    /// single-candidate rule.
    cache_target: Option<String>,
    /// A daemon already serving on this host, and which binary reported it.
    running: Option<RunningDaemon>,
    /// Whether the host has a terminfo entry for *this dashboard's* `TERM`.
    /// `None` when we didn't ask or couldn't tell — no usable local `TERM`, or a
    /// host with no `infocmp`/`tic` to answer with, where a `false` would only
    /// provoke an install we can't perform.
    terminfo: Option<bool>,
}

/// The provisioning action a probe + local facts imply. Pure + unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provision {
    /// A daemon is **already serving** on this host, so deploy nothing and just
    /// connect. Outranks the rest of the ladder because `daemon ensure` never
    /// restarts a live daemon — it *is* the pty pool — so anything we uploaded
    /// could not take effect until that daemon exited. Whether we can actually
    /// talk to it is settled by the handshake rather than by a `--version` on
    /// disk, which describes a binary that may have replaced the running
    /// process (§3.3).
    UseRunning(RunningDaemon),
    /// A protocol-compatible binary is already on PATH; invoke `miao-server`.
    UsePath,
    /// A version-matching binary is already at the cache path; invoke it there.
    UseCache,
    /// Nothing usable is there, but we can supply a payload this host might run:
    /// push it to the cache path, then use it. Carries the target as well as the
    /// digest — the target is needed to fetch the bytes and to write the marker,
    /// and the digest is what the retry cooldown keys on.
    Upload { target: String, sha256: String },
    /// Nothing version-matching anywhere and nothing to upload; fall back to
    /// `miao-server` on PATH and let the connection fail loudly.
    FallBack,
}

/// How a [`Provision`] decision reads in the connection log. Separate from
/// `Debug` because the digest an `Upload` carries is noise to the reader —
/// what they need is which of the four it chose. Pure.
fn provision_label(action: &Provision) -> &'static str {
    match action {
        Provision::UseRunning(_) => "use the daemon already running there",
        Provision::UsePath => "use the host's own miao-server",
        Provision::UseCache => "use the one already deployed",
        Provision::Upload { .. } => "deploy ours",
        Provision::FallBack => "nothing to deploy; try PATH anyway",
    }
}

/// The shell script the probe runs over ssh. Six lines out: `$HOME`, the
/// machine, a `--version` line (or our `-` sentinel) for the PATH binary and for
/// the cache-path binary, the digest marker, and which binary — if either —
/// reports a **running daemon**. `--version` errors and "command not found"
/// both land on stderr and a non-zero exit, so `|| echo -` normalizes them.
///
/// The marker is read through a variable rather than `cat`'d straight out,
/// because the parse is **positional** and a degenerate marker would shift every
/// field after it. `cat` of an *empty* file (a disk-full `echo` wrote nothing)
/// succeeds and emits no line at all, so `|| echo -` never fires and the daemon
/// line slides up into the marker's slot — misreading a running daemon as absent
/// at exactly the moment the host is already in a strange state. A marker
/// missing its trailing newline would likewise run into the next line. Assigning
/// first and echoing once guarantees exactly one line whatever the file holds.
///
/// `set -f` is why that echo can stay unquoted. Unquoted is what collapses a
/// multi-line marker onto one line — quoting it would preserve the newlines and
/// reintroduce the very shift this avoids — but unquoted also invites globbing,
/// and a marker holding a `*` would expand against the *remote's* working
/// directory and hand us a directory listing where a digest belongs. Disabling
/// pathname expansion keeps the word-splitting and drops the globbing, which is
/// exactly the pair we want.
///
/// The daemon line cannot use that same `|| echo -` trick, and the reason is
/// worth stating: `daemon status` exits **0 whether or not a daemon is
/// running** (it is a report, not a test) and prints several lines when one is,
/// so both the exit code and the line count would lie. Matching its first line
/// is the only honest read — hence `grep -q`, whose exit status *is* the
/// question, with the classification left to [`parse_probe`].
///
/// Shell-variable assignment is safe here even though `ssh` hands this to the
/// account's login shell: [`login_shell_safe`] wraps the whole thing in
/// `/bin/sh -c '…'`, so the inner dialect is always POSIX sh. The rule that
/// still binds is the wrapper's — no single quote and no backslash anywhere.
fn probe_script(terminfo: Option<&TerminfoName>) -> String {
    // Whether the host can describe *our* terminal. Asked here rather than
    // anywhere else because this is the one round trip that already exists, and
    // because the answer is only actionable during provisioning — a pooled
    // session's `TERM` is fixed when its pty is created, so the fix has to be in
    // place before the session, not after it.
    //
    // Gated on `tic` as well as `infocmp`: a `no` we can't act on is worth
    // nothing, and reporting it would make us re-attempt an install on a host
    // with no ncurses tools on every single connect.
    let terminfo = match terminfo {
        Some(term) => format!(
            "t=-; \
             if command -v infocmp >/dev/null 2>&1 && command -v tic >/dev/null 2>&1; \
             then if infocmp {term} >/dev/null 2>&1; then t=yes; else t=no; fi; fi; \
             echo t=$t"
        ),
        None => "echo t=-".to_string(),
    };
    format!(
        "set -f; \
         echo \"$HOME\"; uname -sm; \
         {SERVER_BIN} --version 2>/dev/null || echo -; \
         \"$HOME/{REMOTE_CACHE_REL}\" --version 2>/dev/null || echo -; \
         m=$(cat \"$HOME/{REMOTE_MARKER_REL}\" 2>/dev/null); \
         if [ -z \"$m\" ]; then m=-; fi; \
         echo m=$m; \
         d=-; \
         if {SERVER_BIN} daemon status 2>/dev/null | grep -q \"{DAEMON_RUNNING_MARK}\"; \
         then d=path; \
         elif \"$HOME/{REMOTE_CACHE_REL}\" daemon status 2>/dev/null | grep -q \"{DAEMON_RUNNING_MARK}\"; \
         then d=cache; fi; \
         echo $d; \
         {terminfo}"
    )
}

/// A terminfo name that has passed the allowlist below, and is therefore safe
/// to splice into a script and to hand to `infocmp` as a positional argument.
///
/// A newtype rather than a validated `String` because the invariant has to
/// outlive the call site that established it: every script this module sends is
/// wrapped by [`login_shell_safe`] in `/bin/sh -c '…'`, so a name carrying a
/// single quote does not merely break the wrapping — it *closes* it, and the
/// rest of the value runs as commands on every host the dashboard touches. The
/// value comes from `TERM`, an environment variable, which is exactly the class
/// of input that shouldn't be trusted on the strength of a comment. Making the
/// only constructor the validator means a future caller cannot forget.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminfoName(String);

impl TerminfoName {
    /// `Some` for a name that is safe to send and worth asking about.
    ///
    /// **Allowlist, not escaping.** Real terminfo names are
    /// `[A-Za-z0-9._+-]` (`xterm-kitty`, `screen.xterm-256color`), so anything
    /// else is refused outright — there is no legitimate name we'd lose, and an
    /// escaping scheme is a thing to get subtly wrong forever after.
    ///
    /// **The first character must be alphanumeric.** `-` is otherwise a
    /// perfectly good terminfo character, but a *leading* one makes the name an
    /// option to both `infocmp` and `tic`, which is a second injection grammar
    /// hiding behind the first: `TERM=-V` would have `infocmp -V` exit 0 and be
    /// read as "the host has this terminal".
    ///
    /// Universally-present names are dropped as not worth a question — every
    /// host has them, and the pool wrapper substitutes `xterm-256color` for
    /// `dumb` anyway, so the answer could only ever be `yes`.
    fn new(raw: &str) -> Option<Self> {
        let name = raw.trim();
        let safe = name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
        let worth_asking = !matches!(name, "dumb" | "xterm-256color" | "xterm" | "linux");
        (safe && worth_asking).then(|| Self(name.to_string()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TerminfoName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// This dashboard's own `TERM`, if it is a name we can safely ask a host about.
fn terminfo_to_provision() -> Option<TerminfoName> {
    TerminfoName::new(&std::env::var("TERM").ok()?)
}

/// The fragment of `miao-server daemon status`'s first line that means a daemon
/// is up — it prints `daemon:   running (pid 1234)` or `daemon:   not running`
/// (`crates/cm-server/src/server.rs`). Matched on the remote by `grep -q`, so it
/// must contain no single quote or backslash (see [`login_shell_safe`]).
const DAEMON_RUNNING_MARK: &str = "running (pid";

/// Pull the version out of a remote `<binary> --version`, tolerating anything a
/// login shell's rc files printed around it — a `fish_greeting` or an `echo` in
/// `.bashrc` lands on the same stdout, so taking "the second word of the output"
/// would read the greeting instead. Pure.
fn reported_version(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next()? == SERVER_BIN)
            .then(|| words.next())?
            .map(str::to_string)
    })
}

/// Parse [`probe_script`] output. A `--version` line is `miao-server
/// <ver>`; our `-` sentinel and a blank line map to `None`. Pure.
fn parse_probe(out: &str) -> Option<RemoteProbe> {
    let mut lines = out.lines();
    let home = lines.next()?.trim().to_string();
    let arch = lines.next()?.trim().to_string();
    if home.is_empty() || arch.is_empty() {
        return None;
    }
    // A plain fn, not a closure: closure lifetime elision can't express
    // "borrowed from the argument" for a `&str` in and a `&str` out.
    fn field(line: Option<&str>) -> Option<&str> {
        let l = line?.trim();
        (!l.is_empty() && l != "-").then_some(l)
    }
    // clap prints "<name> <version> protocol <n>" (the protocol rides the same
    // line deliberately — see the server's `version_string`), so the version is
    // the second word and the protocol the fourth. A server too old to announce
    // one yields `None`, which is what the exact-version fallback keys on.
    let split = |line: Option<&str>| -> (Option<String>, Option<u32>) {
        let Some(l) = field(line) else {
            return (None, None);
        };
        let mut words = l.split_whitespace();
        let version = words.nth(1).map(str::to_string);
        // `nth` consumed through the version, so the protocol number is one
        // past the "protocol" keyword from here.
        let protocol = (words.next() == Some("protocol"))
            .then(|| words.next())
            .flatten()
            .and_then(|n| n.parse().ok());
        (version, protocol)
    };
    let (path_version, path_protocol) = split(lines.next());
    let (cache_version, cache_protocol) = split(lines.next());
    // `<sha256> <target>`; a marker from before the target was recorded has just
    // the digest, and yields `None` for the target rather than a wrong guess.
    // The `m=` prefix is the probe's, and it is load-bearing rather than
    // decoration: `echo` treats a leading `-n` as its own flag and suppresses
    // the newline, so a corrupt marker beginning with one would merge this line
    // into the next and shift every field after it. A prefix makes the first
    // word unflaggable.
    let marker = lines
        .next()
        .and_then(|l| l.trim().strip_prefix("m="))
        .filter(|l| !l.is_empty() && *l != "-");
    let cache_sha = marker
        .and_then(|m| m.split_whitespace().next())
        .map(str::to_string);
    let cache_target = marker
        .and_then(|m| m.split_whitespace().nth(1))
        .map(str::to_string);
    let running = match field(lines.next()) {
        Some("path") => Some(RunningDaemon::OnPath),
        Some("cache") => Some(RunningDaemon::InCache),
        _ => None,
    };
    // Prefixed like the marker, and for the same reason: a bare `-` would be
    // read by `echo` as a flag.
    let terminfo = match lines
        .next()
        .map(str::trim)
        .and_then(|l| l.strip_prefix("t="))
    {
        Some("yes") => Some(true),
        Some("no") => Some(false),
        _ => None,
    };
    Some(RemoteProbe {
        home,
        arch,
        path_version,
        path_protocol,
        cache_version,
        cache_protocol,
        cache_sha,
        cache_target,
        running,
        terminfo,
    })
}

// =============================================================================
// Deciding what to deploy
// =============================================================================

/// Decide which remote binary to invoke.
///
/// `candidates` is `(target, sha256)` for every server we can supply **locally**
/// for this host, in preference order (glibc before musl) — passed as plain
/// strings rather than `&ServerPayload`s so the decision stays testable in a
/// build carrying no payload, which is every test run that sets no
/// `CM_SERVER_PAYLOAD_MANIFEST`.
///
/// "Locally" is load-bearing and not a shorthand: a payload that only the
/// downloader could supply has no digest until it has been fetched, so it cannot
/// be compared against the marker and must never appear here. Resolving one that
/// way would mean downloading a binary purely to answer a comparison — and, on a
/// host already running a perfectly good server, prompting to do it. The
/// downloader is an escalation the *caller* reaches for when this returns
/// nothing usable, not a resolution step.
///
/// `suppliable` is every target we could supply *at all*, including ones the
/// host has already refused this pass. It exists purely to keep the marker's two
/// cases apart, and conflating it with `candidates` is a real bug rather than a
/// tidiness point: "the marker names a target we cannot supply" (keep what is
/// deployed — it proved itself here) and "the marker names one we just watched
/// the host reject" (keep looking) are opposite conclusions, and `candidates`
/// alone cannot tell them apart once a refusal has filtered it.
///
/// Stays pure: the IO (env lookups, cache reads, downloads) happens at the
/// resolution edge, and the looping happens at the deploy site. Unit-tested.
fn decide_provision(
    local_version: &str,
    probe: &RemoteProbe,
    candidates: &[(&str, &str)],
    suppliable: &[&str],
) -> Provision {
    // A daemon already serving outranks the whole ladder: uploading anything
    // while it holds the singleton lock is megabytes for nothing. Note this
    // does *not* check compatibility — the handshake does, because it is the
    // only authoritative answer (§3.3), and an incompatible one is a loud
    // failure rather than a fallback (§6.11).
    if let Some(which) = probe.running {
        return Provision::UseRunning(which);
    }
    // A user install always wins, and we never overwrite it.
    if path_is_usable(local_version, probe) {
        return Provision::UsePath;
    }
    // What is already deployed at *our* path. The four cases below are what make
    // the candidate loop terminate; the ordering among them matters more than
    // any one of them.
    //
    // (2) The marker names which build won here, and its *target* is read
    // outside the version gate on purpose: the two facts it records have
    // different lifetimes. Which build is deployed is only interesting at the
    // same version — but which target this host can execute is a fact about the
    // *host*, its loader and its libc, and a release does not repeal it. Reading
    // it only at equal versions meant every version bump restarted the gnu-first
    // race on a host that had already settled the question, which the connect
    // loop recovers from at the cost of a wasted upload and the upgrade path —
    // one shot, no loop — does not recover from at all.
    //
    // So: same target, same digest, same version is this exact build, and
    // anything else re-deploys **that** target rather than the one we merely
    // prefer. The dev loop and the upgrade are the same move at different
    // versions.
    if let Some(marker) = probe.cache_target.as_deref()
        && let Some((t, sha)) = candidates.iter().find(|(t, _)| *t == marker)
    {
        if probe.cache_version.as_deref() == Some(local_version)
            && probe.cache_sha.as_deref() == Some(*sha)
        {
            return Provision::UseCache;
        }
        return Provision::Upload {
            target: (*t).to_string(),
            sha256: (*sha).to_string(),
        };
    }
    if probe.cache_version.as_deref() == Some(local_version) {
        match probe.cache_target.as_deref() {
            // (3) We can no longer supply the target the marker names — a
            // released dashboard whose host runs a downloaded musl, now offline
            // or declined. Keep it: it is the right version, and it is the
            // binary that proved itself here. Churning it for one we merely
            // prefer is exactly how the every-reconnect loop starts.
            //
            // But only when it is genuinely beyond us. A target missing from
            // `candidates` merely because the host *just refused it* is the
            // opposite situation: keeping the deployed copy there would strand
            // us on a binary we have watched fail and skip every remaining
            // candidate — on a no-loader host, exactly the musl fallback this
            // design exists to reach.
            Some(marker) if !suppliable.contains(&marker) => return Provision::UseCache,
            Some(_) => {}
            // (4) A marker written before targets were recorded: fall back to
            // the single-candidate rule this had before the loop existed.
            None => match candidates.first() {
                Some((_, sha)) if probe.cache_sha.as_deref() != Some(*sha) => {}
                _ => return Provision::UseCache,
            },
        }
    }
    // (1) Nothing usable is deployed, so the loop runs: offer our first choice
    // and let the host rule on it.
    match candidates.first() {
        Some((t, sha)) => Provision::Upload {
            target: (*t).to_string(),
            sha256: (*sha).to_string(),
        },
        // Everything we could offer is spent. A same-version binary at *our*
        // cache path beats falling back to PATH, which on a host we have been
        // deploying to is usually not there at all.
        //
        // The guard is doing more work than it looks: `cache_version` exists
        // **only because the probe ran that binary on this host seconds ago and
        // it answered**. So this arm can only ever choose something that
        // demonstrably executes there — it is not a hopeful guess, and it needs
        // no argument about libcs to be safe.
        //
        // That generality is the point, and it is easy to get wrong by reasoning
        // about the no-loader case specifically: such a host *can* report a
        // `cache_version`, because a musl server deployed there earlier runs
        // perfectly well. Picking it is exactly right. What that host cannot do
        // is report one for a glibc corpse at the same path, which is why a
        // never-successfully-provisioned no-loader host still falls through to
        // the honest failure. Both follow from the one fact above; neither needs
        // a special case.
        None if probe.cache_version.as_deref() == Some(local_version) => Provision::UseCache,
        None => Provision::FallBack,
    }
}

/// What stopping this host's daemon would deploy, when that differs from what
/// is already running there.
///
/// Exists because [`Provision::UseRunning`] short-circuits the whole ladder: a
/// connected host reports the version it *is* serving and nothing at all about
/// the one it would pick up next time, so drift is invisible until something
/// else forces a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpgradeOffer {
    /// The payload target the host would be sent.
    pub(crate) target: String,
    /// Its digest — what the deploy stages, verifies and records in the marker.
    pub(crate) sha256: String,
    /// The version we carry, to name beside the one the host is running.
    pub(crate) version: String,
    /// Which binary is serving the daemon we would have to stop. Carried here
    /// because the upgrade runs with the backend already torn down, and it is
    /// what decides how the script spells `daemon stop`.
    pub(crate) running: RunningDaemon,
    /// The remaining targets for this host, in preference order, to try if the
    /// host refuses `target`.
    ///
    /// The connect path answers a refusal by dropping that candidate and asking
    /// [`decide_provision`] again; the upgrade has no such loop to sit in — it
    /// is one keystroke, one ssh, one report — so it has to carry the rest of
    /// the list with it. Without them a host that refuses our first choice is
    /// simply stuck on its old server, with the one payload that would have
    /// worked still sitting in the dashboard.
    pub(crate) fallbacks: Vec<String>,
}

/// The [`UpgradeOffer`] for a host whose daemon is already up: what
/// [`decide_provision`] would choose with that daemon out of the picture.
///
/// **A re-decision, not a digest comparison.** The ladder's ordering *is* the
/// answer to "would a restart change anything" — a user's PATH install wins and
/// is never overwritten, a marker naming a target we can no longer supply is
/// kept — and a second implementation of that reasoning would drift from the
/// first. Only an `Upload` is an offer: every other outcome comes back on the
/// same bytes, and killing a host's sessions to redeploy what it already runs is
/// the one result this must never produce. Pure, so it is unit-tested.
///
/// The candidates it did *not* choose ride along as [`UpgradeOffer::fallbacks`]
/// — the upgrade runs once, with no loop to re-decide in, so the alternatives
/// have to travel with the decision.
fn upgrade_offer_for(
    local_version: &str,
    probe: &RemoteProbe,
    candidates: &[(&str, &str)],
    suppliable: &[&str],
) -> Option<UpgradeOffer> {
    let restarted = RemoteProbe {
        running: None,
        ..probe.clone()
    };
    match decide_provision(local_version, &restarted, candidates, suppliable) {
        Provision::Upload { target, sha256 } => Some(UpgradeOffer {
            fallbacks: candidates
                .iter()
                .map(|(t, _)| (*t).to_string())
                .filter(|t| *t != target)
                .collect(),
            target,
            sha256,
            version: local_version.to_string(),
            running: probe.running?,
        }),
        _ => None,
    }
}

/// Whether the host's own `miao-server` is one we can talk to, and so should
/// defer to rather than deploy over.
///
/// **Protocol compatibility, not version equality.** `PROTOCOL_MIN` is 4,
/// decoding above it is forward-tolerant, and v4 is documented as the last
/// refusing bump — so a 0.2.1 dashboard refusing a 0.3.0 server it could talk to
/// perfectly well was a self-inflicted deploy. Loosening this is also what makes
/// the Home Manager module sufficient on its own: a Nix host whose server came
/// from a slightly older captain-miao keeps working across dashboard upgrades,
/// with no deploy and no version lockstep — which matters because a NixOS host
/// with LDAP/SSSD users cannot be served by *any* payload we could ship.
///
/// A server too old to announce a protocol falls back to the exact-version
/// rule, since nothing else can be inferred about it. Pure.
fn path_is_usable(local_version: &str, probe: &RemoteProbe) -> bool {
    match probe.path_protocol {
        Some(p) => cm_core::protocol::protocol_compatible(p),
        None => probe.path_version.as_deref() == Some(local_version),
    }
}

/// Why a connection to an **already-running** daemon cannot proceed, phrased for
/// the hosts panel.
///
/// This is a hard failure rather than a fallback, and deliberately never an
/// automatic restart: the daemon *is* the pty pool, so stopping it kills every
/// pooled session on the host — which is why `daemon stop` itself refuses
/// without `--force`. No upload can help either, since the running daemon holds
/// the singleton `flock` until it exits. So the honest outcome is to say what is
/// wrong and hand the user the one command that fixes it (§6.11).
///
/// **Word order is load-bearing.** The hosts-panel row flattens this and
/// truncates it to the row width, so the actionable clause has to come early:
/// the mismatch first, the remedy second, and the consequence last where only
/// the connection log (`l`) is guaranteed to carry it. The natural phrasing puts
/// the remedy at the end, which is exactly where the row cuts it off.
///
/// Note the severity: this is *not* the soft indicator a stale-but-compatible
/// PATH server gets. That one is an annotation on a working host; this one means
/// the connection cannot happen at all. Pure.
pub(super) fn incompatible_daemon_reason(server_version: &str, protocol: u32) -> String {
    format!(
        "host runs miao-server {server_version} protocol {protocol}, need \u{2265} {PROTOCOL_MIN} \
         — run `miao-server daemon stop` there to upgrade \
         (this kills its pooled sessions)"
    )
}

/// The **loud** half of "assume it's there, verify, and fail loudly" (§4): turn
/// a fall-back decision into a sentence the hosts panel can show, instead of the
/// generic connection failure a missing or stale server used to produce.
/// `None` when the provision succeeded and there is nothing to report.
///
/// `upload_error` is the reason an attempted deploy didn't land; it takes
/// precedence, because "we tried to fix this for you and here's what stopped us"
/// is more actionable than "not found".
///
/// `supplied` is what the **source chain** could actually offer for this host,
/// not merely what is compiled in. Those stopped being the same thing once env
/// vars, the cache and the downloader joined the chain, and reporting the
/// embedded table alone would tell a user with a perfectly good
/// `CAPTAIN_MIAO_SERVER_DIR` that "this build carries no server payload" — true
/// of the binary, and useless as a diagnosis. Pure.
fn provision_failure(
    local_version: &str,
    probe: &RemoteProbe,
    action: &Provision,
    upload_error: Option<&str>,
    supplied: &[&str],
) -> Option<String> {
    if !matches!(action, Provision::FallBack) {
        return None;
    }
    if let Some(e) = upload_error {
        return Some(format!("could not deploy miao-server: {e}"));
    }
    let found: Vec<&str> = [
        probe.path_version.as_deref(),
        probe.cache_version.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Why we didn't just fix it ourselves: either this build ships no payloads
    // at all, or none for this host's arch.
    let cannot_deploy = if supplied.is_empty() {
        format!("no server available for {} to deploy", probe.arch)
    } else {
        format!(
            "every server we could offer for {} was refused ({})",
            probe.arch,
            supplied.join(", ")
        )
    };
    Some(match found.as_slice() {
        // No `redeploy.sh` in the advice: that script is a dev-loop convenience
        // in this repo, not something an installed user has.
        [] => format!(
            "miao-server not found (need {local_version}); {cannot_deploy} — \
             install it on the host"
        ),
        versions => format!(
            "miao-server version mismatch (found {}, need {local_version}); \
             {cannot_deploy}",
            versions.join(", ")
        ),
    })
}

/// The remote command an action resolves to: the absolute cache path for
/// `UseCache` (and for `Upload`, which lands there), else `miao-server`
/// from PATH.
fn remote_exe_for(action: &Provision, home: &str) -> String {
    match action {
        Provision::UseCache
        | Provision::Upload { .. }
        | Provision::UseRunning(RunningDaemon::InCache) => format!("{home}/{REMOTE_CACHE_REL}"),
        Provision::UsePath | Provision::FallBack | Provision::UseRunning(RunningDaemon::OnPath) => {
            "miao-server".to_string()
        }
    }
}

// =============================================================================
// The scripts we send
// =============================================================================

/// Remembers a failed upload so the next reconnect doesn't repeat it. Keyed on
/// the payload digest, so building a new server *does* get a fresh attempt
/// immediately — only re-sending the same bytes to the same host is suppressed.
/// Pure over an injected `now`, so the cooldown is unit-tested without sleeping.
#[derive(Default)]
pub(super) struct UploadGate {
    /// digest → (when it failed, what the host said).
    ///
    /// **A map, not a single slot**, and that is the whole point: with more than
    /// one candidate per host, one remembered failure is evicted by the next. A
    /// NixOS box with LDAP/SSSD users refuses *both* payloads — gnu has no
    /// loader, musl fails the self-check — so a single slot would remember only
    /// musl, leave gnu unsuppressed on the next pass, and re-send both, forever,
    /// at a backoff that caps at 30s. Remembering each independently is what
    /// makes the wasted transfer once per host rather than once per reconnect.
    ///
    /// A `None` stamp means **never retry, until the gate is cleared** — that is
    /// a deliberate refusal, which is a different thing from a transient failure
    /// and must not expire on a timer. A 5-minute cooldown on a decline means
    /// the popup returns twice an hour forever on a host the user already said
    /// no to.
    failed: HashMap<String, (Option<Instant>, String)>,
}

impl UploadGate {
    /// The remembered error, if `sha` is still suppressed.
    fn suppressed(&self, sha: &str, now: Instant) -> Option<&str> {
        let (at, error) = self.failed.get(sha)?;
        match at {
            // A refusal stands until something clears it.
            None => Some(error.as_str()),
            Some(t) => (now.duration_since(*t) < UPLOAD_RETRY_COOLDOWN).then_some(error.as_str()),
        }
    }

    /// Remember a failure that is worth retrying after a cooldown — a full disk,
    /// a refused write, a 404 that might be a release still publishing.
    fn record_failure(&mut self, sha: &str, now: Instant, error: String) {
        self.failed.insert(sha.to_string(), (Some(now), error));
    }

    /// Remember a **decision**, which no amount of waiting changes. Cleared only
    /// when the host actually works, so saying no is never permanent — but it is
    /// also never re-asked on a timer.
    fn record_refusal(&mut self, sha: &str, error: String) {
        self.failed.insert(sha.to_string(), (None, error));
    }

    /// Forget every remembered failure — called once a connection actually
    /// works, so a transient problem doesn't hold the cooldown past its
    /// usefulness.
    pub(super) fn clear(&mut self) {
        self.failed.clear();
    }
}

/// The script the remote runs while we stream the binary into its stdin.
///
/// Staged through a temp file and moved into place only after the host itself
/// has both **run it and agreed it is the right version**: a truncated transfer
/// or a payload for the wrong ABI fails the `self-check` line, a
/// wrong-versioned one fails the `grep`, `set -e` aborts either way, and nothing
/// was ever visible at the path the next connect will invoke. The run is also
/// what covers the one thing `uname` can't tell us, glibc vs musl.
///
/// **The version has to be checked here, not just by the caller.** It used to be
/// compared dashboard-side from the script's output — which is *after* the `mv`
/// has already happened, so a binary we then rejected had already replaced a
/// working deployment and rewritten its marker. That is reachable now that a
/// payload can come from an env var pointing at any build: the next probe sees
/// a cache version that doesn't match, re-uploads the same stale binary, and
/// repeats every cooldown. The caller's parse stays as a belt, but the property
/// the design claims — nothing unusable becomes the binary the next connect
/// invokes — only holds if the host refuses *before* publishing.
///
/// **Why `self-check` and not `--version`.** `--version` proves the file loads
/// and matches; it never resolves user information, so a static-musl server on
/// a host whose users come from LDAP/SSSD passes it, installs, and then fails on
/// *first attach* — the pool resolves the user with `getpwuid_r` and errors when
/// NSS has nothing to answer with. `self-check` makes the host answer the
/// question that actually matters: can this binary host a session here? It
/// prints the same `miao-server <ver> …` shape, so the reply is parsed exactly
/// as before. (One consequence worth knowing: a binary predating `self-check` —
/// one handed over via the env vars, or fetched from an older release — fails
/// this as a clap usage error rather than a version mismatch. Acceptable; the
/// deploy refuses either way, which is the safe direction.)
///
/// Two constraints shape how it's written, both from [`login_shell_safe`]: no
/// single quote and no backslash anywhere in it. Hence `echo` for the marker
/// rather than `printf '%s\n'`, and hence clearing the temp file at the *start*
/// of the run rather than with an `EXIT` trap — a failed deploy leaves its temp
/// behind, which costs some cache-directory space until the next attempt and
/// buys a script that runs everywhere. Pure, so all of this is unit-tested.
fn upload_script(sha256: &str, target: &str) -> String {
    format!(
        "set -e; {}; {}",
        stage_steps(),
        publish_steps(sha256, target)
    )
}

/// Everything up to and including the host's verdict: stream the binary into a
/// temp file, make it executable, run it, and refuse a version that isn't ours.
/// Leaves `$t` naming the verified file and **nothing** at the published path.
///
/// Split out from [`publish_steps`] so the two halves can be separated by
/// something else — see [`upgrade_script`], where what goes between them is
/// stopping the host's daemon. On its own this half is inert: it can be run
/// against a host in any state without disturbing it.
fn stage_steps() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "t=\"$HOME/{REMOTE_INCOMING_REL}\"; \
         mkdir -p \"$HOME/{REMOTE_BIN_DIR_REL}\"; \
         rm -f \"$t\"; \
         cat > \"$t\"; \
         chmod 0755 \"$t\"; \
         out=$(\"$t\" self-check); \
         echo \"$out\"; \
         echo \"$out\" | grep -q \"{SERVER_BIN} {version} \""
    )
}

/// Publish what [`stage_steps`] verified, and record which build it was.
/// Assumes `$t` is set and the marker is written *after* the `mv`, so a crash
/// between them leaves a good binary described by a stale marker (the next probe
/// re-deploys) rather than a stale binary described by a good one.
fn publish_steps(sha256: &str, target: &str) -> String {
    format!(
        "mv -f \"$t\" \"$HOME/{REMOTE_CACHE_REL}\"; \
         echo {sha256} {target} > \"$HOME/{REMOTE_MARKER_REL}\""
    )
}

/// The hosts-panel upgrade, as one `set -e` script: stage, verify, **stop the
/// daemon**, publish.
///
/// The ordering is the whole feature. Everything destructive sits downstream of
/// the host's own `self-check`, so a wrong-ABI payload, a truncated transfer or
/// a stale build costs a transfer and nothing else — the daemon is still
/// serving and its pooled sessions are untouched. `set -e` is what enforces
/// that; there is no arm here that reaches the stop on a failed verify.
///
/// The publish is downstream of the *stop* for a second, less obvious reason:
/// `mv`-ing onto a live daemon's own path leaves its `/proc/<pid>/exe` reading
/// `(deleted)`, and the launcher argv it bakes into new reservations comes from
/// `current_exe()` — so a session opened in that window would carry a path that
/// cannot be executed. Stopping first makes that unreachable rather than
/// unlikely.
///
/// Which is load-bearing on `daemon stop` **waiting**, not merely signalling:
/// it returns once the daemon has exited and its pool with it, so the `mv`
/// below lands on a path nothing is running from, and this whole script only
/// completes — releasing the dashboard to reconnect and resume — once the host
/// has no sessions left to duplicate.
///
/// `stop_exe` is spelled `$HOME`-relative rather than passed as a resolved path:
/// this string is wrapped by [`login_shell_safe`], which forbids a single quote
/// anywhere in it, and a home directory is not ours to make promises about.
fn upgrade_script(sha256: &str, target: &str, running: RunningDaemon) -> String {
    let stop_exe = match running {
        RunningDaemon::OnPath => SERVER_BIN.to_string(),
        RunningDaemon::InCache => format!("\"$HOME/{REMOTE_CACHE_REL}\""),
    };
    format!(
        "set -e; {}; {stop_exe} daemon stop --force; {}",
        stage_steps(),
        publish_steps(sha256, target)
    )
}

// =============================================================================
// Doing it: probe, deploy, install a terminfo
// =============================================================================

/// Bounds on fetching a published server. The download is the one step that
/// leaves the machine, so what is on the far end is a web server — which may be
/// slow, may be enormous, and (a redirect chain later) may not be the one the
/// URL named.
///
/// `DOWNLOAD_TIMEOUT` is generous because this is tens of megabytes over
/// whatever link the user has; `GRACE` exists so curl's own `--max-time` fires
/// first and produces the message, leaving the outer timeout as the backstop
/// for a curl that doesn't honour it. `EXTRACT_TIMEOUT` bounds a gzip bomb's
/// running time, and `MAX_SERVER_BYTES` bounds what a bomb can leave behind —
/// the archive cap can't, since the whole point of a bomb is the ratio.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const GRACE: Duration = Duration::from_secs(15);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SERVER_BYTES: u64 = 512 * 1024 * 1024;

/// How long a probe may take. It is five `--version` calls and a `cat` over an
/// already-primed ControlMaster; the generous end of that is still seconds.
/// Needed because `ConnectTimeout` covers only the handshake and `ServerAlive*`
/// only a host that goes *silent* — neither bounds a host that answers slowly
/// forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Run [`probe_script`] on the remote (this also primes the ControlMaster).
async fn probe_remote(target: &str, opts: &[String]) -> Option<RemoteProbe> {
    let child = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(&probe_script(
            terminfo_to_provision().as_ref(),
        )))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout below ends the attempt by dropping the future, which
        // would otherwise leave an ssh child talking to nobody.
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let (status, stdout, _stderr) =
        tokio::time::timeout(PROBE_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .ok()?
            .ok()?;
    if !status.success() {
        return None;
    }
    parse_probe(&stdout)
}

/// Teach the host this terminal's terminfo, by piping the local entry into the
/// remote's `tic` — the same "stream it in over the connection the probe already
/// opened" shape as [`upload_server`], at a thousandth of the size.
///
/// **Why it belongs in provisioning.** Without the entry, everything that runs
/// on that host falls back: the pool wrapper rewrites `TERM` to
/// `xterm-256color` when `infocmp` can't resolve it, and — because libshpool
/// fixes a session's environment when it *spawns* the command — that rewrite is
/// permanent for the session's whole life. So this has to land before the
/// session, and provisioning is the only phase that runs before one. Sessions
/// already created keep the terminfo they were born with; the detail panel's
/// warning is what still names those.
///
/// Installs into `$HOME/.terminfo`, which needs no privilege and which ncurses
/// searches ahead of the system directories. `-o` names the directory outright
/// rather than relying on tic's own not-root fallback, so the destination is the
/// same on every host.
///
/// Verified the way the upload is: not by tic's exit status but by asking the
/// host to resolve the name afterwards, which is the thing we actually want to
/// be true. A failure is returned, never fatal — a host that can't take the
/// entry still runs sessions, just in `xterm-256color`.
async fn install_terminfo(
    target: &str,
    opts: &[String],
    term: &TerminfoName,
) -> Result<(), String> {
    // No shell on this side — an argv, so the name needs no quoting here. It is
    // still a [`TerminfoName`], because a leading `-` would make it an *option*
    // to infocmp rather than the terminal to describe.
    let local = Command::new("infocmp")
        .arg("-x")
        .arg(term.as_str())
        .output()
        .await
        .map_err(|e| format!("running local infocmp: {e}"))?;
    if !local.status.success() || local.stdout.is_empty() {
        return Err(format!("this machine has no terminfo source for {term}"));
    }

    let mut child = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(&terminfo_install_script(term)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ssh: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let source = local.stdout;
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&source).await?;
        stdin.shutdown().await
    });
    // Capped, not `wait_with_output`: the host is on the other end of this and
    // has no obligation to be brief (see [`capped_output`]).
    let (status, stdout, stderr) =
        tokio::time::timeout(TERMINFO_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| format!("timed out after {}s", TERMINFO_TIMEOUT.as_secs()))?
            .map_err(|e| format!("ssh failed: {e}"))?;
    let _ = writer.await;

    // Both halves: the script's own exit status *and* its marker. Either alone
    // can lie — ssh reports the remote status faithfully but a login shell's rc
    // can exit 0 on its own, and stdout is shared with whatever that rc printed.
    if !status.success() || !terminfo_took(&stdout) {
        let stderr: String = stderr.trim().chars().take(200).collect();
        return Err(if stderr.is_empty() {
            format!("the host did not take it (rc={:?})", status.code())
        } else {
            stderr
        });
    }
    Ok(())
}

/// A terminfo entry is a couple of kilobytes; anything slower than this is a
/// sick link, and the connection behind it has its own troubles to report.
const TERMINFO_TIMEOUT: Duration = Duration::from_secs(20);

/// The remote half of [`install_terminfo`]: compile the entry arriving on stdin
/// into `~/.terminfo`, then prove the name resolves. The final `echo` is the
/// contract — tic's own exit status says the file compiled, not that ncurses
/// will find it. Quote- and backslash-free for [`login_shell_safe`]; pure.
/// Pinned by `every_script_we_send_survives_the_wrapping_that_defeats_a_login_shell`.
fn terminfo_install_script(term: &TerminfoName) -> String {
    format!(
        "d=\"$HOME/.terminfo\"; mkdir -p \"$d\"; \
         tic -x -o \"$d\" - && infocmp {term} >/dev/null 2>&1 && echo {TIC_OK_MARK}"
    )
}

/// What the install script prints when the entry both compiled *and* resolves.
///
/// Distinctive, and matched as a whole line, because the host's stdout is not
/// ours alone: `ssh` runs the command through the account's **login shell**, so
/// a `fish_greeting` or an `echo` in `.bashrc` lands on the same stream — the
/// same hazard [`reported_version`] exists for. A plain `ok` would let a chatty
/// rc file report a success that never happened, and the failure would then
/// surface much later as a session mysteriously running in `xterm-256color`.
const TIC_OK_MARK: &str = "cm-terminfo-installed";

/// Whether the host's output actually claims the install landed. Whole-line
/// match on [`TIC_OK_MARK`], so neither a greeting mentioning it in passing nor
/// a prompt fragment counts. Pure.
fn terminfo_took(stdout: &str) -> bool {
    stdout.lines().any(|l| l.trim() == TIC_OK_MARK)
}

/// Upgrade a host that is already running a daemon: stage our server there,
/// let the host verify it, then stop the daemon and publish — [`upgrade_script`]
/// with the payload on its stdin.
///
/// **The caller must have taken this host's backend down first.** The reconnect
/// backoff floors at 500ms, so a redial landing between the stop and the `mv`
/// would run `daemon ensure` against the *old* binary still at the cache path
/// and resurrect it — after which the probe reports `UseRunning` on the old
/// version and every session was killed for nothing. Suspending the host is what
/// makes that window not exist; it is also what lets the fresh dial afterwards
/// find exactly our digest and resolve straight to `UseCache`.
///
/// The digest sent is the one resolved *now*, not the one the offer was minted
/// with: a rebuild between the two is the dev loop, and the marker has to
/// describe what actually landed.
///
/// **A refusal moves to the next candidate**, the same way the connect path's
/// loop does and for the same reason: `uname` cannot report a libc, so which
/// payload a host can run is settled by watching it try. Everything destructive
/// in [`upgrade_script`] sits downstream of the host's own `self-check`, so a
/// refused candidate cost a transfer and left the daemon serving — which is what
/// makes trying the next one safe rather than reckless. It stays safe in the
/// other order too: a failure *after* the stop leaves the daemon down, and the
/// next candidate's `daemon stop --force` exits 0 with nothing to stop, so the
/// fallback is the recovery rather than a second casualty.
pub(crate) async fn upgrade_host_server(
    target: &str,
    options: &[String],
    offer: &UpgradeOffer,
) -> Result<(), String> {
    let (extra, _forwards) = split_connection_options(options);
    let opts = ssh_common_opts(&crate::state::ssh_control_path(target), &extra);
    // The preferred candidate's failure is the reported one, as on the connect
    // path: it is the one whose refusal explains why we are on a fallback at
    // all. The rest are logged, so a two-refusal host is still diagnosable.
    let mut first_error: Option<String> = None;
    for t in std::iter::once(&offer.target).chain(offer.fallbacks.iter()) {
        let Some(payload) = crate::server_payload::resolve_target(t) else {
            first_error.get_or_insert(format!(
                "no {t} server to deploy any more — the payload this offer named is gone"
            ));
            continue;
        };
        match upload_server(
            target,
            &opts,
            &payload,
            &upgrade_script(&payload.sha256, &payload.target, offer.running),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    target: "captain_miao::provision",
                    "{target}: upgrade to {t} refused: {e}"
                );
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| format!("nothing left to deploy for {}", offer.target)))
}

/// Stream an embedded server payload to the host's cache path over the ssh
/// connection the probe already opened (so it costs no extra authentication —
/// the ControlMaster is up by now).
///
/// The binary goes in over **stdin** rather than via `scp`: `scp` would need a
/// local temp file holding a multi-megabyte executable, and a second remote
/// command to chmod and move it, where `cat > tmp` is one round trip with no
/// local artifact. The payload is inflated here rather than shipped compressed,
/// which deliberately trades bandwidth for having no decompressor requirement on
/// a host whose entire distinguishing feature is that nothing is installed on it
/// yet.
async fn upload_server(
    target: &str,
    opts: &[String],
    payload: &crate::server_payload::Candidate,
    script: &str,
) -> Result<(), String> {
    let bytes = payload
        .bytes()
        .map_err(|e| format!("reading the {} payload: {e}", payload.target))?;
    // A payload the *user* pointed us at is checked before it leaves: a
    // store-linked binary filed under a generic triple looks entirely correct
    // and fails on every host but the one that built it, and finding that out
    // after a multi-megabyte upload is a bad trade for one header read.
    if payload.is_locally_sourced() {
        crate::server_payload::check_interpreter(&bytes, &payload.target)?;
    }
    let len = bytes.len();
    tracing::info!(
        target: "captain_miao::provision",
        "{target}: deploying {} server from {} ({len} bytes) to ~/{REMOTE_CACHE_REL}",
        payload.target,
        payload.source.label()
    );

    let mut child = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(script))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout below is enforced by dropping the future, which would
        // otherwise leave an ssh child holding a half-written temp file.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ssh: {e}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    // Feed stdin from a task while `wait_with_output` drains stdout/stderr:
    // doing both from one task deadlocks the moment either pipe fills.
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await
    });

    // Capped like the probe: the host is on the far end and the deploy script's
    // whole reply is one `--version` line (see [`capped_output`]).
    let (status, stdout, stderr) =
        tokio::time::timeout(UPLOAD_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| format!("timed out after {}s", UPLOAD_TIMEOUT.as_secs()))?
            .map_err(|e| format!("ssh failed: {e}"))?;
    // A write error here is usually the *consequence* of the remote script
    // failing (it exited, closing the pipe), so the script's own stderr below is
    // the better message; only report this one if the script looked fine.
    let write_err = match writer.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("sending the binary: {e}")),
        Err(e) => Some(format!("upload task: {e}")),
    };

    if !status.success() {
        let stderr: String = stderr.trim().chars().take(200).collect();
        return Err(if stderr.is_empty() {
            write_err.unwrap_or_else(|| format!("host rejected it (rc={:?})", status.code()))
        } else {
            stderr
        });
    }
    if let Some(e) = write_err {
        return Err(e);
    }
    // The script echoed what the *host* got from `<binary> --version`, which is
    // the real proof it both landed intact and can run there.
    let expected = env!("CARGO_PKG_VERSION");
    if reported_version(&stdout).as_deref() != Some(expected) {
        return Err(format!(
            "deployed binary reported {:?}, expected {SERVER_BIN} {expected}",
            stdout.trim().chars().take(120).collect::<String>()
        ));
    }
    tracing::info!(target: "captain_miao::provision", "{target}: deployed {expected} ({} bytes)", len);
    Ok(())
}

/// Ask the user whether we may fetch a server, and wait for the answer.
///
/// Returns `false` on a decline, on a lapse, and whenever there is no UI to ask
/// — every ambiguous outcome refuses, because this is the one step that leaves
/// the machine.
async fn ask_consent(question: String) -> bool {
    let Some(tx) = CONSENT.get() else {
        return false;
    };
    let (reply, rx) = oneshot::channel();
    if tx.send(ConsentPrompt { question, reply }).is_err() {
        return false;
    }
    // A closed channel is a decline: the UI drops the sender on `n`, on Esc, and
    // on quit, so refusal needs no message of its own.
    matches!(
        tokio::time::timeout(CONSENT_TIMEOUT, rx).await,
        Ok(Ok(true))
    )
}

/// Fetch a published server into the XDG cache, and return where it landed.
///
/// Two guards travel with the download, both mirroring `xtask`'s copy and both
/// tested here independently:
///
/// - `--proto =https` is re-asserted rather than trusted from the URL, because
///   `--location` is on and GitHub bounces release downloads to S3 — the scheme
///   has to hold on *every* hop, not just the first.
/// - the archive member is extracted **by name**, so a `../` entry has nothing
///   to land on, and the result is rejected unless it is a regular file: `tar`
///   will happily extract an entry recorded as a symlink, and reading through
///   one would pull in a file from outside the staging directory.
async fn download_server(target: &str, url: &str) -> Result<std::path::PathBuf, String> {
    let dest = crate::server_payload::cache_path_for(target)
        .ok_or_else(|| "no cache directory available".to_string())?;
    let dir = dest
        .parent()
        .ok_or_else(|| "bad cache path".to_string())?
        .to_path_buf();
    // A fresh directory per fetch: `tar` extracts over whatever is there, so a
    // failed download followed by an extract of a *previous* archive would
    // silently install a stale binary.
    // std, not tokio::fs: these are metadata-sized local operations, and the
    // dashboard's tokio deliberately does not enable the `fs` feature.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let tgz = dir.join("server.tar.gz");

    // Both bounds are curl's own as well as ours: `--max-time` is what actually
    // stops a server that accepts the connection and then dribbles (a stall no
    // connect timeout covers), and it gets to produce the error message, so the
    // outer timeout below is only the backstop for a curl that ignores it.
    let child = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--max-time",
            &DOWNLOAD_TIMEOUT.as_secs().to_string(),
            "--max-filesize",
            &MAX_ARCHIVE_BYTES.to_string(),
            "--output",
        ])
        .arg(&tgz)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning curl: {e}"))?;
    let (status, _, stderr) = tokio::time::timeout(
        DOWNLOAD_TIMEOUT + GRACE,
        capped_output(child, REMOTE_OUTPUT_CAP),
    )
    .await
    .map_err(|_| format!("download timed out after {}s", DOWNLOAD_TIMEOUT.as_secs()))?
    .map_err(|e| format!("curl failed: {e}"))?;
    if !status.success() {
        let err: String = stderr.trim().chars().take(200).collect();
        return Err(if err.is_empty() {
            format!("download failed (rc={:?})", status.code())
        } else {
            err
        });
    }

    let child = Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&dir)
        .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning tar: {e}"))?;
    let (status, _, stderr) =
        tokio::time::timeout(EXTRACT_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| format!("extract timed out after {}s", EXTRACT_TIMEOUT.as_secs()))?
            .map_err(|e| format!("tar failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "the archive did not contain {SERVER_BIN}: {}",
            stderr.trim()
        ));
    }
    let _ = std::fs::remove_file(&tgz);

    let meta = std::fs::symlink_metadata(&dest)
        .map_err(|e| format!("{url} did not yield {SERVER_BIN}: {e}"))?;
    if !meta.is_file() {
        let _ = std::fs::remove_file(&dest);
        return Err(format!(
            "{url} did not yield a regular file at {SERVER_BIN}"
        ));
    }
    // The archive was capped on the wire, but gzip expands: a small download can
    // still be a large file on disk. Checked after the fact rather than
    // prevented, because there is no portable way to bound what `tar` writes —
    // so the cost of a bomb is bounded by the extract timeout, and this is what
    // stops the result being *used*.
    if meta.len() > MAX_SERVER_BYTES {
        let _ = std::fs::remove_file(&dest);
        return Err(format!(
            "{url} yielded a {}MB {SERVER_BIN}, which is not a server binary",
            meta.len() / 1_000_000
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(dest)
}

/// Source (5): ask, fetch, and cache a published server for the first target we
/// can't supply locally. `None` when there is nothing left to try.
///
/// Rate-limited by the same gate shape the upload uses, and for the same reason:
/// the reconnect backoff caps at 30s, so a 404 for an arch we never published —
/// or a user who said no — would otherwise be re-attempted twice a minute
/// forever. A **decline is remembered exactly like a failure**, which is the one
/// refinement "confirm every time" needs: without it the popup returns every 500
/// ms → 30 s and a declined host becomes unusable. A successful connection
/// clears the memory, so saying no is not permanent.
async fn try_download_candidate(
    arch: &str,
    available: &[crate::server_payload::Candidate],
    refused: &[String],
    gate: &mut UploadGate,
    host: &HostId,
    log: &ConnLog,
) -> Option<crate::server_payload::Candidate> {
    let version = env!("CARGO_PKG_VERSION");
    let wanted = crate::server_payload::target_candidates(arch)
        .iter()
        .find(|t| !refused.iter().any(|r| r == *t) && !available.iter().any(|c| c.target == **t))?;
    let url = release_url(RELEASE_BASE, version, wanted);
    let now = Instant::now();
    if let Some(previous) = gate.suppressed(&url, now) {
        // A decline is held until the host actually works, not until a timer
        // expires, so say which of the two this is — "yet" on a decision the
        // user made reads as though it will be re-asked shortly, and it won't.
        log.info(format!(
            "not fetching {wanted}: {previous}              (cleared when this host connects; removing and re-adding it also resets this)"
        ));
        return None;
    }
    log.info(format!("nothing local for {wanted}; asking to download it"));
    if !ask_consent(format!(
        "Download miao-server for {wanted} on host \"{}\"?\n{url}",
        host.0
    ))
    .await
    {
        log.info(format!("download of {wanted} declined"));
        gate.record_refusal(&url, "you declined the download".to_string());
        return None;
    }
    log.info(format!("downloading {url}"));
    match download_server(wanted, &url).await {
        Ok(path) => {
            log.info(format!("cached {}", path.display()));
            // Re-resolve rather than hand-building a candidate: the download
            // wrote into cache source (4), so the chain now finds it with a real
            // digest, computed from the bytes that actually landed.
            crate::server_payload::resolve_candidates(arch)
                .into_iter()
                .find(|c| &c.target == wanted)
        }
        Err(e) => {
            log.error(format!("downloading {wanted} failed: {e}"));
            gate.record_failure(&url, now, e);
            None
        }
    }
}

// =============================================================================
// Resolving the command to invoke
// =============================================================================

/// Resolve the remote command to invoke: probe → decide → (deploy) → invoke.
/// Never errors — any failure resolves to `miao-server` on PATH so the
/// rest of `setup_ssh` behaves exactly as it did before provisioning existed.
/// The second half of the pair is the *diagnosis*: a `Some(reason)` names what's
/// wrong with the remote install, for `ConnState::Failed` to carry (§4).
pub(super) async fn resolve_remote_exe(
    target: &str,
    opts: &[String],
    prov: &mut Provisioning<'_>,
    log: &ConnLog,
) -> Provisioned {
    let host = prov.host;
    let Some(probe) = probe_remote(target, opts).await else {
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: probe failed (unreachable / no shell) → PATH miao-server"
        );
        log.error("probe failed — the host is unreachable over ssh, or has no shell");
        return Provisioned {
            exe: "miao-server".to_string(),
            failure: Some("host unreachable over ssh (or no shell)".to_string()),
            upgrade: None,
            home: None,
            darwin: false,
        };
    };
    let local_version = env!("CARGO_PKG_VERSION");
    // The source chain: env vars, then what this build carries, then anything
    // downloaded earlier. Naming the source in the log matters — "deployed the
    // gnu server" is a different fact from "deployed the one you pointed
    // CAPTAIN_MIAO_SERVER_DIR at", and only one of them is our bug.
    let mut available = crate::server_payload::resolve_candidates(&probe.arch);
    log.info(format!(
        "probed {}: PATH {}, cache {}, need {local_version}; can supply {}",
        probe.arch,
        probe.path_version.as_deref().unwrap_or("none"),
        probe.cache_version.as_deref().unwrap_or("none"),
        if available.is_empty() {
            "nothing".to_string()
        } else {
            available
                .iter()
                .map(|p| format!("{} ({})", p.target, p.source.label()))
                .collect::<Vec<_>>()
                .join(", ")
        },
    ));

    // Before anything else: if the host can't describe this terminal, offer to
    // teach it. Strictly ahead of the first session, which is the only time it
    // can help — a pooled session's terminfo is fixed when its pty is created.
    //
    // **Asked, not assumed.** Deploying a server is the thing the user asked
    // for by adding the host; writing a terminfo entry into their `$HOME` is a
    // side effect they did not, and "captain-miao put files on my server" is
    // not a sentence a tool gets to earn quietly. It rides the same consent
    // channel as the download, so it inherits the queueing, the timeout, and
    // the rule that every ambiguous outcome — no UI, a lapse, Esc, quit —
    // declines.
    if probe.terminfo == Some(false)
        && let Some(term) = terminfo_to_provision()
    {
        let now = Instant::now();
        // A decline is a standing preference, not a transient failure, so it is
        // recorded with no deadline and this gate — unlike the deploy's — is
        // never cleared: a host that connects fine is exactly the host that
        // would otherwise re-ask on every reconnect, forever.
        if let Some(previous) = prov.terminfo.suppressed(term.as_str(), now) {
            log.info(format!("not installing the {term} terminfo: {previous}"));
        } else if ask_consent(format!(
            "Host \"{}\" has no terminfo for {term}.\nInstall it in ~/.terminfo there? \
             Sessions opened from this terminal will keep {term} instead of falling back \
             to xterm-256color.",
            host.0
        ))
        .await
        {
            match install_terminfo(target, opts, &term).await {
                Ok(()) => {
                    tracing::info!(target: "captain_miao::provision", "{target}: installed {term} terminfo");
                    log.info(format!(
                        "installed the {term} terminfo in ~/.terminfo — sessions opened from here keep it"
                    ));
                }
                Err(e) => {
                    tracing::warn!(target: "captain_miao::provision", "{target}: {term} terminfo install failed: {e}");
                    // A cooldown, not a refusal: a full disk or a missing tic
                    // may not be true next week, and the user said yes once.
                    prov.terminfo.record_failure(term.as_str(), now, e.clone());
                    log.error(format!(
                        "could not install the {term} terminfo ({e}); sessions here will run as xterm-256color"
                    ));
                }
            }
        } else {
            log.info(format!(
                "{term} terminfo install declined; sessions here will run as xterm-256color \
                 (removing and re-adding the host asks again)"
            ));
            prov.terminfo
                .record_refusal(term.as_str(), "you declined it".to_string());
        }
    }

    // **The candidate loop.** `uname` cannot report a libc and neither can
    // anything else we can ask cheaply, so selection is verified rather than
    // guessed: offer a payload, let the host's own `self-check` rule on it, and
    // on a refusal drop that candidate and ask the decision again. What the host
    // accepts is recorded in the marker, so this race is run once per host and
    // not once per connect.
    //
    // A failure is reported verbatim rather than retried here — the reconnect
    // loop is the retry mechanism, and `gate` is what stops it re-sending
    // megabytes every pass.
    let mut refused: Vec<String> = Vec::new();
    let mut upload_error = None;
    let action = loop {
        let candidates: Vec<(&str, &str)> = available
            .iter()
            .filter(|p| !refused.contains(&p.target))
            .map(|p| (p.target.as_str(), p.sha256.as_str()))
            .collect();
        // `suppliable` is deliberately unfiltered: it separates "cannot supply"
        // from "just refused", which the marker's cases (3) and the loop below
        // read in opposite directions.
        let suppliable: Vec<&str> = available.iter().map(|p| p.target.as_str()).collect();
        let action = decide_provision(local_version, &probe, &candidates, &suppliable);
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: arch={:?} path={:?} cache={:?}/{:?} refused={refused:?} → {action:?}",
            probe.arch, probe.path_version, probe.cache_version, probe.cache_target
        );

        let Provision::Upload { target: t, sha256 } = &action else {
            // Nothing local left to offer. Before giving up, see whether a
            // *published* server exists for a target we carry nothing for —
            // this is source (5), and it is what lets a gnu-only released
            // dashboard reach a host with no generic loader at all.
            if matches!(action, Provision::FallBack)
                && let Some(fetched) = try_download_candidate(
                    &probe.arch,
                    &available,
                    &refused,
                    prov.download,
                    host,
                    log,
                )
                .await
            {
                available.push(fetched);
                continue;
            }
            log.info(format!("\u{2192} {}", provision_label(&action)));
            break action;
        };
        let payload = available
            .iter()
            .find(|p| &p.target == t)
            .expect("Upload names a candidate we offered");

        let now = Instant::now();
        let failure = match prov.upload.suppressed(sha256, now) {
            Some(previous) => {
                // Worth saying out loud: nothing was sent this time, so the
                // error below is a *remembered* one, not a fresh symptom.
                log.info(format!(
                    "deploy of {t} suppressed — the same payload failed recently"
                ));
                Some(previous.to_string())
            }
            None => {
                log.info(format!("deploying {t} from {}", payload.source.label()));
                match upload_server(
                    target,
                    opts,
                    payload,
                    &upload_script(sha256, &payload.target),
                )
                .await
                {
                    Ok(()) => None,
                    Err(e) => {
                        tracing::warn!(target: "captain_miao::provision", "{target}: deploy of {t} failed: {e}");
                        prov.upload.record_failure(sha256, now, e.clone());
                        Some(e)
                    }
                }
            }
        };
        match failure {
            None => {
                log.info(format!("deployed {t}, and the host ran it"));
                break Provision::UseCache;
            }
            Some(e) => {
                log.error(format!("{t} refused by the host:\n{e}"));
                // Keep the *first* refusal as the reported reason: it is the
                // candidate we preferred, so it is the one whose failure
                // explains the outcome. Later ones are fallbacks.
                upload_error.get_or_insert(e);
                refused.push(payload.target.clone());
            }
        }
    };

    let exe = remote_exe_for(&action, &probe.home);
    tracing::debug!(target: "captain_miao::provision", "{target}: remote exe = {exe}");
    log.info(format!("will invoke `{exe}` on the host"));
    // Only a host we deployed nothing to can still have something to gain: every
    // other arm above has already provisioned whatever it was going to.
    let upgrade = match action {
        Provision::UseRunning(_) => {
            // Nothing was refused on this pass — the upload branch is exactly
            // what `UseRunning` skips — so every candidate is still on offer.
            let candidates: Vec<(&str, &str)> = available
                .iter()
                .map(|p| (p.target.as_str(), p.sha256.as_str()))
                .collect();
            let suppliable: Vec<&str> = candidates.iter().map(|(t, _)| *t).collect();
            let offer = upgrade_offer_for(local_version, &probe, &candidates, &suppliable);
            if let Some(o) = &offer {
                log.info(format!(
                    "a restart here would deploy {} {} (the daemon is running {})",
                    o.target,
                    o.version,
                    probe
                        .cache_version
                        .as_deref()
                        .or(probe.path_version.as_deref())
                        .unwrap_or("an unknown version")
                ));
            }
            offer
        }
        _ => None,
    };
    Provisioned {
        exe,
        home: Some(probe.home.clone()),
        // `uname -sm`, so the sysname is the first word.
        darwin: probe.arch.split_whitespace().next() == Some("Darwin"),
        failure: provision_failure(
            local_version,
            &probe,
            &action,
            upload_error.as_deref(),
            &available
                .iter()
                .map(|p| p.target.as_str())
                .collect::<Vec<_>>(),
        ),
        upgrade,
    }
}

/// What [`resolve_remote_exe`] settled: the command to invoke on the host, why
/// that is a fall-back if it is one, and whether restarting the host's daemon
/// would deploy something newer.
pub(super) struct Provisioned {
    pub(super) exe: String,
    pub(super) failure: Option<String>,
    pub(super) upgrade: Option<UpgradeOffer>,
    /// The host's `$HOME`, as the probe reported it — `None` when the probe never
    /// answered. Carried out because the clipboard forward's remote path has to be
    /// absolute (ssh expands nothing in a forward spec) and this is the one round
    /// trip that already asks.
    pub(super) home: Option<String>,
    /// Whether the host is a Mac, from the probe's `uname -sm`. Carried for one
    /// reason: an agent's macOS clipboard path is `osascript`, which never reaches
    /// a shim, so `Ctrl+V` there can never work and the offer needs to say so.
    pub(super) darwin: bool,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// The payload a test dashboard carries: `(target, sha256)`, exactly the
    /// shape `decide_provision` takes.
    const PAYLOAD: (&str, &str) = ("x86_64-unknown-linux-gnu", "abc123");

    const GNU: (&str, &str) = ("x86_64-unknown-linux-gnu", "gnu-sha");
    const MUSL: (&str, &str) = ("x86_64-unknown-linux-musl", "musl-sha");
    const BOTH_TARGETS: &[&str] = &[GNU.0, MUSL.0];

    /// Run the deploy command against a throwaway `$HOME` under a given shell,
    /// feeding it a stand-in binary on stdin — exactly as `ssh` would.
    ///
    /// This is the half of the deploy that exists only as a shell string, so
    /// there is nothing else to type-check it: the staging/verify/publish
    /// ordering and the quoting are only *actually* correct if a shell agrees.
    /// A stand-in executable rather than a real payload, so it runs in every
    /// checkout and on any arch — and needs no embedded server.
    ///
    /// The marker these write is `<digest> <target>`: the target is what makes
    /// the candidate loop terminate, so it has to survive the round trip through
    /// a real shell like the digest does.
    const MARKER_TARGET: &str = "x86_64-unknown-linux-gnu";

    fn run_upload_script(home: &Path, stdin_bytes: &[u8], sha: &str) -> std::process::Output {
        run_deploy("/bin/sh", home, stdin_bytes, sha)
    }

    /// A probe of a host with no running daemon and no protocol announced by
    /// either binary — i.e. servers old enough to fall back to the
    /// exact-version rule, which is what most of these tests are about.
    fn probe(arch: &str, path: Option<&str>, cache: Option<&str>) -> RemoteProbe {
        RemoteProbe {
            home: "/home/u".into(),
            arch: arch.into(),
            path_version: path.map(str::to_string),
            path_protocol: None,
            cache_version: cache.map(str::to_string),
            cache_protocol: None,
            cache_sha: None,
            cache_target: None,
            running: None,
            terminfo: None,
        }
    }

    fn upload(sha: &str) -> Provision {
        Provision::Upload {
            target: PAYLOAD.0.to_string(),
            sha256: sha.to_string(),
        }
    }

    #[test]
    fn parse_probe_extracts_home_arch_versions_and_marker() {
        let out = "/home/u\nLinux x86_64\nmiao-server 0.1.0\n-\nm=-\n";
        let p = parse_probe(out).unwrap();
        assert_eq!(p.home, "/home/u");
        assert_eq!(p.arch, "Linux x86_64");
        assert_eq!(p.path_version.as_deref(), Some("0.1.0"));
        assert_eq!(p.cache_version, None); // the "-" sentinel
        assert_eq!(p.cache_sha, None);
    }

    #[test]
    fn parse_probe_handles_cache_only_and_blank_lines() {
        // PATH binary missing ("-"), cache binary present, marker written.
        let p = parse_probe("/root\nDarwin arm64\n-\nmiao-server 0.2.0\nm=deadbeef\n").unwrap();
        assert_eq!(p.path_version, None);
        assert_eq!(p.cache_version.as_deref(), Some("0.2.0"));
        assert_eq!(p.cache_sha.as_deref(), Some("deadbeef"));
        // A host deployed by an older build (or by redeploy.sh) has no marker.
        let p = parse_probe("/root\nDarwin arm64\n-\nmiao-server 0.2.0").unwrap();
        assert_eq!(p.cache_sha, None);
        // Truncated/garbage output → None rather than a half-built probe.
        assert!(parse_probe("/home/u").is_none());
        assert!(parse_probe("\n\n").is_none());
    }

    #[test]
    fn parse_probe_reads_the_protocol_off_the_version_line() {
        // The server folds its protocol onto the same line as the version
        // precisely so this parse stays one-field-per-line.
        let out = "/home/u\nLinux x86_64\nmiao-server 0.3.0 protocol 4\n-\nm=-\n-\n";
        let p = parse_probe(out).unwrap();
        assert_eq!(p.path_version.as_deref(), Some("0.3.0"));
        assert_eq!(p.path_protocol, Some(4));
        assert_eq!(p.cache_protocol, None);
        assert_eq!(p.running, None);

        // A server too old to announce one still yields its version, which is
        // what the exact-version fallback needs.
        let old = parse_probe("/home/u\nLinux x86_64\nmiao-server 0.2.1\n-\nm=-\n-\n").unwrap();
        assert_eq!(old.path_version.as_deref(), Some("0.2.1"));
        assert_eq!(old.path_protocol, None);

        // Garbage where the number should be is "didn't announce one", never a
        // definite answer we'd then compare against the floor.
        let junk =
            parse_probe("/home/u\nLinux x86_64\nmiao-server 0.3.0 protocol wat\n-\nm=-\n-\n")
                .unwrap();
        assert_eq!(junk.path_protocol, None);
    }

    #[test]
    fn the_probe_survives_a_degenerate_marker_file() {
        // The parse is positional, so what matters is that the marker read emits
        // exactly one line no matter what is in the file — an empty one (a
        // disk-full `echo` wrote nothing), a missing one, one without a trailing
        // newline, one with extra lines, and one holding a glob character, which
        // would otherwise expand against the *remote's* cwd and hand us a
        // directory listing where a digest belongs.
        let root = scratch_home("marker");
        let script = probe_script(None);
        // The marker+daemon tail, run against a throwaway $HOME. Everything
        // before it needs a real host, so slice from the marker read.
        let tail = &script[script.find("m=$(cat").expect("the marker read")..];
        let tail = format!("set -f; {tail}");

        for (name, contents) in [
            ("empty", Some("")),
            ("missing", None),
            ("nonl", Some("abc x86_64-unknown-linux-gnu")),
            ("multi", Some("abc x86_64-unknown-linux-gnu\njunk\n")),
            ("glob", Some("* x86_64-unknown-linux-gnu\n")),
            // `echo` would read a leading -n as its own flag and drop the
            // newline, merging this line into the daemon line below it.
            ("dashn", Some("-n x86_64-unknown-linux-gnu\n")),
        ] {
            let home = root.join(name);
            std::fs::create_dir_all(home.join(".cache/captain-miao/bin")).unwrap();
            if let Some(c) = contents {
                std::fs::write(home.join(REMOTE_MARKER_REL), c).unwrap();
            }
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&tail)
                .env("HOME", &home)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                text.lines().count(),
                3,
                "{name}: marker + daemon + terminfo must be exactly three lines, got {text:?}"
            );
            // The terminfo line is the tail's last, and with no terminal name
            // sent it must still be *emitted* — the parse is positional, so a
            // line that sometimes isn't there would shift every field after it
            // if one is ever added below.
            assert_eq!(text.lines().last(), Some("t=-"), "{name}: {text:?}");
            // A glob must stay literal rather than listing the host's cwd,
            // and the `m=` prefix must survive so the parse can find it.
            if name == "glob" {
                assert!(text.starts_with("m=* "), "{name}: globbed: {text:?}");
            }
            if name == "dashn" {
                assert!(text.starts_with("m=-n "), "{name}: {text:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_probe_reads_the_winning_target_beside_the_digest() {
        let p = parse_probe(
            "/home/u\nLinux x86_64\n-\nmiao-server 0.2.0\nm=dead x86_64-unknown-linux-musl\n-\n",
        )
        .unwrap();
        assert_eq!(p.cache_sha.as_deref(), Some("dead"));
        assert_eq!(p.cache_target.as_deref(), Some("x86_64-unknown-linux-musl"));

        // A marker from a build that recorded only the digest yields no target
        // rather than a guess — which is what case (4) of the sticky rule keys
        // on, and why an upgrade doesn't churn every already-deployed host.
        let old =
            parse_probe("/home/u\nLinux x86_64\n-\nmiao-server 0.2.0\nm=deadbeef\n-\n").unwrap();
        assert_eq!(old.cache_sha.as_deref(), Some("deadbeef"));
        assert_eq!(old.cache_target, None);
    }

    #[test]
    fn parse_probe_reads_which_binary_reported_a_running_daemon() {
        let mk = |d: &str| {
            parse_probe(&format!("/home/u\nLinux x86_64\n-\n-\nm=-\n{d}\n"))
                .unwrap()
                .running
        };
        assert_eq!(mk("path"), Some(RunningDaemon::OnPath));
        assert_eq!(mk("cache"), Some(RunningDaemon::InCache));
        assert_eq!(mk("-"), None);
        // A probe from before this field existed simply has no sixth line.
        assert_eq!(
            parse_probe("/home/u\nLinux x86_64\n-\n-\nm=-\n")
                .unwrap()
                .running,
            None
        );
    }

    /// The terminfo answer rides the probe's last line. Only an explicit `yes`
    /// or `no` counts: everything else — a host with no ncurses tools, a probe
    /// we sent no terminal name in, a daemon from before the field existed —
    /// must read as *unknown*, because a spurious `no` provokes an install and a
    /// spurious `yes` suppresses one that was needed.
    #[test]
    fn parse_probe_reads_whether_the_host_knows_this_terminal() {
        let mk = |t: &str| {
            parse_probe(&format!("/home/u\nLinux x86_64\n-\n-\nm=-\n-\n{t}\n"))
                .unwrap()
                .terminfo
        };
        assert_eq!(mk("t=yes"), Some(true));
        assert_eq!(mk("t=no"), Some(false));
        assert_eq!(mk("t=-"), None);
        assert_eq!(mk("t=surprise"), None);
        // No seventh line at all — an older probe script, or output cut short.
        assert_eq!(
            parse_probe("/home/u\nLinux x86_64\n-\n-\nm=-\n-\n")
                .unwrap()
                .terminfo,
            None
        );
    }

    /// The name is spliced into a shell script wrapped in single quotes, so the
    /// allowlist is load-bearing rather than tidiness: a `TERM` carrying a quote
    /// or a `;` would break the wrapping and run whatever followed it, on every
    /// host this dashboard touches.
    #[test]
    fn only_a_plausible_terminfo_name_is_ever_sent_to_a_host() {
        let name = |t: &str| {
            // SAFETY: single-threaded test, and the value is read back at once.
            unsafe { std::env::set_var("TERM", t) };
            terminfo_to_provision()
        };
        assert_eq!(name("xterm-kitty"), TerminfoName::new("xterm-kitty"));
        assert_eq!(
            name("screen.xterm-256color"),
            TerminfoName::new("screen.xterm-256color")
        );
        // Nothing to ask about: every host has these, and the pool wrapper
        // substitutes for the rest anyway.
        assert_eq!(name("xterm-256color"), None);
        assert_eq!(name("dumb"), None);
        assert_eq!(name(""), None);
        // Injection attempts, in the forms an environment variable can take.
        // The `'` case is the one that matters most: `login_shell_safe` wraps
        // the script in single quotes, so that value doesn't escape a quote —
        // it *closes* one, and `id` runs on the host.
        for hostile in [
            "x; rm -rf ~",
            "x' ; id ; '",
            "$(id)",
            "`id`",
            "a\\b",
            "a b",
            "a\nb",
            "a|b",
            "a&b",
            "a>b",
            "a$b",
            "../../etc/passwd",
            "*",
            &"x".repeat(65),
        ] {
            assert_eq!(name(hostile), None, "accepted {hostile:?}");
        }
        // A leading `-` is the second grammar: legal *inside* a terminfo name,
        // but at the front it makes the value an option to infocmp/tic rather
        // than a terminal. `-V` exits 0, which would read as "the host has it".
        assert_eq!(name("-V"), None);
        assert_eq!(name("-o/tmp/x"), None);
        // …while the same character mid-name is ordinary and must survive.
        assert!(name("rxvt-unicode-256color").is_some());

        // Belt and braces: whatever survives the allowlist must still be inert
        // in the two scripts it reaches, which is what the wrapper depends on.
        for ok in ["xterm-kitty", "screen.xterm-256color", "rxvt-unicode"] {
            let n = TerminfoName::new(ok).expect("a real name");
            for script in [probe_script(Some(&n)), terminfo_install_script(&n)] {
                assert!(!script.contains('\''), "{script}");
                assert!(!script.contains('\\'), "{script}");
            }
        }
    }

    /// The install's success signal shares stdout with the account's **login
    /// shell**, so it is matched as a whole distinctive line. A bare `ok` would
    /// let a `.bashrc` greeting report a success that never happened — and the
    /// symptom would surface much later, as a session mysteriously running in
    /// `xterm-256color`.
    #[test]
    fn a_chatty_login_shell_cannot_fake_a_terminfo_install() {
        assert!(terminfo_took(TIC_OK_MARK));
        // The real shape: an rc greeting, then our marker.
        assert!(terminfo_took(&format!(
            "Welcome to box!\nHave a nice day\n{TIC_OK_MARK}\n"
        )));
        assert!(!terminfo_took(""));
        assert!(!terminfo_took("ok"));
        assert!(!terminfo_took("everything looks ok\n"));
        assert!(!terminfo_took(&format!("almost-{TIC_OK_MARK}\n")));
        assert!(!terminfo_took(&format!("{TIC_OK_MARK}-not-really\n")));
    }

    #[test]
    fn a_running_daemon_outranks_everything_and_deploys_nothing() {
        // Uploading while a live daemon holds the singleton lock is megabytes
        // for nothing: it cannot take effect until that daemon exits, and we
        // never stop one (it *is* the pty pool).
        let mut p = probe("Linux x86_64", None, None);
        p.running = Some(RunningDaemon::OnPath);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UseRunning(RunningDaemon::OnPath)
        );
        assert_eq!(
            remote_exe_for(&Provision::UseRunning(RunningDaemon::OnPath), "/home/u"),
            "miao-server"
        );

        // Which binary answered is what lets us name an exe — the running
        // daemon's own path is not otherwise observable.
        p.running = Some(RunningDaemon::InCache);
        assert_eq!(
            remote_exe_for(
                &decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
                "/home/u"
            ),
            format!("/home/u/{REMOTE_CACHE_REL}")
        );
    }

    /// The upgrade offer is the same ladder asked a different question, and the
    /// cases that must answer "nothing to gain" are the ones that matter: an
    /// offer is a keystroke that kills every session on the host.
    #[test]
    fn an_upgrade_is_offered_only_when_a_restart_would_land_somewhere_else() {
        let running = |p: &RemoteProbe| {
            let mut p = p.clone();
            p.running = Some(RunningDaemon::InCache);
            p
        };

        // Nothing deployed, and we carry something: a restart would deploy it.
        let p = running(&probe("Linux x86_64", None, None));
        assert_eq!(
            upgrade_offer_for("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Some(UpgradeOffer {
                target: PAYLOAD.0.to_string(),
                sha256: PAYLOAD.1.to_string(),
                version: "0.1.0".to_string(),
                running: RunningDaemon::InCache,
                fallbacks: vec![],
            })
        );

        // The host already runs our exact build. Re-deploying identical bytes
        // and killing its sessions to do it is the worst outcome available.
        let mut p = running(&probe("Linux x86_64", None, Some("0.1.0")));
        p.cache_sha = Some(PAYLOAD.1.to_string());
        p.cache_target = Some(PAYLOAD.0.to_string());
        assert_eq!(
            upgrade_offer_for("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            None
        );

        // A *user's* install on PATH wins the ladder and is never overwritten,
        // so stopping the daemon would bring the very same binary back up. The
        // stale-version annotation in the panel is all this host can be told.
        let p = running(&probe("Linux x86_64", Some("0.1.0"), None));
        assert_eq!(
            upgrade_offer_for("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            None
        );

        // A dashboard carrying no payload for this host has nothing to offer,
        // however stale the host is.
        let p = running(&probe("Linux riscv64", None, Some("0.0.9")));
        assert_eq!(upgrade_offer_for("0.1.0", &p, &[], &[]), None);
    }

    #[test]
    fn an_upgrade_carries_the_candidates_it_did_not_choose() {
        // The upgrade is one keystroke, one ssh, one report — there is no loop
        // for it to re-decide in the way the connect path does, so a host that
        // refuses our first choice would simply stay on its old server with the
        // payload that works still sitting here. The alternatives travel with
        // the offer instead.
        let mut p = probe("Linux x86_64", None, None);
        p.running = Some(RunningDaemon::InCache);
        let offer = upgrade_offer_for("0.1.0", &p, &[GNU, MUSL], BOTH_TARGETS).expect("an offer");
        assert_eq!(offer.target, GNU.0);
        assert_eq!(offer.fallbacks, vec![MUSL.0.to_string()]);

        // Whatever was chosen is not also a fallback — including when the marker
        // moved the choice off the head of the list.
        let mut nixos = probe("Linux x86_64", None, Some("0.0.9"));
        nixos.running = Some(RunningDaemon::InCache);
        nixos.cache_target = Some(MUSL.0.into());
        nixos.cache_sha = Some("the-musl-we-deployed".into());
        let offer =
            upgrade_offer_for("0.1.0", &nixos, &[GNU, MUSL], BOTH_TARGETS).expect("an offer");
        assert_eq!(offer.target, MUSL.0);
        assert_eq!(offer.fallbacks, vec![GNU.0.to_string()]);
    }

    #[test]
    fn a_path_server_wins_on_protocol_compatibility_not_version_equality() {
        use cm_core::protocol::{PROTOCOL_MIN, PROTOCOL_VERSION};

        // The self-inflicted deploy this removes: a different version we can
        // still talk to perfectly well used to be refused and overwritten.
        let mut p = probe("Linux x86_64", Some("0.9.9"), None);
        p.path_protocol = Some(PROTOCOL_VERSION);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );

        // Below the floor is not talkable, so it does not win — the ladder
        // falls through to our payload.
        p.path_protocol = Some(PROTOCOL_MIN - 1);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );

        // A server too old to announce a protocol keeps the old exact-version
        // rule: nothing else can be inferred about it.
        p.path_protocol = None;
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        let matching = probe("Linux x86_64", Some("0.1.0"), None);
        assert_eq!(
            decide_provision("0.1.0", &matching, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );
    }

    #[test]
    fn an_incompatible_running_daemon_leads_with_the_remedy_not_the_consequence() {
        let msg = incompatible_daemon_reason("0.1.0", 3);
        // The row flattens and truncates this, so what must survive the cut is
        // the mismatch and the command — the consequence can fall off the end.
        let stop = msg.find("daemon stop").expect("names the remedy");
        let kills = msg.find("kills").expect("states the consequence");
        assert!(msg.find("protocol 3").unwrap() < stop, "{msg}");
        assert!(stop < kills, "{msg}");
        assert!(msg.contains(&PROTOCOL_MIN.to_string()), "{msg}");
        // Never advertised as something we'd do for them: it would kill live
        // pooled sessions, which is why `daemon stop` itself refuses unforced.
        assert!(!msg.contains("automatic"), "{msg}");
    }

    #[test]
    fn decide_prefers_path_install_over_cache() {
        let lx = "Linux x86_64";
        // PATH match wins outright — a user install beats our cache copy, and is
        // never overwritten even when we carry a payload.
        let p = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UsePath
        );
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );
        // No PATH match, but our cache copy matches → use it.
        let p = probe(lx, None, Some("0.1.0"));
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UseCache
        );
    }

    #[test]
    fn decide_falls_back_when_nothing_matches_and_we_carry_nothing() {
        let lx = "Linux x86_64";
        // Nothing deployed anywhere.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), &[], &[]),
            Provision::FallBack
        );
        // Both present but stale — a version mismatch must not be invoked, since
        // the wire protocol isn't guaranteed compatible across versions.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.2.0", &stale, &[], &[]),
            Provision::FallBack
        );
    }

    #[test]
    fn a_payload_turns_every_fallback_into_a_deploy() {
        let lx = "Linux x86_64";
        // Nothing there at all — the fresh-host case.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        // Everything there but stale.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.2.0", &stale, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
    }

    #[test]
    fn a_same_version_cache_binary_is_refreshed_unless_it_is_this_exact_build() {
        // The dev loop: the version never moves between builds, so identity has
        // to come from the digest marker we left beside the binary.
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));

        p.cache_sha = Some("abc123".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UseCache
        );

        // A different build of the same version — re-deploy.
        p.cache_sha = Some("999999".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );

        // No marker at all (redeploy.sh, or a pre-marker dashboard). We own this
        // path, so we take it over rather than trusting an unlabelled binary.
        p.cache_sha = None;
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        // …but a build carrying no payload has nothing better to offer, so it
        // keeps using what's there.
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UseCache
        );
    }

    fn upload_of(p: (&str, &str)) -> Provision {
        Provision::Upload {
            target: p.0.to_string(),
            sha256: p.1.to_string(),
        }
    }

    #[test]
    fn the_marker_makes_the_winning_target_sticky() {
        // The failure this prevents: a NixOS host settles on musl; the next
        // connect compares its marker against our *preferred* gnu payload, sees
        // a mismatch, re-deploys gnu, watches the host refuse it, falls back to
        // musl — and does the whole thing again every reconnect, forever.
        let both = [GNU, MUSL];
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));

        // (2) The marker names musl and we can still supply it; same digest, so
        // it is already this exact build — keep it, even though gnu is what we
        // would otherwise offer first.
        p.cache_sha = Some(MUSL.1.into());
        p.cache_target = Some(MUSL.0.into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            Provision::UseCache
        );

        // (2) Same target, different digest: the dev loop. Re-deploy *that*
        // target rather than restarting the race from the top.
        p.cache_sha = Some("some-older-build".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            upload_of(MUSL)
        );

        // (3) The marker names a target we can no longer supply — a released
        // dashboard whose host runs a downloaded musl, now offline or declined.
        // Keep what proved itself here; re-offering gnu is how the loop starts.
        assert_eq!(
            decide_provision("0.1.0", &p, &[GNU], &[GNU.0]),
            Provision::UseCache
        );

        // (4) A marker written before targets were recorded falls back to the
        // single-candidate rule this had before the loop existed.
        p.cache_target = None;
        p.cache_sha = Some(GNU.1.into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            Provision::UseCache
        );
        p.cache_sha = Some("a-different-build".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            upload_of(GNU)
        );

        // A version mismatch upgrades — but to the target the marker names, not
        // to the one at the head of our preference list. The old binary is
        // stale; the *host's verdict on which target it can execute* is not, and
        // a release does not repeal a fact about the host's loader.
        //
        // This is the polaris case: a NixOS host settled on musl, the dashboard
        // moved 0.3.0 → 0.4.0, and re-running the race from gnu offered it a
        // binary it had already proved it cannot start. On the connect path that
        // costs a wasted 4.8MB upload before the loop recovers; on the upgrade
        // path — one shot — it was the whole outcome.
        let mut old = probe("Linux x86_64", None, Some("0.0.9"));
        old.cache_sha = Some(MUSL.1.into());
        old.cache_target = Some(MUSL.0.into());
        assert_eq!(
            decide_provision("0.1.0", &old, &both, BOTH_TARGETS),
            upload_of(MUSL)
        );

        // Stickiness is not a licence to strand: a marker target we can no
        // longer supply at all still restarts the race rather than keeping a
        // binary of the wrong version.
        assert_eq!(
            decide_provision("0.1.0", &old, &[GNU], &[GNU.0]),
            upload_of(GNU)
        );
    }

    #[test]
    fn a_refusal_does_not_let_the_marker_strand_us_on_a_dead_binary() {
        // Regression. The marker's "we can no longer supply that target" case
        // must not fire for a target that is missing from the candidate list
        // only because the host *just refused it*. Those are opposite
        // situations, and `candidates` alone cannot tell them apart once a
        // refusal has filtered it.
        //
        // The scenario: a no-loader host with a same-version gnu binary already
        // deployed from an earlier build. We offer our gnu payload (the digest
        // differs), the host cannot run it, and gnu drops out of the running.
        // If we then read the marker as "gnu is beyond us", we keep the gnu
        // binary sitting there — which this host equally cannot run — and never
        // try musl, the one payload that would have worked.
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));
        p.cache_sha = Some("an-earlier-gnu-build".into());
        p.cache_target = Some(GNU.0.into());

        // gnu refused, so only musl remains offerable — but both are suppliable.
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], BOTH_TARGETS),
            upload_of(MUSL),
            "a refused marker target must not short-circuit to UseCache"
        );

        // The genuine case (3) still holds: when the marker names something we
        // truly cannot supply, keep what proved itself there.
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], &[MUSL.0]),
            Provision::UseCache
        );
    }

    #[test]
    fn a_spent_candidate_list_still_prefers_the_deployed_binary_over_path() {
        // Regression, and the mirror of the test above: fixing the
        // refused-target case must not cost a healthy host its last resort.
        //
        // A mainstream glibc host with a same-version server already deployed.
        // A rebuild's upload fails transiently — a full disk, an ssh blip, or
        // merely the cooldown from an earlier one. gnu drops out, nothing is
        // left to offer, and the terminal state must be the binary that is
        // sitting there working, not `miao-server` on a PATH that on a
        // deploy-provisioned host is typically empty.
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));
        p.cache_sha = Some("an-earlier-gnu-build".into());
        p.cache_target = Some(GNU.0.into());
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UseCache
        );

        // A host that never got a working server keeps its honest failure: the
        // binary at the cache path cannot execute, so the probe's `--version`
        // yields nothing and there is no cache version to fall back on.
        let mut dead = probe("Linux x86_64", None, None);
        dead.cache_sha = Some("an-earlier-gnu-build".into());
        dead.cache_target = Some(GNU.0.into());
        assert_eq!(
            decide_provision("0.1.0", &dead, &[], BOTH_TARGETS),
            Provision::FallBack
        );

        // …but a no-loader host that *has* been provisioned reaches this arm and
        // should: a musl server deployed there earlier runs fine, which is
        // precisely why it reported a version. Choosing it is right, and it is
        // the running-seconds-ago guard that makes it right — not any argument
        // about which libc the host has.
        let mut nixos = probe("Linux x86_64", None, Some("0.1.0"));
        nixos.cache_sha = Some("the-musl-we-deployed".into());
        nixos.cache_target = Some(MUSL.0.into());
        assert_eq!(
            decide_provision("0.1.0", &nixos, &[], BOTH_TARGETS),
            Provision::UseCache
        );
    }

    #[test]
    fn a_refused_candidate_lets_the_next_one_be_offered() {
        // What the deploy site does after the host's self-check refuses a
        // payload: drop that candidate and ask again. Preference order is only
        // a starting point — the host has the last word, since `uname` cannot
        // report a libc and nothing else can be asked cheaply.
        let p = probe("Linux x86_64", None, None);
        assert_eq!(
            decide_provision("0.1.0", &p, &[GNU, MUSL], BOTH_TARGETS),
            upload_of(GNU)
        );
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], BOTH_TARGETS),
            upload_of(MUSL)
        );
        // Both refused — the NixOS-with-LDAP row. No payload we could ship
        // serves it, so say so rather than install something that breaks later.
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::FallBack
        );
    }

    #[test]
    fn the_release_url_matches_what_the_workflow_publishes() {
        // This copy is duplicated from xtask by decision, so it carries the
        // contract test too — the URL is already a three-way agreement between
        // `release_url`, this fetcher, and build.yml's asset names, and no
        // amount of code sharing between two of them would make it one.
        assert_eq!(
            release_url(RELEASE_BASE, "0.2.1", "aarch64-unknown-linux-musl"),
            "https://github.com/hyperlogue/captain-miao/releases/download/v0.2.1/\
             miao-server-v0.2.1-aarch64-unknown-linux-musl.tar.gz"
        );
        // Either spelling of the version resolves the same, so a `v` prefix
        // picked up from a tag can't produce a second, wrong URL.
        assert_eq!(
            release_url(RELEASE_BASE, "v0.2.1", "x86_64-unknown-linux-gnu"),
            release_url(RELEASE_BASE, "0.2.1", "x86_64-unknown-linux-gnu")
        );
        // A base with a trailing slash is the same base.
        assert_eq!(
            release_url("https://mirror.example/dl/", "0.2.1", "t"),
            release_url("https://mirror.example/dl", "0.2.1", "t")
        );
    }

    /// The download's extraction guards, exercised against archives built to
    /// abuse them. `tar` is the thing under test here, so these run it for real
    /// rather than asserting on the argv.
    #[test]
    fn a_hostile_archive_cannot_write_outside_the_cache_dir() {
        let root = scratch_home("tar");
        let stage = root.join("stage");
        std::fs::create_dir_all(&stage).unwrap();

        let tar_ok = std::process::Command::new("tar")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if !tar_ok {
            return; // no tar here; the deploy tests cover the rest
        }

        // An archive whose only member escapes the extraction directory. We ask
        // for `miao-server` **by name**, so there is nothing for it to land on.
        let evil = root.join("evil");
        std::fs::create_dir_all(evil.join("sub")).unwrap();
        std::fs::write(evil.join("sub/miao-server"), b"payload").unwrap();
        let tgz = root.join("evil.tar.gz");
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&evil)
            .arg("--transform=s|sub/miao-server|../escaped|")
            .arg("sub/miao-server")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let out = std::process::Command::new("tar")
                .arg("-xzf")
                .arg(&tgz)
                .arg("-C")
                .arg(&stage)
                .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
                .output()
                .unwrap();
            // Naming the member is the guard: the escaping entry isn't it, so
            // the extraction finds nothing and nothing is written anywhere.
            assert!(!out.status.success() || !stage.join(SERVER_BIN).exists());
            assert!(!root.join("escaped").exists(), "escaped the staging dir");
        }

        // An archive whose `miao-server` is a *symlink*: tar extracts the link
        // happily, and reading through it would pull in a file we never fetched.
        let link_src = root.join("linky");
        std::fs::create_dir_all(&link_src).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", link_src.join(SERVER_BIN)).unwrap();
        let tgz2 = root.join("link.tar.gz");
        let ok2 = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz2)
            .arg("-C")
            .arg(&link_src)
            .arg(SERVER_BIN)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok2 {
            let stage2 = root.join("stage2");
            std::fs::create_dir_all(&stage2).unwrap();
            let _ = std::process::Command::new("tar")
                .arg("-xzf")
                .arg(&tgz2)
                .arg("-C")
                .arg(&stage2)
                .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
                .output()
                .unwrap();
            let landed = stage2.join(SERVER_BIN);
            // This is exactly what `download_server` refuses on: the check is
            // `symlink_metadata(...).is_file()`, so a link never passes.
            if landed.exists() || landed.is_symlink() {
                let meta = std::fs::symlink_metadata(&landed).unwrap();
                assert!(
                    !meta.is_file(),
                    "a symlink wearing the member name must not read as a regular file"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_gate_remembers_each_payload_independently() {
        // A single remembered failure would be evicted by the next candidate's,
        // leaving the first unsuppressed on the following pass — so a host that
        // refuses *both* payloads gets both re-sent every reconnect, at a
        // backoff that caps at 30s. That is the loop this gate exists to stop.
        let mut gate = UploadGate::default();
        let now = Instant::now();
        gate.record_failure(GNU.1, now, "no loader".into());
        gate.record_failure(MUSL.1, now, "self-check failed".into());

        assert_eq!(gate.suppressed(GNU.1, now), Some("no loader"));
        assert_eq!(gate.suppressed(MUSL.1, now), Some("self-check failed"));
        // A payload we have never sent is not suppressed by either.
        assert_eq!(gate.suppressed("a-fresh-build", now), None);
        // The cooldown still expires, and a working connection still clears it.
        assert_eq!(gate.suppressed(GNU.1, now + UPLOAD_RETRY_COOLDOWN), None);
        gate.clear();
        assert_eq!(gate.suppressed(MUSL.1, now), None);
    }

    #[test]
    fn remote_exe_resolves_cache_path_or_falls_back_to_path() {
        assert_eq!(
            remote_exe_for(&Provision::UsePath, "/home/u"),
            "miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::FallBack, "/home/u"),
            "miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::UseCache, "/root"),
            "/root/.cache/captain-miao/bin/miao-server"
        );
        // An upload lands at the cache path, so it resolves there too.
        assert_eq!(
            remote_exe_for(&upload("abc123"), "/root"),
            "/root/.cache/captain-miao/bin/miao-server"
        );
    }

    #[test]
    fn the_failure_text_says_which_of_the_three_things_went_wrong() {
        let lx = "Linux x86_64";
        let missing = probe(lx, None, None);
        let msg = provision_failure("0.2.0", &missing, &Provision::FallBack, None, &[]).unwrap();
        assert!(msg.contains("not found"), "{msg}");
        // The diagnosis names what the *source chain* could offer, not what is
        // compiled in: with env vars, the cache and the downloader in the chain,
        // "this build carries nothing" would be true of the binary and useless
        // as a diagnosis to someone with CAPTAIN_MIAO_SERVER_DIR set.
        assert!(msg.contains("no server available"), "{msg}");
        // The advice has to be something an installed user can act on — this
        // repo's dev-loop script isn't on their machine.
        assert!(!msg.contains("redeploy.sh"), "{msg}");

        let stale = probe(lx, Some("0.1.0"), None);
        let msg = provision_failure("0.2.0", &stale, &Provision::FallBack, None, &[]).unwrap();
        assert!(msg.contains("version mismatch"), "{msg}");

        // We could offer something and the host took none of it: name what was
        // tried, so "then supply a different one" is an obvious next step.
        let msg = provision_failure(
            "0.2.0",
            &probe("Linux riscv64", None, None),
            &Provision::FallBack,
            None,
            &["x86_64-unknown-linux-gnu"],
        )
        .unwrap();
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("x86_64-unknown-linux-gnu"), "{msg}");

        // A failed deploy outranks both: it's the more actionable sentence.
        let msg = provision_failure(
            "0.2.0",
            &missing,
            &Provision::FallBack,
            Some("disk full"),
            &[],
        )
        .unwrap();
        assert!(msg.contains("could not deploy"), "{msg}");
        assert!(msg.contains("disk full"), "{msg}");

        // Nothing to report when provisioning worked.
        assert!(provision_failure("0.2.0", &missing, &Provision::UseCache, None, &[]).is_none());
        assert!(provision_failure("0.2.0", &missing, &upload("x"), None, &[]).is_none());
    }

    #[test]
    fn a_failed_upload_is_not_retried_until_the_cooldown_or_a_new_payload() {
        let mut gate = UploadGate::default();
        let t0 = Instant::now();
        assert!(gate.suppressed("sha-a", t0).is_none());

        gate.record_failure("sha-a", t0, "read-only $HOME".into());
        // Same payload, still inside the window: reuse the remembered reason
        // rather than re-sending megabytes on every reconnect.
        assert_eq!(
            gate.suppressed("sha-a", t0 + Duration::from_secs(30)),
            Some("read-only $HOME")
        );
        // A *different* payload is a new fact — try immediately.
        assert!(gate.suppressed("sha-b", t0).is_none());
        // Past the cooldown, so is the same one.
        assert!(
            gate.suppressed("sha-a", t0 + UPLOAD_RETRY_COOLDOWN + Duration::from_secs(1))
                .is_none()
        );
        // A working connection wipes the memory outright.
        gate.clear();
        assert!(gate.suppressed("sha-a", t0).is_none());
    }

    /// A *decline* is a decision, not a symptom, and the two are remembered
    /// differently. This is what the terminfo offer rides on: its gate is the
    /// one that is never cleared, so a host that connects perfectly well — the
    /// very host that would otherwise re-ask on every reconnect — stays quiet.
    #[test]
    fn a_declined_offer_is_remembered_without_a_deadline() {
        let mut gate = UploadGate::default();
        let t0 = Instant::now();
        gate.record_refusal("xterm-kitty", "you declined it".into());
        assert_eq!(gate.suppressed("xterm-kitty", t0), Some("you declined it"));
        // Not a cooldown: still refused long past when a failure would retry.
        assert_eq!(
            gate.suppressed("xterm-kitty", t0 + UPLOAD_RETRY_COOLDOWN * 100),
            Some("you declined it")
        );
        // And a decline about one terminal says nothing about another.
        assert!(gate.suppressed("rxvt-unicode", t0).is_none());
    }

    /// Every ambiguous outcome declines. With no UI wired up — a test, a
    /// headless run, a dashboard shutting down — there is nobody to ask, and
    /// the answer must be no rather than "go ahead": both things this channel
    /// gates (fetching a binary from the internet, writing into someone's
    /// remote `$HOME`) are ones a user has to actually say yes to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consent_with_nobody_to_ask_is_refused() {
        // `CONSENT` is a process-wide OnceLock the TUI sets at startup; this
        // test binary never does.
        assert!(!ask_consent("may I?".to_string()).await);
    }

    /// The upgrade's whole safety argument, as an ordering: nothing that ends a
    /// session happens before the host has run the binary and agreed it is ours.
    #[test]
    fn the_upgrade_script_verifies_before_it_stops_anything() {
        let script = upgrade_script("d1g3st", "x86_64-unknown-linux-gnu", RunningDaemon::InCache);
        let stage = script.find("cat > ").unwrap();
        let verify = script.find("self-check").unwrap();
        let version_check = script.find("grep -q").unwrap();
        let stop = script.find("daemon stop --force").unwrap();
        let publish = script.find("mv -f").unwrap();
        let marker = script.find("miao-server.sha256").unwrap();
        assert!(stage < verify, "{script}");
        // The two that matter: a payload the host refuses must cost a transfer
        // and nothing else, so both the run and the version check precede the
        // stop — and `set -e` is what turns "precede" into "gate".
        assert!(verify < stop, "{script}");
        assert!(version_check < stop, "{script}");
        // And the publish follows the stop, so no live daemon ever has its own
        // executable replaced under it.
        assert!(stop < publish, "{script}");
        assert!(publish < marker, "{script}");
        assert!(script.starts_with("set -e;"), "{script}");

        // Which binary stops depends on which one answered the probe; a cache
        // deploy is named `$HOME`-relative because the script is single-quoted
        // whole and a home directory is not ours to make promises about.
        assert!(script.contains("\"$HOME/.cache/captain-miao/bin/miao-server\" daemon stop"));
        let on_path = upgrade_script("d1g3st", "x86_64-unknown-linux-gnu", RunningDaemon::OnPath);
        assert!(
            on_path.contains("miao-server daemon stop --force"),
            "{on_path}"
        );
        assert!(!on_path.contains("$HOME/.cache/captain-miao/bin/miao-server\" daemon stop"));

        // Same `login_shell_safe` constraints as the deploy it shares steps with.
        for s in [&script, &on_path] {
            assert!(!s.contains('\''), "no single quote: {s}");
            assert!(!s.contains('\\'), "no backslash: {s}");
        }
    }

    #[test]
    fn the_upload_script_stages_verifies_then_moves() {
        let script = upload_script("d1g3st", "x86_64-unknown-linux-gnu");
        // Order is the safety property: the binary is only visible at the path
        // the next connect invokes *after* the host itself has run it.
        let stage = script.find("cat > ").unwrap();
        let verify = script.find("self-check").unwrap();
        let version_check = script.find("grep -q").unwrap();
        let publish = script.find("mv -f").unwrap();
        // The version is checked ON THE HOST, before the mv — a check that runs
        // after it has already replaced a working deployment is not a refusal.
        assert!(verify < version_check, "{script}");
        assert!(version_check < publish, "{script}");
        let marker = script.find("miao-server.sha256").unwrap();
        assert!(stage < verify, "{script}");
        assert!(verify < publish, "{script}");
        assert!(publish < marker, "{script}");
        // The temp is cleared before it's written, not after — there is no trap
        // to clean up with (see the doc comment), so the next attempt does it.
        assert!(script.find("rm -f").unwrap() < stage, "{script}");
        // A failure anywhere aborts rather than publishing half a deploy.
        assert!(script.starts_with("set -e;"), "{script}");
        // The digest is what a later probe compares against.
        assert!(script.contains("echo d1g3st"), "{script}");
        // `$HOME` is expanded by the *remote* shell — the client is
        // home-ignorant (§3), so it must never splice its own in.
        assert!(
            script.contains("\"$HOME/.cache/captain-miao/bin\""),
            "{script}"
        );
    }

    #[test]
    fn the_deployed_version_is_read_past_whatever_the_login_shell_printed() {
        assert_eq!(
            reported_version("miao-server 0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        // A `fish_greeting` or an `echo` in .bashrc shares this stdout.
        assert_eq!(
            reported_version("Welcome to box!\n\nmiao-server 0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        assert_eq!(reported_version("Welcome to box!\n"), None);
        assert_eq!(reported_version("miao-server\n"), None);
        assert_eq!(reported_version(""), None);
    }

    #[test]
    fn every_script_we_send_survives_the_wrapping_that_defeats_a_login_shell() {
        // The constraint that makes `/bin/sh -c '<script>'` parse identically in
        // sh, bash, zsh, fish and csh. `login_shell_safe` debug-asserts it too,
        // but only for the scripts a given run happens to build.
        let safe_name = TerminfoName::new("xterm-kitty").expect("a plain name is accepted");
        for script in [
            probe_script(None),
            // A terminfo name is spliced into the probe, so the sanitized form
            // has to survive the same wrapping.
            probe_script(Some(&safe_name)),
            terminfo_install_script(&safe_name),
            upload_script(&"a".repeat(64), "aarch64-unknown-linux-musl"),
        ] {
            let script = script.as_str();
            assert!(!script.contains('\''), "{script}");
            assert!(!script.contains('\\'), "{script}");
        }
        assert_eq!(login_shell_safe("echo hi"), "/bin/sh -c 'echo hi'");
    }

    fn run_deploy(shell: &str, home: &Path, stdin_bytes: &[u8], sha: &str) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new(shell)
            .arg("-c")
            .arg(login_shell_safe(&upload_script(sha, MARKER_TARGET)))
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {shell}: {e}"));
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_bytes)
            .expect("feeding the script");
        child.wait_with_output().expect("waiting for the shell")
    }

    #[test]
    fn the_upload_script_deploys_a_binary_the_host_can_run() {
        let home = scratch_home("ok");
        let version = env!("CARGO_PKG_VERSION");
        // Answers `self-check` *deliberately*, not by ignoring its argv: the
        // deploy's whole verification now hangs on that subcommand existing, so
        // a stand-in that replied to anything would pass this test while a real
        // binary predating `self-check` failed on a host.
        // Mirrors the real `self-check` line exactly — name, version, protocol,
        // user. The trailing fields matter: the script greps for the version
        // followed by a space, which is what stops 0.2.1 matching 0.2.10.
        let fake = format!(
            "#!/bin/sh\ntest \"$1\" = self-check || exit 64\n\
             echo 'miao-server {version} protocol 4 user someone'\n"
        );
        let out = run_upload_script(&home, fake.as_bytes(), "d1g3st");

        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The version the *host* reported is what `upload_server` verifies.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("miao-server {version} protocol 4 user someone")
        );

        let deployed = home.join(REMOTE_CACHE_REL);
        assert_eq!(std::fs::read(&deployed).unwrap(), fake.as_bytes());
        assert_eq!(
            std::fs::metadata(&deployed).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // The marker is what makes the next probe recognise this exact build.
        assert_eq!(
            std::fs::read_to_string(home.join(REMOTE_MARKER_REL))
                .unwrap()
                .trim(),
            format!("d1g3st {MARKER_TARGET}")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_deploy_lands_under_every_login_shell_installed_here() {
        // The bug this pins: `ssh host <cmd>` hands `<cmd>` to the *account's
        // login shell*, so a POSIX-sh script reached a fish user as
        // "fish: Unsupported use of '='" and no host with fish as its shell
        // could ever be provisioned. Whichever of these a machine has, they all
        // have to produce the same deploy.
        let version = env!("CARGO_PKG_VERSION");
        // Answers `self-check` *deliberately*, not by ignoring its argv: the
        // deploy's whole verification now hangs on that subcommand existing, so
        // a stand-in that replied to anything would pass this test while a real
        // binary predating `self-check` failed on a host.
        // Mirrors the real `self-check` line exactly — name, version, protocol,
        // user. The trailing fields matter: the script greps for the version
        // followed by a space, which is what stops 0.2.1 matching 0.2.10.
        let fake = format!(
            "#!/bin/sh\ntest \"$1\" = self-check || exit 64\n\
             echo 'miao-server {version} protocol 4 user someone'\n"
        );
        for shell in ["/bin/sh", "bash", "zsh", "fish", "tcsh"] {
            if std::process::Command::new(shell)
                .arg("-c")
                .arg("exit 0")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                continue; // not installed here
            }
            let home = scratch_home(&format!("shell-{}", shell.replace('/', "_")));
            let out = run_deploy(shell, &home, fake.as_bytes(), "d1g3st");
            assert!(
                out.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                std::fs::read(home.join(REMOTE_CACHE_REL)).unwrap(),
                fake.as_bytes(),
                "{shell} did not deploy the binary"
            );
            assert_eq!(
                std::fs::read_to_string(home.join(REMOTE_MARKER_REL))
                    .unwrap()
                    .trim(),
                format!("d1g3st {MARKER_TARGET}"),
                "{shell} did not write the marker"
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn a_wrong_versioned_binary_never_reaches_the_cache_path() {
        // Regression. The version used to be compared dashboard-side, from the
        // script's output — which is *after* the mv. So a runnable but
        // wrong-versioned payload (an env var pointing at a stale build) got
        // installed over a working deployment and rewrote its marker; the
        // dashboard then "refused" it, and the next probe saw a mismatched cache
        // version and re-uploaded the same stale binary every cooldown, forever.
        let home = scratch_home("wrongver");
        let bin_dir = home.join(".cache/captain-miao/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(home.join(REMOTE_CACHE_REL), b"the working server").unwrap();

        // Self-check passes — it *is* a working miao-server — but of a version
        // we cannot talk to.
        let stale = "#!/bin/sh\ntest \"$1\" = self-check || exit 64\n\
                     echo 'miao-server 0.0.1 protocol 4 user someone'\n";
        let out = run_upload_script(&home, stale.as_bytes(), "d1g3st");

        assert!(
            !out.status.success(),
            "a wrong version must abort the script"
        );
        assert_eq!(
            std::fs::read(home.join(REMOTE_CACHE_REL)).unwrap(),
            b"the working server",
            "the previous deployment must survive"
        );
        assert!(
            !home.join(REMOTE_MARKER_REL).exists(),
            "no marker may be written for a payload that was refused"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_binary_the_host_cannot_run_never_reaches_the_cache_path() {
        // The wrong-ABI / truncated-transfer case, which is the whole reason the
        // script verifies before it publishes: the previous deploy (if any) must
        // survive, and no temp file may be left behind.
        let home = scratch_home("bad");
        let bin_dir = home.join(".cache/captain-miao/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(home.join(REMOTE_CACHE_REL), b"the previous server").unwrap();

        let out = run_upload_script(&home, b"\x7fELF\x00 not runnable here", "d1g3st");
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read(home.join(REMOTE_CACHE_REL)).unwrap(),
            b"the previous server"
        );
        assert!(!home.join(REMOTE_MARKER_REL).exists());
        let leftovers: Vec<_> = std::fs::read_dir(&bin_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "trap left debris: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The whole provisioning path against a **real** ssh host: probe, deploy
    /// the embedded payload, verify it runs there, then confirm a second connect
    /// recognises its own work and doesn't re-send it.
    ///
    /// Ignored by default because it needs a host, and a payload manifest so the
    /// test binary carries a server for that host's arch. It is the one part of
    /// §10.3's end-to-end checklist that
    /// doesn't need a *remote* machine — an sshd on localhost exercises every
    /// line of it — so run it whenever the deploy path changes:
    ///
    /// ```text
    /// # `dist` obtains the server, packs it, and writes the very manifest
    /// # `build.rs` reads — one TSV per variant under the target dir — so there
    /// # is nothing to hand-roll here. (`prepare-servers --out` is the wrong
    /// # tool: it lays out *uncompressed* `<target>/miao-server` for publishing,
    /// # and the manifest's third column wants the packed `.xz`.)
    /// cargo xtask dist --variant bundle-linux-x86_64
    ///
    /// CM_SERVER_PAYLOAD_MANIFEST=target/cm-server-payloads/bundle-linux-x86_64.tsv \
    ///   CM_TEST_SSH_TARGET=127.0.0.1 \
    ///   CM_TEST_SSH_OPTS="-p 2299 -i /tmp/id -o StrictHostKeyChecking=no" \
    ///   cargo test -p captain-miao --features remote -- \
    ///     --ignored provisions_a_real_host
    /// ```
    ///
    /// The manifest is what puts a payload in the test binary; without one there
    /// is nothing to deploy and the test says so.
    ///
    /// It deploys to `~/.cache/captain-miao/bin/` on the target, which is
    /// exactly where a normal connect would put it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a real ssh host: set CM_TEST_SSH_TARGET"]
    async fn provisions_a_real_host_end_to_end() {
        let target = std::env::var("CM_TEST_SSH_TARGET").expect("CM_TEST_SSH_TARGET");
        let ctl = crate::state::ssh_control_path(&target);
        if let Some(dir) = ctl.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut opts = ssh_common_opts(&ctl, &[]);
        if let Ok(extra) = std::env::var("CM_TEST_SSH_OPTS") {
            opts.extend(extra.split_whitespace().map(str::to_string));
        }

        let probe = probe_remote(&target, &opts).await.expect("probe");
        let payload = crate::server_payload::resolve_candidates(&probe.arch)
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "no payload for {:?}; set CM_SERVER_PAYLOAD_MANIFEST (have: {:?})",
                    probe.arch,
                    crate::server_payload::embedded_targets()
                )
            });

        // Start from a clean slate so this really is the fresh-host path.
        let wipe = format!("rm -f \"$HOME/{REMOTE_CACHE_REL}\" \"$HOME/{REMOTE_MARKER_REL}\"");
        assert!(
            Command::new("ssh")
                .args(&opts)
                .arg(&target)
                .arg(&wipe)
                .status()
                .await
                .unwrap()
                .success()
        );
        let fresh = probe_remote(&target, &opts).await.expect("probe");
        assert_eq!(fresh.cache_version, None);
        assert_eq!(
            decide_provision(
                env!("CARGO_PKG_VERSION"),
                &fresh,
                &[(payload.target.as_str(), payload.sha256.as_str())],
                &[payload.target.as_str()],
            ),
            Provision::Upload {
                target: payload.target.clone(),
                sha256: payload.sha256.clone(),
            },
        );

        // First connect: deploys, and resolves to what it deployed.
        let mut gate = UploadGate::default();
        let log = ConnLog::default();
        let host = HostId("test".into());
        let mut dl = UploadGate::default();
        let Provisioned { exe, failure, .. } = resolve_remote_exe(
            &target,
            &opts,
            &mut Provisioning {
                upload: &mut gate,
                download: &mut dl,
                terminfo: &mut UploadGate::default(),
                host: &host,
            },
            &log,
        )
        .await;
        assert_eq!(failure, None, "deploy reported: {failure:?}");
        assert_eq!(exe, format!("{}/{REMOTE_CACHE_REL}", fresh.home));

        // The deployed binary is real: it answers `--version` on the host with
        // our version, and it left the marker that identifies this exact build.
        let after = probe_remote(&target, &opts).await.expect("probe");
        assert_eq!(
            after.cache_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(after.cache_sha.as_deref(), Some(payload.sha256.as_str()));
        // The marker now records which build won, not just its digest.
        assert_eq!(after.cache_target.as_deref(), Some(payload.target.as_str()));

        // Second connect: recognises its own deploy and re-sends nothing.
        assert_eq!(
            decide_provision(
                env!("CARGO_PKG_VERSION"),
                &after,
                &[(payload.target.as_str(), payload.sha256.as_str())],
                &[payload.target.as_str()],
            ),
            Provision::UseCache,
        );
        let Provisioned {
            exe: exe2,
            failure: failure2,
            ..
        } = resolve_remote_exe(
            &target,
            &opts,
            &mut Provisioning {
                upload: &mut gate,
                download: &mut dl,
                terminfo: &mut UploadGate::default(),
                host: &host,
            },
            &log,
        )
        .await;
        assert_eq!(failure2, None);
        assert_eq!(exe2, exe);

        // And the thing we deployed actually is the daemon, not just a binary
        // that parses `--version`.
        let out = Command::new("ssh")
            .args(&opts)
            .arg(&target)
            .arg(format!("\"$HOME/{REMOTE_CACHE_REL}\" daemon status"))
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("daemon"), "daemon status said: {text:?}");
    }
}
