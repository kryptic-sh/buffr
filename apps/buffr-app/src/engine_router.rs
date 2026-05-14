//! `engine_router` — URL-to-engine dispatch table.
//!
//! Holds a registry of registered [`BrowserEngine`] backends keyed by
//! [`EngineId`]. Resolves a URL to the correct engine via an ordered
//! list of glob rules (matched against the URL's host component) with a
//! configurable default fallback.
//!
//! # Design decisions
//!
//! - **`globset` for glob matching** — same wildcard syntax as Chromium's
//!   enterprise URL filter policies (`*.example.com`, `example.com`).
//!   Chosen over `wildmatch` for its battle-tested correctness and clear
//!   escape semantics.
//! - **Host-scoped matching** — we extract the host via `url::Url::host_str`
//!   and match against that string only. `file://`, `about:blank`, and
//!   `data:` URLs have no host → they always fall through to the default
//!   engine. Tests cover this explicitly.
//! - **Case-insensitive** — both the host and the compiled glob pattern are
//!   lowercased before comparison so `FIGMA.COM` and `figma.com` behave the
//!   same.
//! - **`Arc<dyn BrowserEngine>`** — engines are shared across the app
//!   (event handlers, paint callbacks, …) so reference-counting is the
//!   natural ownership model. The router borrows through `Arc` and does not
//!   take exclusive ownership.

use std::collections::HashMap;
use std::sync::Arc;

use buffr_engine::{BrowserEngine, EngineId};
use globset::{Glob, GlobMatcher};
use thiserror::Error;
use url::Url;

/// One compiled per-domain routing rule.
struct CompiledRule {
    matcher: GlobMatcher,
    engine: EngineId,
}

/// Routes a URL to the [`BrowserEngine`] that should host it.
///
/// Build via [`EngineRouterBuilder`] (obtained from [`EngineRouter::builder`]).
///
/// `Debug` is implemented manually because `Arc<dyn BrowserEngine>` is not
/// `Debug`.
pub struct EngineRouter {
    engines: HashMap<EngineId, Arc<dyn BrowserEngine>>,
    default: EngineId,
    rules: Vec<CompiledRule>,
}

/// Errors that [`EngineRouterBuilder::build`] can return.
#[derive(Debug, Error)]
pub enum RouterError {
    /// A config reference names an engine id that was never registered.
    #[error("engine id `{0}` referenced in config but not registered")]
    UnknownEngine(String),
    /// A `match` glob pattern in the config could not be compiled.
    #[error("invalid glob pattern `{pattern}`: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    /// `build()` was called with zero registered engines.
    #[error("empty engine registry")]
    EmptyRegistry,
}

impl std::fmt::Debug for EngineRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineRouter")
            .field("engines", &self.engines.keys().collect::<Vec<_>>())
            .field("default", &self.default)
            .field("rules_count", &self.rules.len())
            .finish()
    }
}

impl EngineRouter {
    /// Returns a fresh [`EngineRouterBuilder`].
    pub fn builder() -> EngineRouterBuilder {
        EngineRouterBuilder::default()
    }

    /// Resolve a URL string to the [`EngineId`] that should host it.
    ///
    /// Resolution order:
    ///
    /// 1. Parse `url` and extract the host component (lowercased).
    /// 2. Walk `rules` in declaration order; return the first matching rule's
    ///    engine id.
    /// 3. Fall back to `self.default`.
    ///
    /// URLs with no host (`about:blank`, `data:`, `file://path`) skip step 2
    /// and immediately return the default.
    pub fn resolve(&self, url: &str) -> &EngineId {
        if let Ok(parsed) = Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            let host_lc = host.to_lowercase();
            for rule in &self.rules {
                if rule.matcher.is_match(&host_lc) {
                    return &rule.engine;
                }
            }
        }
        &self.default
    }

    /// Get a handle to the engine that should host `url`.
    ///
    /// # Panics
    ///
    /// Never in normal operation — the builder invariant ensures every
    /// resolved [`EngineId`] maps to a registered engine.
    pub fn engine_for(&self, url: &str) -> &Arc<dyn BrowserEngine> {
        let id = self.resolve(url);
        self.engines
            .get(id)
            .expect("router invariant: resolved id always registered")
    }

    /// Iterator over all registered engine ids.
    pub fn engine_ids(&self) -> impl Iterator<Item = &EngineId> {
        self.engines.keys()
    }

    /// Direct lookup by id (e.g. for issuing engine-wide commands like
    /// resize-all or close-all). Reserved for Phase 3 multi-engine use.
    #[allow(dead_code)]
    pub fn get(&self, id: &EngineId) -> Option<&Arc<dyn BrowserEngine>> {
        self.engines.get(id)
    }
}

/// Builder for [`EngineRouter`].
#[derive(Default)]
pub struct EngineRouterBuilder {
    engines: HashMap<EngineId, Arc<dyn BrowserEngine>>,
    default: Option<EngineId>,
    rule_specs: Vec<(String, String)>,
}

impl EngineRouterBuilder {
    /// Register a backend engine under `id`.
    pub fn register(mut self, id: EngineId, engine: Arc<dyn BrowserEngine>) -> Self {
        self.engines.insert(id, engine);
        self
    }

    /// Set the default engine id (used when no rule matches).
    ///
    /// If not called, the default is the first registered engine.
    pub fn default_engine(mut self, id: EngineId) -> Self {
        self.default = Some(id);
        self
    }

    /// Add a per-domain routing rule.
    ///
    /// `pattern` is a glob matched against the URL's host component
    /// (case-insensitive). `engine` is the id of the engine to route to.
    pub fn rule(mut self, pattern: impl Into<String>, engine: impl Into<String>) -> Self {
        self.rule_specs.push((pattern.into(), engine.into()));
        self
    }

    /// Validate and compile the router configuration.
    ///
    /// Returns [`RouterError::EmptyRegistry`] when no engines were registered,
    /// [`RouterError::UnknownEngine`] when the default or a rule references an
    /// unregistered engine id, or [`RouterError::InvalidGlob`] for malformed
    /// glob patterns.
    pub fn build(self) -> Result<EngineRouter, RouterError> {
        if self.engines.is_empty() {
            return Err(RouterError::EmptyRegistry);
        }

        // Determine the default engine id.
        let default = self
            .default
            .unwrap_or_else(|| self.engines.keys().next().unwrap().clone());

        if !self.engines.contains_key(&default) {
            return Err(RouterError::UnknownEngine(default.to_string()));
        }

        // Compile glob rules.
        let mut rules = Vec::with_capacity(self.rule_specs.len());
        for (pattern, engine_str) in self.rule_specs {
            let engine = EngineId::new(engine_str.clone());
            if !self.engines.contains_key(&engine) {
                return Err(RouterError::UnknownEngine(engine_str));
            }
            // Compile against the lowercased pattern so the matcher's
            // case-sensitivity setting doesn't matter — we always lowercase
            // both sides at resolve time.
            let matcher = Glob::new(&pattern.to_lowercase())
                .map_err(|e| RouterError::InvalidGlob {
                    pattern: pattern.clone(),
                    source: e,
                })?
                .compile_matcher();
            rules.push(CompiledRule { matcher, engine });
        }

        Ok(EngineRouter {
            engines: self.engines,
            default,
            rules,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use buffr_engine::{
        EngineError, MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState, SharedOsrFrame,
        SharedOsrViewState, TabId, TabSummary,
    };

    // ── Stub engine ──────────────────────────────────────────────────────────

    /// No-op engine used in tests. All methods panic — the router tests only
    /// exercise `resolve` / `engine_for`, not the engine interface.
    struct StubEngine;

    impl BrowserEngine for StubEngine {
        fn close_all_browsers(&self) {}
        fn open_tab(&self, _url: &str) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn open_tab_background(&self, _url: &str) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn open_tab_at(&self, _url: &str, _idx: usize) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn close_tab(&self, _id: TabId) -> Result<bool, EngineError> {
            unimplemented!()
        }
        fn close_active(&self) -> Result<bool, EngineError> {
            unimplemented!()
        }
        fn select_tab(&self, _id: TabId) {}
        fn next_tab(&self) {}
        fn prev_tab(&self) {}
        fn move_tab(&self, _from: usize, _to: usize) {}
        fn duplicate_active(&self) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn toggle_pin_active(&self) {}
        fn set_pinned(&self, _id: TabId, _pinned: bool) {}
        fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
            unimplemented!()
        }
        fn closed_stack_len(&self) -> usize {
            0
        }
        fn active_tab(&self) -> Option<TabSummary> {
            None
        }
        fn tabs_summary(&self) -> Vec<TabSummary> {
            vec![]
        }
        fn tab_count(&self) -> usize {
            0
        }
        fn pinned_count(&self) -> usize {
            0
        }
        fn active_index(&self) -> Option<usize> {
            None
        }
        fn navigate(&self, _url: &str) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn active_tab_live_url(&self) -> String {
            String::new()
        }
        fn pump_address_changes(&self) -> bool {
            false
        }
        fn resize(&self, _w: u32, _h: u32) {}
        fn set_device_scale(&self, _scale: f32) {}
        fn set_frame_rate(&self, _hz: u32) {}
        fn notify_screen_info_changed(&self) {}
        fn osr_resize(&self, _w: u32, _h: u32) {}
        fn osr_key_event(&self, _event: NeutralKeyEvent) {}
        fn osr_mouse_move(&self, _x: i32, _y: i32, _mods: u32) {}
        fn osr_mouse_click(
            &self,
            _x: i32,
            _y: i32,
            _btn: MouseButton,
            _up: bool,
            _cnt: i32,
            _mods: u32,
        ) {
        }
        fn osr_mouse_leave(&self, _mods: u32) {}
        fn osr_mouse_wheel(&self, _x: i32, _y: i32, _dx: i32, _dy: i32, _mods: u32) {}
        fn osr_focus(&self, _focused: bool) {}
        fn osr_frame(&self) -> SharedOsrFrame {
            Arc::new(Mutex::new(OsrFrame::new(1, 1)))
        }
        fn osr_view(&self) -> SharedOsrViewState {
            Arc::new(OsrViewState::default())
        }
        fn force_repaint_active(&self) {}
        fn osr_sleep(&self, _sleep: bool) {}
        fn osr_invalidate_view(&self) {}
        fn set_osr_wake(&self, _wake: Arc<dyn Fn() + Send + Sync>) {}
        fn start_find(&self, _query: &str, _forward: bool) {}
        fn stop_find(&self) {}
        fn active_zoom_level(&self) -> f64 {
            0.0
        }
        fn any_audio_active(&self) -> bool {
            false
        }
        fn any_video_active(&self) -> bool {
            false
        }
    }

    fn stub_arc() -> Arc<dyn BrowserEngine> {
        Arc::new(StubEngine)
    }

    // ── resolve tests ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_uses_default_when_no_rules() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        assert_eq!(router.resolve("https://example.com"), &EngineId::new("cef"));
    }

    #[test]
    fn resolve_picks_first_matching_rule() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("figma.com", "webkit")
            .rule("figma.com", "cef") // second rule — should not be reached
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://figma.com/file/abc"),
            &EngineId::new("webkit")
        );
    }

    #[test]
    fn resolve_supports_wildcard_subdomain() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("*.figma.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://www.figma.com/file/xyz"),
            &EngineId::new("webkit"),
            "wildcard subdomain should match"
        );
        assert_eq!(
            router.resolve("https://figma.com/file/xyz"),
            &EngineId::new("cef"),
            "bare domain should not match *.figma.com"
        );
    }

    #[test]
    fn resolve_case_insensitive_host() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("figma.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://FIGMA.COM/file"),
            &EngineId::new("webkit"),
            "host comparison must be case-insensitive"
        );
    }

    #[test]
    fn resolve_returns_default_for_invalid_url() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        // Completely unparseable input.
        assert_eq!(router.resolve("not a url at all"), &EngineId::new("cef"));
    }

    #[test]
    fn resolve_returns_default_for_about_blank() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        // `about:blank` is a valid URL but has no host.
        assert_eq!(router.resolve("about:blank"), &EngineId::new("cef"));
    }

    #[test]
    fn resolve_returns_default_for_data_url() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("data:text/plain,hello"),
            &EngineId::new("cef")
        );
    }

    // ── build error tests ─────────────────────────────────────────────────────

    #[test]
    fn build_rejects_unknown_engine_in_rule() {
        let err = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("*.figma.com", "webkit") // webkit not registered
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RouterError::UnknownEngine(ref s) if s == "webkit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_rejects_unknown_default_engine() {
        let err = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("webkit")) // not registered
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RouterError::UnknownEngine(ref s) if s == "webkit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_rejects_empty_registry() {
        let err = EngineRouter::builder().build().unwrap_err();
        assert!(
            matches!(err, RouterError::EmptyRegistry),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_rejects_invalid_glob() {
        let err = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("[invalid-glob", "cef")
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RouterError::InvalidGlob { .. }),
            "unexpected error: {err}"
        );
    }

    // ── engine_for smoke ──────────────────────────────────────────────────────

    #[test]
    fn engine_for_returns_registered_engine() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        // Should not panic.
        let _engine = router.engine_for("https://example.com");
    }
}
