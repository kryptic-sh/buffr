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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Config, ConfigError, load_from_path, validate};

/// Debounce window for filesystem events. 250ms matches typical "atomic
/// save" sequences that editors emit.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// RAII guard for an active config watcher. Drop to stop watching.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    _thread: Option<thread::JoinHandle<()>>,
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

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            )
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

    let cb = Arc::new(Mutex::new(callback));
    let path_for_thread = path.clone();
    let handle = thread::spawn(move || {
        let mut last: Option<Instant> = None;
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
            // Coarse rate-limit: never fire twice within DEBOUNCE.
            if let Some(prev) = last
                && prev.elapsed() < DEBOUNCE
            {
                continue;
            }
            last = Some(Instant::now());
            let result = load_from_path(&path_for_thread).and_then(|(cfg, _)| {
                validate(&cfg)?;
                Ok(cfg)
            });
            if let Ok(cb) = cb.lock() {
                cb(result);
            }
        }
    });

    Ok(ConfigWatcher {
        _watcher: watcher,
        _thread: Some(handle),
    })
}
