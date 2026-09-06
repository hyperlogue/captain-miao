//! Client-side window↔session bindings.
//!
//! The dashboard owns the session↔window binding for every session it spawns —
//! local and remote uniformly (§6). Each binding is keyed by
//! `(host, token)`, where the **token** is the session's `pool_session` for a
//! remote pty-pool session (the libshpool join key the local `ssh attach` window
//! names — §8) or its dashboard-minted `launch_id` for a local one. The
//! value is the local window the dashboard opened. When that window dies (laptop
//! slept, ssh dropped, or — locally — the user closed the kitty window) the
//! binding is pruned against the live window set; for a remote session that
//! detaches the row from the dashboard (§5), for a local one it just
//! garbage-collects (the launcher died with its window).
//!
//! Pure data structure: the spawn path calls [`WindowBindings::record`], the
//! reload loop calls [`WindowBindings::prune_dead`] with a `Terminal::snapshot`'s
//! live window ids, and startup seeds it from `window-bindings.json`. See
//! `docs/remote-sessions.md` §6, §8.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::state::HostId;
use crate::terminal::WindowId;

/// Identifies one bound session: its host plus the binding **token** — the pool
/// session name for a remote session (§8) or the `launch_id` for a local one.
/// Only surfaces at the [`WindowBindings::prune_dead`] boundary (the
/// dropped keys); lookups probe the two-level map by reference and never build
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BindingKey {
    pub(crate) host: HostId,
    pub(crate) token: String,
}

/// A bound window, plus when the dashboard opened it.
struct Bound {
    window: WindowId,
    /// Used only by [`WindowBindings::prune_token`], to tell an attach that ran
    /// and then ended from one that died on arrival — see [`RetiredBinding`].
    since: Instant,
}

/// What [`WindowBindings::prune_token`] hands back: the window that was bound,
/// and how long it had been. The caller needs the duration to decide whether the
/// window is worth closing or worth leaving on screen, so it is returned rather
/// than the raw `Instant` — the policy is the dashboard's, the bookkeeping is
/// this module's.
#[derive(Clone)]
pub(crate) struct RetiredBinding {
    pub(crate) window: WindowId,
    pub(crate) held_for: Duration,
}

/// How long a binding retired by [`WindowBindings::prune_dead`] stays
/// recoverable by [`WindowBindings::prune_token`].
///
/// The two detectors race, and the snapshot is the one that can win without
/// knowing anything: it says only "that window is gone", where the report says
/// *how* it went — and `[remote] on_window_close` turns entirely on the 129 only
/// the report carries. Closing the **last** attach window is the case that makes
/// the race deterministic: focus falls back to the dashboard, `FocusGained` arms
/// the prune on the spot, and its `kitten @ ls` beats the wrapper's
/// `miao attach-exited` — so the report found nothing to retire and the session
/// stayed up. Close one of several windows and focus lands on a sibling, no
/// prune is armed, and the report wins; that is the whole of why the bug looked
/// like it depended on how many windows were open.
///
/// Generous on purpose. The report is written milliseconds after the window
/// dies, so anything past a second is already slack for a loaded machine — while
/// on the other side nothing but a real window teardown produces the 129 this
/// gates, and a queued close still waits out
/// [`CLOSE_ON_WINDOW_CLOSE_DELAY`](super::CLOSE_ON_WINDOW_CLOSE_DELAY) with the
/// dashboard alive.
const PRUNE_REPORT_GRACE: Duration = Duration::from_secs(10);

/// `(host, token) → local window` for every session the dashboard has a window
/// for. Nested `host → (token → window)` so `window_for`/`remove` probe by
/// `&HostId` then `&str` without allocating a [`BindingKey`]. Invariant: an
/// inner map is never left empty — `remove`/`prune_dead` drop the host entry
/// when its last token goes, so `is_empty` is just the outer map's emptiness.
#[derive(Default)]
pub(crate) struct WindowBindings {
    by_host: HashMap<HostId, HashMap<String, Bound>>,
    /// Sessions the dashboard **expects** to be holding an attach window for.
    ///
    /// Deliberately outlives the window itself: [`WindowBindings::prune_dead`]
    /// drops the binding when the window dies but leaves this set alone, so
    /// after a laptop sleep or a broken pipe the dashboard still knows *which*
    /// sessions the user had open and can reattach them all on reconnect (§7).
    /// A deliberate `D` detach clears it — that's the whole distinction between
    /// "you detached" and "the link dropped".
    expected: HashSet<BindingKey>,
    /// Bindings [`WindowBindings::prune_dead`] retired in the last
    /// [`PRUNE_REPORT_GRACE`], so a detach report that lost the race to the
    /// snapshot still finds what it came to retire. Kept out of `by_host`
    /// deliberately: for every other purpose — the detached tier, `Enter`,
    /// auto-reattach — the binding really is gone the moment the snapshot says
    /// the window is.
    recently_pruned: HashMap<BindingKey, (RetiredBinding, Instant)>,
}

impl WindowBindings {
    /// Record (or replace) the local window bound to a session's token, and
    /// remember that this session is expected to stay attached.
    pub(crate) fn record(&mut self, host: HostId, token: String, window: WindowId) {
        let key = BindingKey {
            host: host.clone(),
            token: token.clone(),
        };
        // A fresh window for this session settles the race the grace map exists
        // for: whatever the old binding was, a report naming it can no longer
        // mean "end the session behind this row".
        self.recently_pruned.remove(&key);
        self.expected.insert(key);
        self.by_host.entry(host).or_default().insert(
            token,
            Bound {
                window,
                since: Instant::now(),
            },
        );
    }

    /// Drop the binding for `(host, token)` if present, returning the window it
    /// pointed at. Used by an explicit **detach**: the dashboard closes the local
    /// `ssh attach` window and forgets it, while the remote pool session keeps
    /// running (so the row stays and `Enter` re-attaches). Distinct from
    /// [`WindowBindings::prune_dead`], which reacts to a window that *already*
    /// died; this initiates the teardown — and being deliberate, it also clears
    /// the expected-attached memory so auto-reattach leaves it detached.
    pub(crate) fn remove(&mut self, host: &HostId, token: &str) -> Option<WindowId> {
        let key = BindingKey {
            host: host.clone(),
            token: token.to_string(),
        };
        self.expected.remove(&key);
        // And it forgets any pruned remnant, which is what keeps `D` from
        // killing: `D` retires *then* closes the window, so the 129 that follows
        // must find nothing — including in the grace map, in case a snapshot
        // prune had already retired this same binding moments earlier.
        self.recently_pruned.remove(&key);
        let inner = self.by_host.get_mut(host)?;
        let removed = inner.remove(token);
        if inner.is_empty() {
            self.by_host.remove(host);
        }
        removed.map(|b| b.window)
    }

    /// Retire the binding for `(host, token)` because its window is gone,
    /// returning the window it pointed at.
    ///
    /// The single-key form of [`WindowBindings::prune_dead`], and it keeps that
    /// one's semantics rather than [`WindowBindings::remove`]'s: the
    /// expected-attached memory **survives**. The caller is a detach report from
    /// a window that ended, which is "the link dropped", not "you detached" — a
    /// closed window and a killed ssh are indistinguishable from here, and both
    /// should come back when the host reconnects. Only `D` retires the
    /// expectation.
    /// A binding [`WindowBindings::prune_dead`] retired within
    /// [`PRUNE_REPORT_GRACE`] is still answered here, once. The report is the
    /// only witness to *how* the window went, and dropping it because the
    /// snapshot got there first is what left a closed window's session running.
    pub(crate) fn prune_token(&mut self, host: &HostId, token: &str) -> Option<RetiredBinding> {
        if let Some(inner) = self.by_host.get_mut(host)
            && let Some(b) = inner.remove(token)
        {
            if inner.is_empty() {
                self.by_host.remove(host);
            }
            return Some(RetiredBinding {
                window: b.window,
                held_for: b.since.elapsed(),
            });
        }
        let key = BindingKey {
            host: host.clone(),
            token: token.to_string(),
        };
        let (retired, at) = self.recently_pruned.remove(&key)?;
        (at.elapsed() < PRUNE_REPORT_GRACE).then_some(retired)
    }

    /// Tokens on `host` the dashboard expects to be attached to but currently
    /// holds no window for — the auto-reattach work list after a reconnect (§7).
    pub(crate) fn expected_without_window(&self, host: &HostId) -> Vec<String> {
        self.expected
            .iter()
            .filter(|k| &k.host == host)
            .filter(|k| self.window_for(&k.host, &k.token).is_none())
            .map(|k| k.token.clone())
            .collect()
    }

    /// Forget every expectation for sessions no longer present on their host, so
    /// a killed session isn't reattached forever. `live` is the set of
    /// `(host, token)` pairs the current rows carry.
    pub(crate) fn retain_expected(&mut self, live: &HashSet<BindingKey>) {
        self.expected.retain(|k| live.contains(k));
    }

    /// The local window bound to this session's token, if any.
    pub(crate) fn window_for(&self, host: &HostId, token: &str) -> Option<&WindowId> {
        self.by_host.get(host)?.get(token).map(|b| &b.window)
    }

    /// Drop bindings whose window is no longer in `live` (the windows a
    /// `Terminal::snapshot` currently shows). Returns the dropped keys — the
    /// remote sessions that just detached and should leave the dashboard.
    pub(crate) fn prune_dead(&mut self, live: &HashSet<WindowId>) -> Vec<BindingKey> {
        let dead: Vec<BindingKey> =
            self.by_host
                .iter()
                .flat_map(|(host, inner)| {
                    inner.iter().filter(|(_, b)| !live.contains(&b.window)).map(
                        move |(token, _)| BindingKey {
                            host: host.clone(),
                            token: token.clone(),
                        },
                    )
                })
                .collect();
        // Anything already past the grace is dead weight; sweeping on the one
        // path that inserts keeps the map bounded without a timer of its own.
        let now = Instant::now();
        self.recently_pruned
            .retain(|_, (_, at)| now.duration_since(*at) < PRUNE_REPORT_GRACE);
        for k in &dead {
            if let Some(inner) = self.by_host.get_mut(&k.host) {
                if let Some(b) = inner.remove(&k.token) {
                    self.recently_pruned.insert(
                        k.clone(),
                        (
                            RetiredBinding {
                                window: b.window,
                                held_for: b.since.elapsed(),
                            },
                            now,
                        ),
                    );
                }
                if inner.is_empty() {
                    self.by_host.remove(&k.host);
                }
            }
        }
        dead
    }

    /// Whether the dashboard holds no window bindings at all (local or remote).
    pub(crate) fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    /// Whether any binding is for a *remote* host — the gate for the reload
    /// loop's detach-prune snapshot. Local `launch_id` bindings (every
    /// dashboard-spawned local session has one) GC via their own state file, so
    /// they must not force a terminal snapshot; only a live remote attachment
    /// needs one to notice its window died.
    pub(crate) fn has_remote(&self) -> bool {
        self.by_host.keys().any(|h| !h.is_local())
    }

    /// Number of bound sessions. Test-only — no production caller today, so it's
    /// gated to test builds rather than carrying an `allow(dead_code)`.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_host.values().map(HashMap::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(s: &str) -> HostId {
        HostId(s.to_string())
    }
    fn win(s: &str) -> WindowId {
        WindowId(s.to_string())
    }

    #[test]
    fn record_then_lookup_is_host_qualified() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "sess".into(), win("w1"));
        b.record(host("h2"), "sess".into(), win("w2"));
        // Same session name on different hosts must not collide.
        assert_eq!(b.window_for(&host("h1"), "sess"), Some(&win("w1")));
        assert_eq!(b.window_for(&host("h2"), "sess"), Some(&win("w2")));
        assert_eq!(b.window_for(&host("h1"), "other"), None);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn remove_drops_only_the_matching_binding() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "sess".into(), win("w1"));
        b.record(host("h2"), "sess".into(), win("w2"));
        // Removing one host's binding returns its window and leaves the other.
        assert_eq!(b.remove(&host("h1"), "sess"), Some(win("w1")));
        assert_eq!(b.window_for(&host("h1"), "sess"), None);
        assert_eq!(b.window_for(&host("h2"), "sess"), Some(&win("w2")));
        // Removing an absent binding is a no-op returning None.
        assert_eq!(b.remove(&host("h1"), "sess"), None);
    }

    #[test]
    fn record_replaces_window_for_same_session() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "sess".into(), win("w1"));
        b.record(host("h1"), "sess".into(), win("w2"));
        assert_eq!(b.window_for(&host("h1"), "sess"), Some(&win("w2")));
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn prune_dead_drops_and_returns_unbacked_windows() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "live".into(), win("w1"));
        b.record(host("h1"), "dead".into(), win("w2"));
        b.record(host("h2"), "also-dead".into(), win("w3"));

        let live: HashSet<WindowId> = [win("w1")].into_iter().collect();
        let mut dropped = b.prune_dead(&live);
        dropped.sort_by(|a, c| a.token.cmp(&c.token));

        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[0].token, "also-dead");
        assert_eq!(dropped[1].token, "dead");
        // The live binding survives; the dead ones are gone.
        assert_eq!(b.window_for(&host("h1"), "live"), Some(&win("w1")));
        assert_eq!(b.window_for(&host("h1"), "dead"), None);
        assert!(!b.is_empty() && b.len() == 1);
    }

    /// A binding the snapshot prune retired is still answerable by the report
    /// that follows — once, and never after the deliberate paths have spoken.
    #[test]
    fn a_pruned_binding_answers_one_late_report() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "a".into(), win("w1"));
        b.prune_dead(&HashSet::new());

        let recovered = b.prune_token(&host("h1"), "a").expect("the late report");
        assert_eq!(recovered.window, win("w1"));
        assert!(
            b.prune_token(&host("h1"), "a").is_none(),
            "and only once — a second report has nothing left to retire"
        );

        // `D` is the ordering the close policy leans on: retire, then close the
        // window. A prune that already retired the binding must not leave a
        // remnant for the 129 that follows.
        b.record(host("h1"), "b".into(), win("w2"));
        b.prune_dead(&HashSet::new());
        b.remove(&host("h1"), "b");
        assert!(
            b.prune_token(&host("h1"), "b").is_none(),
            "an explicit detach forgets the pruned remnant too"
        );

        // So does re-attaching. Auto-reattach binds a fresh window for a token
        // the prune just retired, and the live binding must be the only thing
        // left to answer for it — a remnant behind it would retire the *new*
        // window on a report about the old one.
        b.record(host("h1"), "c".into(), win("w3"));
        b.prune_dead(&HashSet::new());
        b.record(host("h1"), "c".into(), win("w4"));
        assert_eq!(
            b.prune_token(&host("h1"), "c").map(|r| r.window),
            Some(win("w4")),
            "the live binding answers first"
        );
        assert!(
            b.prune_token(&host("h1"), "c").is_none(),
            "and the re-attach had already cleared the earlier prune's remnant"
        );
    }

    /// The expected-attached memory is what makes auto-reattach possible: a
    /// window dying (sleep / dropped ssh) must NOT be read as "the user wants
    /// this detached", while pressing `D` must.
    #[test]
    fn expectation_survives_a_dead_window_but_not_an_explicit_detach() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "a".into(), win("w1"));
        b.record(host("h1"), "b".into(), win("w2"));
        assert!(b.expected_without_window(&host("h1")).is_empty());

        // Both windows died with the link — both are still expected, so both
        // come back on reconnect.
        b.prune_dead(&HashSet::new());
        let mut pending = b.expected_without_window(&host("h1"));
        pending.sort();
        assert_eq!(pending, vec!["a".to_string(), "b".to_string()]);

        // A deliberate detach of `a` retires the expectation for good.
        b.record(host("h1"), "a".into(), win("w3"));
        b.remove(&host("h1"), "a");
        assert_eq!(
            b.expected_without_window(&host("h1")),
            vec!["b".to_string()]
        );

        // A session that's gone from the host stops being expected at all.
        b.retain_expected(&HashSet::new());
        assert!(b.expected_without_window(&host("h1")).is_empty());
    }

    #[test]
    fn has_remote_only_when_a_non_local_host_is_bound() {
        let mut b = WindowBindings::default();
        // A local `launch_id` binding must not read as a remote attachment —
        // otherwise the reload loop would snapshot the terminal every reload.
        b.record(HostId::local(), "L1-1".into(), win("w1"));
        assert!(!b.has_remote());
        b.record(host("remote"), "pool-a".into(), win("w2"));
        assert!(b.has_remote());
        // Dropping the remote binding clears has_remote (its host entry is
        // removed since it was the last token there).
        b.remove(&host("remote"), "pool-a");
        assert!(!b.has_remote());
        assert!(!b.is_empty());
    }

    #[test]
    fn prune_with_all_live_drops_nothing() {
        let mut b = WindowBindings::default();
        b.record(host("h1"), "a".into(), win("w1"));
        b.record(host("h1"), "b".into(), win("w2"));
        let live: HashSet<WindowId> = [win("w1"), win("w2")].into_iter().collect();
        assert!(b.prune_dead(&live).is_empty());
        assert_eq!(b.len(), 2);
    }
}
