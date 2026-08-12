//! Filesystem watcher for `config.toml`.
//!
//! Wraps `notify`'s recommended watcher with a 250ms debounce so a
//! single editor save (which frequently shows up as several events on
//! Linux: rename + create + modify) collapses into one reload.
//!
//! The returned [`ConfigWatcher`] is an opaque RAII guard: dropping it
//! stops the watcher thread.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Config, ConfigError, load_from_path, validate};

/// Debounce window for filesystem events. 250ms matches typical "atomic
/// save" sequences that editors emit.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// RAII guard for an active config watcher. Drop to stop watching.
pub struct ConfigWatcher {
    _watcher: Option<RecommendedWatcher>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // Drop the watcher first so the notification channel closes and
        // the background thread's rx.recv() returns Err, causing the
        // thread to exit quickly.
        drop(self._watcher.take());
        // Join the background thread so the callback is guaranteed to
        // have finished before any teardown callers of this struct.
        if let Some(handle) = self._thread.take() {
            let _ = handle.join();
        }
    }
}

/// Watch `path` for changes; on each debounced change, re-load + validate
/// and pass the result to `callback`.
///
/// `callback` runs on a background thread. It must be `Send + 'static`.
pub fn watch<F>(path: PathBuf, callback: F) -> Result<ConfigWatcher, ConfigError>
where
    F: Fn(Result<Config, ConfigError>) + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<()>();

    // We register on the *parent directory* (see below), so the raw event
    // stream includes every sibling in `~/.config/buffr/` — other config
    // files, editor swap files, `.git` churn. Filter down to events that
    // actually name our target path, otherwise unrelated churn triggers a
    // full reload + validate + user callback (and a spurious
    // `ConfigError::Io` whenever the config file happens to be absent).
    let path_for_filter = path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            )
            && event.paths.iter().any(|p| p == &path_for_filter)
        {
            let _ = tx.send(());
        }
    })
    .map_err(|e| ConfigError::Io {
        path: path.clone(),
        source: std::io::Error::other(e.to_string()),
    })?;

    // Watch the parent directory so atomic-rename saves (editor writes
    // a tempfile next to the target then renames) are still observed.
    let watch_target = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    watcher
        .watch(&watch_target, RecursiveMode::NonRecursive)
        .map_err(|e| ConfigError::Io {
            path: watch_target.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;

    let path_for_thread = path.clone();
    let handle = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loop {
                // Block until at least one event arrives (or sender dropped).
                match rx.recv() {
                    Ok(()) => {}
                    Err(_) => return,
                }
                // Drain anything else queued in the debounce window.
                let deadline = Instant::now() + DEBOUNCE;
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    match rx.recv_timeout(deadline - now) {
                        Ok(()) => continue,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                // The drain loop above always runs a full DEBOUNCE past the
                // triggering `recv()`, so no additional rate-limit is needed
                // (and one would only serve to drop config changes).
                let result = load_from_path(&path_for_thread).and_then(|(cfg, _)| {
                    validate(&cfg)?;
                    Ok(cfg)
                });
                // Isolate a panicking callback: it must neither poison a
                // shared lock (skipping every later reload silently) nor
                // kill this thread. Reloads continue; the panic is logged.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(result)))
                    .is_err()
                {
                    eprintln!("[buffr-config] config watcher callback panicked; reloads continue");
                }
            }
        }));
        if let Err(panic) = result {
            let msg = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            eprintln!("[buffr-config] config watcher thread panicked: {msg}");
        }
    });

    Ok(ConfigWatcher {
        _watcher: Some(watcher),
        _thread: Some(handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Poll `f` until it returns true or `timeout` elapses.
    fn wait_until(timeout: Duration, f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        f()
    }

    #[test]
    fn reload_fires_for_the_watched_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cfg_path = root.join("config.toml");
        std::fs::write(&cfg_path, "[general]\nleader = \" \"\n").unwrap();

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        let _guard = watch(cfg_path.clone(), move |res| {
            assert!(res.is_ok(), "expected a valid reload, got {res:?}");
            hits_cb.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

        std::fs::write(&cfg_path, "[general]\nleader = \",\"\n").unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || hits.load(Ordering::SeqCst) > 0),
            "watcher never fired for its own file"
        );
    }

    #[test]
    fn panicking_callback_does_not_skip_later_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cfg_path = root.join("config.toml");
        std::fs::write(&cfg_path, "[general]\nleader = \" \"\n").unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cb = Arc::clone(&calls);
        let _guard = watch(cfg_path.clone(), move |_res| {
            // Panic on the first reload, then succeed. A panicking
            // callback must not poison a shared lock and skip every
            // later reload.
            if calls_cb.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first callback panics on purpose");
            }
        })
        .unwrap();

        std::fs::write(&cfg_path, "[general]\nleader = \",\"\n").unwrap();
        // Space the writes past the debounce so they are distinct reloads.
        thread::sleep(DEBOUNCE * 2);
        std::fs::write(&cfg_path, "[general]\nleader = \".\"\n").unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || calls.load(Ordering::SeqCst) >= 2),
            "reloads stopped after a panicking callback"
        );
    }

    #[test]
    fn sibling_churn_does_not_trigger_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cfg_path = root.join("config.toml");
        // Deliberately absent: before the M35 fix, sibling churn produced a
        // spurious `ConfigError::Io` callback for the missing config.
        let sibling = root.join("keybinds.toml");

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = Arc::clone(&hits);
        let _guard = watch(cfg_path, move |_res| {
            hits_cb.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

        for i in 0..5 {
            std::fs::write(&sibling, format!("n = {i}\n")).unwrap();
        }
        std::fs::remove_file(&sibling).unwrap();

        // Generously past DEBOUNCE — nothing should have fired.
        thread::sleep(DEBOUNCE * 6);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "sibling file churn triggered a config reload"
        );
    }
}
