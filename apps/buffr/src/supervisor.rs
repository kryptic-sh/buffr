//! Platform-independent supervisor logic.
//!
//! The Unix and Windows supervisor loops used to be near-verbatim copies of
//! each other, differing only in "how do I tell whether the child is alive,
//! how did it die, and how do I kill it". That duplication is why several
//! correctness fixes only ever landed on one side.
//!
//! Everything that does not need a platform primitive lives here and is
//! generic over [`ChildHandle`]:
//!
//! - [`wait_for_connect`] — grace window for the child's heartbeat connect.
//! - [`watch_heartbeat`] — the ping-deadline watchdog.
//! - [`CrashWindow`] — the rolling crash/hang backoff window.
//! - [`classify`] — clean exit vs. propagate vs. restart.
//! - [`quote_arg`] / [`build_command_line`] — Windows command-line quoting
//!   (kept here, and not behind `cfg(windows)`, so it is unit-testable on
//!   every host).

use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Events the heartbeat listener thread sends back to the main loop.
pub enum HeartbeatEvent {
    /// Child successfully connected.
    Connected,
    /// A ping byte arrived from the child.
    Ping,
    /// The connection was closed (EOF or error).
    Disconnected,
}

/// How the child went away, normalised across platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code for a normal exit. `None` on Unix means "killed by a
    /// signal"; on Windows the OS always reports a code, so `crashed`
    /// carries that distinction instead.
    pub code: Option<i32>,
    /// The child died abnormally: killed by a signal (Unix) or terminated
    /// by an unhandled exception (Windows NTSTATUS exit code).
    pub crashed: bool,
}

/// The platform-specific half of the supervisor: one live child process.
pub trait ChildHandle {
    /// OS process id, for logging.
    fn pid(&self) -> u32;

    /// Non-blocking check. `Some` once the child has exited; implementations
    /// must memoise so repeated calls after exit keep returning the same
    /// value (a child may only be reaped once).
    fn poll_exit(&mut self) -> Option<ExitInfo>;

    /// Block until the child exits and reap it.
    fn wait_exit(&mut self) -> ExitInfo;

    /// Kill the child (and, where the platform supports it, its whole
    /// process tree) and reap it.
    fn kill_and_reap(&mut self) -> ExitInfo;
}

/// Outcome of the connect grace window.
pub enum ConnectResult {
    /// Child successfully connected to the heartbeat transport.
    Connected,
    /// Grace window elapsed with no connection.
    TimedOut,
    /// Child exited before connecting (may be a clean exit).
    ChildExited(ExitInfo),
}

/// Outcome of the heartbeat watch.
pub enum WatchOutcome {
    /// No pings within the deadline (or a wedged child that closed the
    /// transport without exiting). The child has been killed and reaped.
    Hang,
    /// The child exited on its own.
    Exited(ExitInfo),
}

/// After the heartbeat transport closes, how long to wait for the child to
/// actually exit before concluding it is wedged.
///
/// A child whose UI thread has frozen may drop its end of the socket while
/// the process itself keeps running. Treating that as "the child exited"
/// makes the supervisor block forever in `wait()`, which is precisely the
/// bug the hang watchdog exists to prevent.
const DISCONNECT_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Poll interval while waiting out [`DISCONNECT_EXIT_GRACE`].
const DISCONNECT_POLL: Duration = Duration::from_millis(50);

/// Wait up to `grace` for the child to connect to the heartbeat transport.
///
/// Also polls the child so a fast-exiting child (clean exit, short
/// subcommand, `--help`) is not misclassified as a hang.
pub fn wait_for_connect<C: ChildHandle>(
    rx: &Receiver<HeartbeatEvent>,
    child: &mut C,
    grace: Duration,
) -> ConnectResult {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(info) = child.poll_exit() {
            return ConnectResult::ChildExited(info);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ConnectResult::TimedOut;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(HeartbeatEvent::Connected) => return ConnectResult::Connected,
            Ok(_) => continue, // ping before Connected — shouldn't happen but fine
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    return ConnectResult::TimedOut;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return ConnectResult::TimedOut,
        }
    }
}

/// Watch heartbeat pings; kill the child on hang.
///
/// Returns [`WatchOutcome::Hang`] when we killed the child, or
/// [`WatchOutcome::Exited`] when it went away on its own — in which case it
/// has already been reaped, so the caller never blocks on it.
pub fn watch_heartbeat<C: ChildHandle>(
    rx: &Receiver<HeartbeatEvent>,
    child: &mut C,
    first_deadline: Instant,
    timeout: Duration,
) -> WatchOutcome {
    let mut last_ping = Instant::now();
    let mut deadline = first_deadline;

    loop {
        if let Some(info) = child.poll_exit() {
            return WatchOutcome::Exited(info);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::error!(
                pid = child.pid(),
                "watchdog: ui hang detected (no heartbeat for {}s); killing child",
                timeout.as_secs()
            );
            child.kill_and_reap();
            return WatchOutcome::Hang;
        }

        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(HeartbeatEvent::Ping) => {
                let now = Instant::now();
                tracing::debug!(
                    lag_ms = now.duration_since(last_ping).as_millis(),
                    "heartbeat: ping received"
                );
                last_ping = now;
                deadline = now + timeout;
            }
            Ok(HeartbeatEvent::Connected) => {
                // Shouldn't arrive here (already connected) but reset.
                last_ping = Instant::now();
                deadline = last_ping + timeout;
            }
            Ok(HeartbeatEvent::Disconnected) => {
                tracing::warn!(pid = child.pid(), "heartbeat: child closed the transport");
                return after_disconnect(child);
            }
            Err(RecvTimeoutError::Timeout) => {
                // No event in this slice — loop back and check deadline.
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!(pid = child.pid(), "heartbeat: listener thread gone");
                return after_disconnect(child);
            }
        }
    }
}

/// The heartbeat transport closed. Give the child a bounded window to
/// actually exit; if it is still running after that it is wedged with a
/// dead socket, so kill it and report a hang.
fn after_disconnect<C: ChildHandle>(child: &mut C) -> WatchOutcome {
    let deadline = Instant::now() + DISCONNECT_EXIT_GRACE;
    loop {
        if let Some(info) = child.poll_exit() {
            return WatchOutcome::Exited(info);
        }
        if Instant::now() >= deadline {
            tracing::error!(
                pid = child.pid(),
                "watchdog: child closed the heartbeat transport but is still running \
                 after {}s; treating as a hang and killing it",
                DISCONNECT_EXIT_GRACE.as_secs()
            );
            child.kill_and_reap();
            return WatchOutcome::Hang;
        }
        std::thread::sleep(DISCONNECT_POLL);
    }
}

/// What the supervisor should do once the child is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Clean shutdown — the supervisor exits 0.
    Done,
    /// A normal non-zero exit (CLI parse error, config validation failure,
    /// panic → 101). Respawning would just re-run the same failure, so
    /// propagate the code instead.
    Propagate(i32),
    /// Crash or hang — restart, subject to the backoff window.
    Restart,
}

/// Decide what to do about a child that has gone away.
///
/// "Clean" means either exit code 0 with no hang, or the child touched the
/// clean-shutdown flag before exiting (which covers a segfault during CEF /
/// wgpu teardown after the user explicitly closed the window).
pub fn classify(hang_detected: bool, exit: Option<&ExitInfo>, clean_flag: bool) -> Disposition {
    let code = exit.and_then(|e| e.code);
    if clean_flag || (!hang_detected && code == Some(0)) {
        return Disposition::Done;
    }
    if !hang_detected
        && let Some(info) = exit
        && !info.crashed
        && let Some(c) = info.code
        && c != 0
    {
        return Disposition::Propagate(c);
    }
    Disposition::Restart
}

/// Rolling window of recent crashes/hangs used for restart backoff.
pub struct CrashWindow {
    times: Vec<Instant>,
    window: Duration,
    limit: usize,
}

impl CrashWindow {
    pub fn new(window: Duration, limit: usize) -> Self {
        Self {
            times: Vec::new(),
            window,
            limit,
        }
    }

    /// Record a crash at `now`, evict everything older than the window, and
    /// return how many crashes remain inside it.
    pub fn record(&mut self, now: Instant) -> usize {
        self.times.push(now);
        // `Instant - Duration` PANICS ("overflow when subtracting duration
        // from instant") when `now` is less than `window` past the monotonic
        // epoch. On Linux that epoch is boot, so a browser autostarted at
        // login that crashes in the first 30 s would take the supervisor down
        // with a panic instead of restarting. `None` means nothing recorded
        // so far can possibly be outside the window — retain everything.
        if let Some(window_start) = now.checked_sub(self.window) {
            self.times.retain(|t| *t >= window_start);
        }
        self.times.len()
    }

    pub fn limit_reached(&self) -> bool {
        self.times.len() >= self.limit
    }
}

// ── Windows command-line quoting ─────────────────────────────────────────────

/// Quote one argument per the `CommandLineToArgvW` / MSVCRT rules.
///
/// Without this, `buffr "C:\My Docs\page.html"` reaches the child as two
/// arguments and an argument containing `"` can inject extra flags into the
/// child's command line.
///
/// Rules: wrap in `"` when the argument is empty or contains whitespace or a
/// quote; double every backslash that immediately precedes a `"` (including
/// the closing one) and escape the `"` itself.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty() || arg.contains([' ', '\t', '\n', '\u{0b}', '"']);
    if !needs_quoting {
        return arg.to_owned();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are escaped, then the quote.
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes would otherwise escape the closing quote.
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Build the full `lpCommandLine` for `CreateProcessW`: quoted binary path
/// followed by each quoted argument.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn build_command_line(bin: &Path, args: &[OsString]) -> String {
    let mut cmdline = quote_arg(&bin.to_string_lossy());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote_arg(&a.to_string_lossy()));
    }
    cmdline
}

/// Does this Windows exit code indicate an abnormal termination?
///
/// Windows always reports *some* exit code, so there is no direct analogue
/// of Unix's "killed by a signal". Unhandled exceptions surface as the
/// NTSTATUS value with the severity bits set (`0x8xxxxxxx` warning,
/// `0xCxxxxxxx` error) — e.g. `0xC0000005` STATUS_ACCESS_VIOLATION or
/// `0xC0000409` STATUS_STACK_BUFFER_OVERRUN. Ordinary program exits use
/// small values, which we treat as deliberate.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn is_crash_exit_code(code: u32) -> bool {
    code >= 0x8000_0000
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CrashWindow / M5 ─────────────────────────────────────────────────────

    #[test]
    fn crash_window_counts_recent_entries() {
        let mut w = CrashWindow::new(Duration::from_secs(30), 3);
        let now = Instant::now();
        assert_eq!(w.record(now), 1);
        assert!(!w.limit_reached());
        assert_eq!(w.record(now), 2);
        assert_eq!(w.record(now), 3);
        assert!(w.limit_reached());
    }

    #[test]
    fn crash_window_evicts_old_entries() {
        let mut w = CrashWindow::new(Duration::from_secs(30), 3);
        let now = Instant::now();
        // Two crashes long before the window opened. `checked_sub` because
        // the process may have been up for less than 120 s.
        let (Some(old), Some(older)) = (
            now.checked_sub(Duration::from_secs(60)),
            now.checked_sub(Duration::from_secs(120)),
        ) else {
            // Uptime below 2 min — nothing to evict, skip.
            return;
        };
        w.times.push(older);
        w.times.push(old);
        assert_eq!(w.record(now), 1, "old entries must be evicted");
        assert!(!w.limit_reached());
    }

    /// M5 regression: `Instant - Duration` panics when the instant is less
    /// than the duration past the monotonic epoch (boot, on Linux). A
    /// supervisor that crashes in the first 30 s of uptime must restart, not
    /// abort with "overflow when subtracting duration from instant".
    #[test]
    fn crash_window_survives_window_longer_than_uptime() {
        // A window far larger than any conceivable uptime forces
        // `checked_sub` to return `None` on every platform.
        let mut w = CrashWindow::new(Duration::from_secs(u64::MAX / 2), 3);
        let now = Instant::now();
        assert_eq!(w.record(now), 1);
        assert_eq!(w.record(Instant::now()), 2);
        assert_eq!(w.record(Instant::now()), 3);
        assert!(w.limit_reached());
    }

    // ── classify / M3 ────────────────────────────────────────────────────────

    fn normal(code: i32) -> ExitInfo {
        ExitInfo {
            code: Some(code),
            crashed: false,
        }
    }

    fn signalled() -> ExitInfo {
        ExitInfo {
            code: None,
            crashed: true,
        }
    }

    #[test]
    fn classify_exit_zero_is_done() {
        assert_eq!(classify(false, Some(&normal(0)), false), Disposition::Done);
    }

    #[test]
    fn classify_clean_flag_wins_over_a_crash() {
        // Segfault during CEF teardown after the user closed the window.
        assert_eq!(classify(false, Some(&signalled()), true), Disposition::Done);
        assert_eq!(classify(false, Some(&normal(139)), true), Disposition::Done);
    }

    #[test]
    fn classify_normal_nonzero_exit_propagates() {
        // `buffr --bogus-flag`: re-running it three times just repeats the
        // failure and reports a misleading "3 crashes/hangs".
        assert_eq!(
            classify(false, Some(&normal(2)), false),
            Disposition::Propagate(2)
        );
        assert_eq!(
            classify(false, Some(&normal(101)), false),
            Disposition::Propagate(101)
        );
    }

    #[test]
    fn classify_abnormal_death_restarts() {
        assert_eq!(
            classify(false, Some(&signalled()), false),
            Disposition::Restart
        );
        // Windows: NTSTATUS access violation reported as an exit code.
        let av = ExitInfo {
            code: Some(0xC000_0005u32 as i32),
            crashed: true,
        };
        assert_eq!(classify(false, Some(&av), false), Disposition::Restart);
    }

    #[test]
    fn classify_hang_always_restarts() {
        assert_eq!(classify(true, None, false), Disposition::Restart);
        // Even a zero exit code recorded alongside a hang is a restart.
        assert_eq!(
            classify(true, Some(&normal(0)), false),
            Disposition::Restart
        );
    }

    // ── is_crash_exit_code ───────────────────────────────────────────────────

    #[test]
    fn crash_exit_codes_are_ntstatus_severities() {
        assert!(is_crash_exit_code(0xC000_0005)); // STATUS_ACCESS_VIOLATION
        assert!(is_crash_exit_code(0xC000_0409)); // STATUS_STACK_BUFFER_OVERRUN
        assert!(is_crash_exit_code(0x8000_0003)); // STATUS_BREAKPOINT
        assert!(!is_crash_exit_code(0));
        assert!(!is_crash_exit_code(1));
        assert!(!is_crash_exit_code(101));
        assert!(!is_crash_exit_code(259)); // STILL_ACTIVE, if it ever leaks
    }

    // ── quote_arg / M8 ───────────────────────────────────────────────────────

    #[test]
    fn quote_arg_leaves_simple_args_alone() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg("--flag=value"), "--flag=value");
        assert_eq!(
            quote_arg(r"C:\Users\me\buffr-app.exe"),
            r"C:\Users\me\buffr-app.exe"
        );
    }

    #[test]
    fn quote_arg_wraps_args_with_spaces() {
        assert_eq!(
            quote_arg(r"C:\My Docs\page.html"),
            "\"C:\\My Docs\\page.html\""
        );
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn quote_arg_quotes_the_empty_string() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn quote_arg_escapes_embedded_quotes() {
        // a"b  →  "a\"b"
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        // --title="x"  →  "--title=\"x\""
        assert_eq!(quote_arg(r#"--title="x""#), r#""--title=\"x\"""#);
    }

    #[test]
    fn quote_arg_doubles_backslashes_before_a_quote() {
        // a\"b : the backslash is doubled, then the quote is escaped.
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        // a\\"b : two backslashes → four, then the escaped quote.
        assert_eq!(quote_arg("a\\\\\"b"), "\"a\\\\\\\\\\\"b\"");
    }

    #[test]
    fn quote_arg_doubles_trailing_backslashes() {
        // "dir\" would escape the closing quote → must become "dir\\".
        assert_eq!(quote_arg(r"a dir\"), "\"a dir\\\\\"");
        assert_eq!(quote_arg("a dir\\\\"), "\"a dir\\\\\\\\\"");
    }

    /// Round-trip against the same parsing rules `CommandLineToArgvW` uses,
    /// so the escaping is verified rather than merely pinned.
    #[test]
    fn quote_arg_round_trips_through_a_reference_parser() {
        let cases = [
            "simple",
            "with space",
            r#"embedded"quote"#,
            r"trailing\",
            r"trailing\\",
            r"C:\My Docs\page.html",
            r#"mix "a\b" c\"#,
            "",
            "--url=https://example.com/?q=a b",
        ];
        for case in cases {
            let quoted = quote_arg(case);
            let parsed = parse_windows_args(&quoted);
            assert_eq!(
                parsed,
                vec![case.to_string()],
                "round-trip failed for {case:?} (quoted as {quoted:?})"
            );
        }
    }

    #[test]
    fn build_command_line_quotes_binary_and_every_argument() {
        let bin = Path::new(r"C:\Program Files\buffr\buffr-app.exe");
        let args = vec![
            OsString::from(r"C:\My Docs\page.html"),
            OsString::from("--private"),
        ];
        let line = build_command_line(bin, &args);
        assert_eq!(
            parse_windows_args(&line),
            vec![
                r"C:\Program Files\buffr\buffr-app.exe".to_string(),
                r"C:\My Docs\page.html".to_string(),
                "--private".to_string(),
            ]
        );
    }

    /// Minimal `CommandLineToArgvW` implementation (post-argv[0] rules,
    /// which is also how argv[0] is parsed when it is quoted) used to verify
    /// [`quote_arg`] without needing a Windows host.
    fn parse_windows_args(cmdline: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut started = false;
        let mut backslashes = 0usize;
        let mut chars = cmdline.chars().peekable();

        let flush_backslashes = |cur: &mut String, n: &mut usize| {
            for _ in 0..*n {
                cur.push('\\');
            }
            *n = 0;
        };

        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    backslashes += 1;
                    started = true;
                }
                '"' => {
                    if backslashes.is_multiple_of(2) {
                        for _ in 0..(backslashes / 2) {
                            cur.push('\\');
                        }
                        backslashes = 0;
                        // A `""` inside a quoted section is a literal quote.
                        if in_quotes && chars.peek() == Some(&'"') {
                            chars.next();
                            cur.push('"');
                        } else {
                            in_quotes = !in_quotes;
                        }
                    } else {
                        for _ in 0..(backslashes / 2) {
                            cur.push('\\');
                        }
                        backslashes = 0;
                        cur.push('"');
                    }
                    started = true;
                }
                ' ' | '\t' if !in_quotes => {
                    flush_backslashes(&mut cur, &mut backslashes);
                    if started {
                        args.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                other => {
                    flush_backslashes(&mut cur, &mut backslashes);
                    cur.push(other);
                    started = true;
                }
            }
        }
        flush_backslashes(&mut cur, &mut backslashes);
        if started {
            args.push(cur);
        }
        args
    }

    // ── wait_for_connect / watch_heartbeat ───────────────────────────────────

    /// Scriptable [`ChildHandle`] for the watch tests.
    struct FakeChild {
        /// Number of `poll_exit` calls before the child "exits".
        exit_after_polls: Option<usize>,
        polls: usize,
        exit: Option<ExitInfo>,
        killed: bool,
    }

    impl FakeChild {
        fn never_exits() -> Self {
            Self {
                exit_after_polls: None,
                polls: 0,
                exit: None,
                killed: false,
            }
        }

        fn exits_after(n: usize) -> Self {
            Self {
                exit_after_polls: Some(n),
                polls: 0,
                exit: None,
                killed: false,
            }
        }
    }

    impl ChildHandle for FakeChild {
        fn pid(&self) -> u32 {
            4242
        }

        fn poll_exit(&mut self) -> Option<ExitInfo> {
            if let Some(e) = self.exit {
                return Some(e);
            }
            self.polls += 1;
            if let Some(n) = self.exit_after_polls
                && self.polls > n
            {
                let e = normal(0);
                self.exit = Some(e);
                return Some(e);
            }
            None
        }

        fn wait_exit(&mut self) -> ExitInfo {
            let e = self.exit.unwrap_or_else(|| normal(0));
            self.exit = Some(e);
            e
        }

        fn kill_and_reap(&mut self) -> ExitInfo {
            self.killed = true;
            let e = self.exit.unwrap_or_else(signalled);
            self.exit = Some(e);
            e
        }
    }

    #[test]
    fn wait_for_connect_reports_connected() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(HeartbeatEvent::Connected).unwrap();
        let mut child = FakeChild::never_exits();
        assert!(matches!(
            wait_for_connect(&rx, &mut child, Duration::from_secs(5)),
            ConnectResult::Connected
        ));
    }

    #[test]
    fn wait_for_connect_reports_a_fast_exiting_child() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut child = FakeChild::exits_after(0);
        assert!(matches!(
            wait_for_connect(&rx, &mut child, Duration::from_secs(5)),
            ConnectResult::ChildExited(_)
        ));
    }

    #[test]
    fn wait_for_connect_times_out() {
        let (_tx, rx) = std::sync::mpsc::channel::<HeartbeatEvent>();
        let mut child = FakeChild::never_exits();
        assert!(matches!(
            wait_for_connect(&rx, &mut child, Duration::from_millis(50)),
            ConnectResult::TimedOut
        ));
    }

    #[test]
    fn watch_heartbeat_reports_a_hang_when_pings_stop() {
        let (_tx, rx) = std::sync::mpsc::channel::<HeartbeatEvent>();
        let mut child = FakeChild::never_exits();
        let outcome = watch_heartbeat(
            &rx,
            &mut child,
            Instant::now() + Duration::from_millis(50),
            Duration::from_millis(50),
        );
        assert!(matches!(outcome, WatchOutcome::Hang));
        assert!(child.killed, "a hang must kill the child");
    }

    /// H12 / M2 regression: a child that closes the heartbeat transport but
    /// keeps running must be killed and reported as a hang. Reporting
    /// "exited" made the Unix supervisor block forever in `wait()` and made
    /// the Windows supervisor record `STILL_ACTIVE` (259) as an exit code and
    /// spawn a *second* browser.
    #[test]
    fn watch_heartbeat_treats_disconnect_while_alive_as_a_hang() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(HeartbeatEvent::Disconnected).unwrap();
        let mut child = FakeChild::never_exits();
        let outcome = watch_heartbeat(
            &rx,
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
        );
        assert!(
            matches!(outcome, WatchOutcome::Hang),
            "a live child that closed the transport is a hang, not an exit"
        );
        assert!(child.killed, "the wedged child must be killed");
    }

    /// The normal shutdown sequence — the child closes the transport and then
    /// exits — must still be reported as an exit, not a hang.
    #[test]
    fn watch_heartbeat_reports_exit_when_disconnect_is_followed_by_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(HeartbeatEvent::Disconnected).unwrap();
        // Exits on the second poll, well inside DISCONNECT_EXIT_GRACE.
        let mut child = FakeChild::exits_after(1);
        let outcome = watch_heartbeat(
            &rx,
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
        );
        match outcome {
            WatchOutcome::Exited(info) => assert_eq!(info.code, Some(0)),
            WatchOutcome::Hang => panic!("a child that exits promptly is not a hang"),
        }
        assert!(!child.killed, "no kill needed for a clean exit");
    }

    /// The listener thread going away is the same situation as an explicit
    /// disconnect and must get the same bounded grace.
    #[test]
    fn watch_heartbeat_treats_a_dead_listener_thread_as_a_hang() {
        let (tx, rx) = std::sync::mpsc::channel::<HeartbeatEvent>();
        drop(tx);
        let mut child = FakeChild::never_exits();
        let outcome = watch_heartbeat(
            &rx,
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
        );
        assert!(matches!(outcome, WatchOutcome::Hang));
        assert!(child.killed);
    }
}
