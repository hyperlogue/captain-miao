//! Whole-host CPU and memory utilisation — the two numbers that say whether a
//! host is worth starting another session on, measured by the process that *is*
//! the host (the daemon) and answered on request.
//!
//! Sampled at the host rather than asked for over ssh: a `ssh host uptime` per
//! poll is a process per poll per host, it can't answer for a socket transport
//! (pooled-localhost has no ssh hop at all), and its numbers would be the
//! *link's* view rather than the host's. The daemon already holds the
//! connection, so the sample costs two small file reads.
//!
//! Two properties shape the API:
//!
//! - **CPU utilisation is a difference, not a reading.** The kernel publishes
//!   monotonic busy/idle counters, so a percentage exists only *between* two
//!   readings — hence [`VitalsSampler`] is stateful, its first sample carries no
//!   CPU figure, and the interval between calls is the window the percentage
//!   covers. A caller that samples on demand rather than on a timer therefore
//!   has to say what "now" means, which [`MAX_CPU_WINDOW`] decides: a reading
//!   older than that describes a gap, not the present, and is discarded rather
//!   than averaged across.
//! - **Every field is optional, and absent means absent.** An OS we can't read,
//!   a `/proc` that isn't mounted, a counter that went backwards — all report
//!   `None`. Never `0`, which on a utilisation display reads as a definite
//!   *idle* host, which is precisely the wrong thing to tell someone deciding
//!   where to put work.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One reading of a host's utilisation, as its own daemon measured it.
///
/// Memory crosses as **used/total bytes** rather than a percentage: the ratio
/// is presentation (the dashboard renders it), while the totals are the fact —
/// and they are what a later surface would need to say `9.8/16G` without a
/// protocol change. Additive by construction (`#[serde(default)]` throughout),
/// so an older daemon that omits a field still decodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HostVitals {
    /// Share of the sampling window the host spent busy across all cores,
    /// `0.0..=100.0` — so a saturated 16-core box reads 100, not 1600.
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    /// Memory in use, excluding what the OS would hand back under pressure.
    #[serde(default)]
    pub mem_used_bytes: Option<u64>,
    /// Physical memory the host has.
    #[serde(default)]
    pub mem_total_bytes: Option<u64>,
}

impl HostVitals {
    /// Memory in use as a percentage of the host's total. `None` unless both
    /// halves were sampled.
    pub fn mem_percent(&self) -> Option<f32> {
        let (used, total) = (self.mem_used_bytes?, self.mem_total_bytes?);
        (total > 0).then(|| (used as f64 / total as f64 * 100.0) as f32)
    }

    /// Whether nothing at all was sampled — an unsupported OS, or a host whose
    /// counters are unreadable. Such a host reports absence rather than a
    /// reading of zeros.
    pub fn is_empty(&self) -> bool {
        self.cpu_percent.is_none()
            && self.mem_used_bytes.is_none()
            && self.mem_total_bytes.is_none()
    }
}

/// The busy/total counter pair a percentage is derived from. Units are whatever
/// the platform counts in (jiffies on Linux, mach ticks on macOS) — only their
/// ratio is ever used, so they never need converting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTicks {
    busy: u64,
    total: u64,
}

/// How stale the previous CPU reading may be and still describe *now*.
///
/// Beyond it the percentage would be an average over the gap — a host pegged an
/// hour ago and idle since would read as busy the moment someone looked, which
/// is worse than no number at all. On-demand callers therefore get `None` after
/// a long quiet spell and are expected to take a second reading a beat later
/// (see [`VitalsSampler::has_reading`]).
pub const MAX_CPU_WINDOW: Duration = Duration::from_secs(60);

/// Holds the previous CPU reading, and when it was taken, so the next one can
/// be turned into a percentage. The interval between [`sample`] calls *is* the
/// window each percentage covers — capped by [`MAX_CPU_WINDOW`].
///
/// [`sample`]: VitalsSampler::sample
#[derive(Debug, Default)]
pub struct VitalsSampler {
    prev: Option<(Instant, CpuTicks)>,
}

impl VitalsSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the host's current utilisation. Carries no CPU figure when there is
    /// no usable earlier reading to difference against — the first call, and
    /// any call more than [`MAX_CPU_WINDOW`] after the last one.
    pub fn sample(&mut self) -> HostVitals {
        self.sample_at(Instant::now())
    }

    /// Whether a *usable* previous reading is now held — i.e. whether sampling
    /// again shortly would produce a CPU figure. What tells an on-demand caller
    /// apart from a host with no CPU counters at all, where waiting to re-sample
    /// would buy nothing.
    pub fn has_reading(&self) -> bool {
        self.prev.is_some()
    }

    /// The clock-injected body of [`sample`], so the window rule is testable
    /// without sleeping.
    ///
    /// [`sample`]: VitalsSampler::sample
    fn sample_at(&mut self, now: Instant) -> HostVitals {
        let ticks = read_cpu_ticks();
        let cpu_percent = ticks.and_then(|cur| cpu_since(self.prev, cur, now));
        // Keep the last *successful* reading: a transient failure should cost
        // one sample, not restart the whole differencing. A stale one is
        // replaced here too, which is what makes the very next sample usable.
        if let Some(cur) = ticks {
            self.prev = Some((now, cur));
        }
        let (mem_used_bytes, mem_total_bytes) = read_memory();
        HostVitals {
            cpu_percent,
            mem_used_bytes,
            mem_total_bytes,
        }
    }
}

/// The CPU figure for `cur` given whatever previous reading is held: `None`
/// when there is none, or when it is older than [`MAX_CPU_WINDOW`] and so
/// describes a gap rather than the present. Pure — the whole freshness policy,
/// with no clock and no `/proc` behind it.
fn cpu_since(prev: Option<(Instant, CpuTicks)>, cur: CpuTicks, now: Instant) -> Option<f32> {
    let (at, prev) = prev?;
    if now.duration_since(at) > MAX_CPU_WINDOW {
        return None;
    }
    cpu_percent(prev, cur)
}

/// Busy share of the interval between two readings.
///
/// `None` when the counters didn't advance (two samples inside one tick) or
/// went *backwards* — mach's tick counters are 32-bit and do wrap, and a
/// container's `/proc` can be replaced underneath us. Dropping that one sample
/// is right either way; the alternative is a nonsense spike on the display.
fn cpu_percent(prev: CpuTicks, cur: CpuTicks) -> Option<f32> {
    let total = cur.total.checked_sub(prev.total)?;
    let busy = cur.busy.checked_sub(prev.busy)?;
    (total > 0).then(|| ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32)
}

#[cfg(target_os = "linux")]
fn read_cpu_ticks() -> Option<CpuTicks> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "linux")]
fn read_memory() -> (Option<u64>, Option<u64>) {
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(text) => parse_meminfo(&text),
        Err(_) => (None, None),
    }
}

/// The aggregate `cpu` line of `/proc/stat` — the first one, which sums every
/// core. Pure, so the field arithmetic below is pinned by tests on any host.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(text: &str) -> Option<CpuTicks> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // user nice system idle iowait irq softirq steal. The `guest`/`guest_nice`
    // that follow on modern kernels are deliberately dropped: they are already
    // counted inside user/nice, so summing them charges guest time twice.
    let ticks: Vec<u64> = fields
        .take(8)
        .map(|f| f.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    // Anything short of idle isn't a `/proc/stat` line we understand.
    if ticks.len() < 4 {
        return None;
    }
    let total: u64 = ticks.iter().sum();
    // iowait is idle time too — the CPU is waiting, not working. Counting it as
    // busy makes a box doing nothing but a slow `dd` look pegged.
    let idle = ticks[3] + ticks.get(4).copied().unwrap_or(0);
    Some(CpuTicks {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// `(used, total)` bytes from `/proc/meminfo`.
///
/// Used is derived from **MemAvailable**, not MemFree: Linux spends every spare
/// page on cache and hands it back under pressure, so MemFree alone reports a
/// perfectly healthy box at 95% used. Kernels before 3.14 don't publish it, and
/// the fallback is the approximation `free(1)` used before they did.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let (mut total, mut available, mut free, mut buffers, mut cached) =
        (None, None, None, None, None);
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let bytes = kb.saturating_mul(1024);
        match key {
            "MemTotal" => total = Some(bytes),
            "MemAvailable" => available = Some(bytes),
            "MemFree" => free = Some(bytes),
            "Buffers" => buffers = Some(bytes),
            "Cached" => cached = Some(bytes),
            _ => {}
        }
    }
    let available = available.or_else(|| Some(free? + buffers.unwrap_or(0) + cached.unwrap_or(0)));
    let used = match (total, available) {
        (Some(t), Some(a)) => Some(t.saturating_sub(a)),
        _ => None,
    };
    (used, total)
}

#[cfg(target_os = "macos")]
fn read_cpu_ticks() -> Option<CpuTicks> {
    let mut info = unsafe { std::mem::zeroed::<libc::host_cpu_load_info>() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `host_statistics` fills exactly `count` `integer_t`s, and
    // `HOST_CPU_LOAD_INFO_COUNT` is that struct's size in those units.
    let rc = unsafe {
        #[allow(deprecated)] // libc points at `mach2`; one call doesn't earn a crate.
        let port = libc::mach_host_self();
        libc::host_statistics(
            port,
            libc::HOST_CPU_LOAD_INFO,
            (&raw mut info).cast(),
            &raw mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    let total: u64 = info.cpu_ticks.iter().map(|&t| u64::from(t)).sum();
    let idle = u64::from(info.cpu_ticks[libc::CPU_STATE_IDLE as usize]);
    Some(CpuTicks {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// `(used, total)` bytes on macOS. Used is **active + wired + compressed**,
/// which is what Activity Monitor calls "Memory Used" — inactive pages are
/// reclaimable and the compressor's pages are real occupancy, so either one
/// left out puts the number visibly at odds with the machine's own display.
#[cfg(target_os = "macos")]
fn read_memory() -> (Option<u64>, Option<u64>) {
    let total = sysctl_u64(c"hw.memsize");
    let page = {
        let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (sz > 0).then_some(sz as u64)
    };
    let used = page.and_then(|page| {
        let mut info = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: as above — a `vm_statistics64` sized by its own count const.
        let rc = unsafe {
            #[allow(deprecated)]
            let port = libc::mach_host_self();
            libc::host_statistics64(
                port,
                libc::HOST_VM_INFO64,
                (&raw mut info).cast(),
                &raw mut count,
            )
        };
        if rc != 0 {
            return None;
        }
        let pages = u64::from(info.active_count)
            + u64::from(info.wire_count)
            + u64::from(info.compressor_page_count);
        Some(pages.saturating_mul(page))
    });
    (used, total)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut out: u64 = 0;
    let mut len = size_of::<u64>();
    // SAFETY: `out`/`len` describe a live `u64`; a read-only sysctl passes null
    // for the new value.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut out).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == size_of::<u64>()).then_some(out)
}

/// Anywhere else: no numbers, rather than wrong ones. The dashboard simply
/// shows nothing for such a host (see [`HostVitals::is_empty`]).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_cpu_ticks() -> Option<CpuTicks> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_sums_every_state_and_counts_iowait_as_idle() {
        let ticks = parse_proc_stat("cpu  100 20 30 800 50 5 5 0 7 3\ncpu0 1 2 3 4\n").unwrap();
        // guest/guest_nice (7, 3) are excluded — they double-count user/nice.
        assert_eq!(ticks.total, 100 + 20 + 30 + 800 + 50 + 5 + 5);
        // idle *and* iowait are idle.
        assert_eq!(ticks.busy, ticks.total - 850);
    }

    #[test]
    fn proc_stat_rejects_what_it_cannot_read() {
        assert!(parse_proc_stat("").is_none());
        assert!(parse_proc_stat("intr 1 2 3\n").is_none());
        // A truncated line has no idle field, so there is no busy share to take.
        assert!(parse_proc_stat("cpu 100 20 30\n").is_none());
    }

    #[test]
    fn cpu_percent_is_the_busy_share_of_the_window() {
        let prev = CpuTicks {
            busy: 100,
            total: 1000,
        };
        let cur = CpuTicks {
            busy: 125,
            total: 1100,
        };
        assert_eq!(cpu_percent(prev, cur), Some(25.0));
        // A window with no elapsed ticks, and counters that went backwards
        // (a wrapped 32-bit mach counter), both yield nothing rather than a spike.
        assert_eq!(cpu_percent(prev, prev), None);
        assert_eq!(cpu_percent(cur, prev), None);
    }

    #[test]
    fn meminfo_prefers_available_over_free() {
        let text = "MemTotal:       16000000 kB\n\
                    MemFree:          500000 kB\n\
                    MemAvailable:    8000000 kB\n\
                    Buffers:          100000 kB\n\
                    Cached:          6000000 kB\n\
                    SwapCached:            0 kB\n";
        let (used, total) = parse_meminfo(text);
        assert_eq!(total, Some(16_000_000 * 1024));
        // 16M - 8M available, i.e. *not* the 15.5M that MemFree alone implies.
        assert_eq!(used, Some(8_000_000 * 1024));
        let vitals = HostVitals {
            cpu_percent: None,
            mem_used_bytes: used,
            mem_total_bytes: total,
        };
        assert_eq!(vitals.mem_percent(), Some(50.0));
    }

    #[test]
    fn meminfo_falls_back_when_available_is_absent() {
        // Pre-3.14 kernels: free + buffers + cached stands in for MemAvailable.
        let text = "MemTotal:       16000000 kB\n\
                    MemFree:         1000000 kB\n\
                    Buffers:          500000 kB\n\
                    Cached:          6500000 kB\n";
        let (used, _) = parse_meminfo(text);
        assert_eq!(used, Some(8_000_000 * 1024));
        // Nothing readable at all stays empty rather than reading as 0% used.
        assert_eq!(parse_meminfo("Hugepagesize: 2048 kB\n"), (None, None));
    }

    #[test]
    fn an_unsampled_host_is_empty_not_idle() {
        assert!(HostVitals::default().is_empty());
        assert_eq!(HostVitals::default().mem_percent(), None);
        assert!(
            !HostVitals {
                cpu_percent: Some(0.0),
                ..Default::default()
            }
            .is_empty()
        );
    }

    /// The whole point of the sampler's state: a percentage needs two readings,
    /// so the first sample is honest about not having one yet — and says so in
    /// a way an on-demand caller can act on.
    #[test]
    fn the_first_sample_carries_no_cpu_figure() {
        let mut sampler = VitalsSampler::new();
        assert!(!sampler.has_reading());
        assert_eq!(sampler.sample().cpu_percent, None);
        // Now armed, so a caller knows a second sample would carry a figure.
        assert_eq!(sampler.has_reading(), read_cpu_ticks().is_some());
    }

    /// A reading from before the window is a claim about a *gap*, not about
    /// now, so it is dropped rather than averaged over. This is what makes an
    /// on-demand caller safe: reopening the panel after an hour can't show what
    /// the host was doing an hour ago.
    #[test]
    fn a_reading_older_than_the_window_is_not_averaged_across() {
        let prev = CpuTicks {
            busy: 100,
            total: 1000,
        };
        let cur = CpuTicks {
            busy: 150,
            total: 1100,
        };
        // Instants are built by *adding* to one taken now — subtracting could
        // underflow the monotonic clock on a freshly booted machine.
        let at = Instant::now();
        assert_eq!(
            cpu_since(Some((at, prev)), cur, at + MAX_CPU_WINDOW),
            Some(50.0)
        );
        assert_eq!(
            cpu_since(
                Some((at, prev)),
                cur,
                at + MAX_CPU_WINDOW + Duration::from_secs(1)
            ),
            None
        );
        // No previous reading at all — the first poll.
        assert_eq!(cpu_since(None, cur, at), None);
    }

    /// The stale case still *stores* its reading, so the second sample an
    /// on-demand caller takes a beat later has an anchor to difference against.
    #[test]
    fn a_stale_sample_still_re_arms_the_sampler() {
        let mut sampler = VitalsSampler::new();
        let start = Instant::now();
        sampler.sample_at(start);
        let late = start + MAX_CPU_WINDOW * 2;
        assert_eq!(sampler.sample_at(late).cpu_percent, None);
        assert_eq!(sampler.has_reading(), read_cpu_ticks().is_some());
    }
}
