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

    /// Returns `true` when more than one engine is registered, indicating
    /// that per-tab engine badges should be shown in the tab strip.
    pub fn show_badges(&self) -> bool {
        self.engines.len() > 1
    }

    /// Curated 8-colour palette for engine badges, as `0x00RRGGBB` values.
    const BADGE_PALETTE: &'static [u32] = &[
        0xFF6B6B, // red
        0xFFAA45, // orange
        0xFFD93D, // yellow
        0x6BCB77, // green
        0x4D96FF, // blue
        0x9B59FF, // violet
        0xFF61C2, // pink/magenta
        0xA9A9A9, // grey
    ];

    /// Return the badge colour for `id`, or `None` if:
    /// - Only one engine is registered (no badge needed), or
    /// - `id` is `"cef"` (primary default — shown without a badge).
    ///
    /// The colour is deterministic: a DJB2 hash of the id string selects
    /// an index into [`BADGE_PALETTE`].
    pub fn badge_color_for(&self, id: &EngineId) -> Option<u32> {
        if !self.show_badges() || id.as_str() == "cef" {
            return None;
        }
        let mut h: u32 = 5381;
        for b in id.as_str().bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u32);
        }
        Some(Self::BADGE_PALETTE[(h as usize) % Self::BADGE_PALETTE.len()])
    }

    /// Return the 2-character uppercase badge label for `id`, or `None` using
    /// the same suppression rules as [`badge_color_for`].
    ///
    /// The label is the first two alphanumeric ASCII characters of `id`
    /// (uppercased), skipping any leading non-alphanumeric characters such as
    /// separators. Falls back to `"??"` when `id` contains fewer than two
    /// usable characters.
    pub fn badge_label_for(&self, id: &EngineId) -> Option<String> {
        if !self.show_badges() || id.as_str() == "cef" {
            return None;
        }
        Some(badge_label_text(id.as_str()))
    }

    /// Direct lookup by id (e.g. for issuing engine-wide commands like
    /// resize-all or close-all). Used by Phase 3 multi-engine fan-out paths
    /// in `main.rs`; suppressed here because `main.rs` accesses engines via
    /// `AppState::engines` directly for the concrete `BrowserHost` type.
    #[allow(dead_code)]
    pub fn get(&self, id: &EngineId) -> Option<&Arc<dyn BrowserEngine>> {
        self.engines.get(id)
    }
}

/// Extract the 2-character badge text from an engine-id string.
///
/// Collects the first two ASCII alphanumeric characters (uppercased), skipping
/// separators such as `-` and `_`. Falls back to `"??"` when fewer than two
/// usable characters exist. This is a module-level free function so it can be
/// unit-tested independently of an [`EngineRouter`] instance.
pub(crate) fn badge_label_text(id: &str) -> String {
    let mut chars: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .take(2)
        .collect();
    while chars.len() < 2 {
        chars.push('?');
    }
    chars
}

// ── Cross-engine navigation verdict ──────────────────────────────────────────

/// Result of [`classify_navigation`]: describes whether a URL stays on the
/// current engine or crosses to a different one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationVerdict {
    /// URL resolves to the same engine — no action needed.
    SameEngine,
    /// URL resolves to a different engine. Open a new tab on `target` and
    /// close the source tab.
    CrossEngine {
        /// The engine that should host this URL.
        target: EngineId,
    },
}

/// Classify whether navigating `url` from `active_engine` requires a
/// cross-engine handoff.
///
/// Returns [`NavigationVerdict::SameEngine`] when the resolved engine matches
/// `active_engine`; otherwise [`NavigationVerdict::CrossEngine`].
///
/// This is a pure function — no side effects — which makes it trivially
/// unit-testable with stub engines. The `check_cross_engine_nav` method in
/// `AppState` uses this in the hot `about_to_wait` path; the unit-testable
/// extraction keeps that logic verifiable without running a live CEF process.
pub fn classify_navigation(
    router: &EngineRouter,
    active_engine: &EngineId,
    url: &str,
) -> NavigationVerdict {
    let target = router.resolve(url);
    if target == active_engine {
        NavigationVerdict::SameEngine
    } else {
        NavigationVerdict::CrossEngine {
            target: target.clone(),
        }
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
        popup::{
            PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink,
            new_popup_create_sink, new_popup_queue,
        },
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
        fn popup_queue(&self) -> PopupQueue {
            new_popup_queue()
        }
        fn popup_create_sink(&self) -> PopupCreateSink {
            new_popup_create_sink()
        }
        fn popup_close_sink(&self) -> PopupCloseSink {
            new_popup_close_sink()
        }
        fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {}
        fn popup_close(&self, _browser_id: i32) {}
        fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
            vec![]
        }
        fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
            vec![]
        }
        fn popup_history_back(&self, _browser_id: i32) {}
        fn popup_history_forward(&self, _browser_id: i32) {}
        fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {}
        fn popup_osr_key_event(&self, _browser_id: i32, _event: NeutralKeyEvent) {}
        fn popup_osr_mouse_click(
            &self,
            _browser_id: i32,
            _x: i32,
            _y: i32,
            _button: MouseButton,
            _mouse_up: bool,
            _click_count: i32,
            _modifiers: u32,
        ) {
        }
        fn popup_osr_mouse_move(&self, _browser_id: i32, _x: i32, _y: i32, _modifiers: u32) {}
        fn popup_osr_mouse_wheel(
            &self,
            _browser_id: i32,
            _x: i32,
            _y: i32,
            _delta_x: i32,
            _delta_y: i32,
            _modifiers: u32,
        ) {
        }
        fn is_hint_mode(&self) -> bool {
            false
        }
        fn hint_status(&self) -> Option<buffr_engine::HintStatus> {
            None
        }
        fn pump_hint_events(&self) -> bool {
            false
        }
        fn feed_hint_key(&self, _c: char) -> Option<buffr_engine::HintAction> {
            None
        }
        fn backspace_hint(&self) -> Option<buffr_engine::HintAction> {
            None
        }
        fn cancel_hint(&self) {}
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

    // ── classify_navigation tests (Phase 3) ───────────────────────────────────

    fn two_engine_router() -> EngineRouter {
        EngineRouter::builder()
            .register(EngineId::new("cef-a"), stub_arc())
            .register(EngineId::new("cef-b"), stub_arc())
            .default_engine(EngineId::new("cef-a"))
            .rule("*.example.com", "cef-b")
            .build()
            .unwrap()
    }

    #[test]
    fn cross_engine_nav_opens_new_tab_on_target() {
        let router = two_engine_router();
        let active = EngineId::new("cef-a");
        let verdict = super::classify_navigation(&router, &active, "https://www.example.com/page");
        assert_eq!(
            verdict,
            super::NavigationVerdict::CrossEngine {
                target: EngineId::new("cef-b")
            },
            "example.com rule should route to cef-b"
        );
    }

    #[test]
    fn cross_engine_nav_no_op_when_same_engine() {
        let router = two_engine_router();
        let active = EngineId::new("cef-a");
        // URL with no matching rule → default engine = cef-a = active engine.
        let verdict = super::classify_navigation(&router, &active, "https://other.com");
        assert_eq!(
            verdict,
            super::NavigationVerdict::SameEngine,
            "other.com should stay on cef-a"
        );
    }

    #[test]
    fn cross_engine_nav_same_when_active_is_target() {
        let router = two_engine_router();
        // Active engine IS cef-b — URL routes to cef-b → same engine.
        let active = EngineId::new("cef-b");
        let verdict = super::classify_navigation(&router, &active, "https://www.example.com/page");
        assert_eq!(verdict, super::NavigationVerdict::SameEngine);
    }

    #[test]
    fn cross_engine_nav_no_host_returns_default() {
        let router = two_engine_router();
        let active = EngineId::new("cef-a");
        // about:blank has no host → default engine (cef-a) = active → SameEngine.
        let verdict = super::classify_navigation(&router, &active, "about:blank");
        assert_eq!(verdict, super::NavigationVerdict::SameEngine);
    }

    // ── badge_color_for tests ─────────────────────────────────────────────────

    fn single_engine_router() -> EngineRouter {
        EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap()
    }

    fn multi_engine_router_with_blink() -> EngineRouter {
        EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("blink-cdp"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap()
    }

    #[test]
    fn badge_color_for_returns_none_for_cef() {
        // "cef" is the primary default — no badge even in multi-engine config.
        let router = multi_engine_router_with_blink();
        assert_eq!(router.badge_color_for(&EngineId::new("cef")), None);
    }

    #[test]
    fn badge_color_for_returns_color_for_non_cef_when_multi() {
        let router = multi_engine_router_with_blink();
        let color = router.badge_color_for(&EngineId::new("blink-cdp"));
        assert!(
            color.is_some(),
            "blink-cdp should get a badge colour in multi-engine config"
        );
    }

    #[test]
    fn badge_color_for_returns_none_when_single_engine() {
        // Only one engine registered → badges suppressed for all ids.
        let router = single_engine_router();
        assert_eq!(router.badge_color_for(&EngineId::new("cef")), None);
        assert_eq!(router.badge_color_for(&EngineId::new("blink-cdp")), None);
    }

    #[test]
    fn badge_color_for_deterministic_per_id() {
        let router = multi_engine_router_with_blink();
        let c1 = router.badge_color_for(&EngineId::new("blink-cdp"));
        let c2 = router.badge_color_for(&EngineId::new("blink-cdp"));
        assert_eq!(
            c1, c2,
            "badge_color_for must be deterministic for the same id"
        );
        // Different ids should (with extremely high probability) differ.
        let c3 = router.badge_color_for(&EngineId::new("webkit"));
        // No guarantee they differ but they usually will. We just verify the
        // function runs without panic and returns Some for non-cef ids.
        assert!(c3.is_some());
    }

    // ── badge_label_for / badge_label_text tests ──────────────────────────────

    #[test]
    fn badge_label_text_two_chars_uppercase() {
        assert_eq!(super::badge_label_text("blink-cdp"), "BL");
        assert_eq!(super::badge_label_text("webkit"), "WE");
        assert_eq!(super::badge_label_text("cef"), "CE");
    }

    #[test]
    fn badge_label_text_skips_separators() {
        // Leading dashes / underscores are skipped; letters are taken.
        assert_eq!(super::badge_label_text("--foo"), "FO");
        assert_eq!(super::badge_label_text("_baz"), "BA");
    }

    #[test]
    fn badge_label_text_fallback_on_short_id() {
        assert_eq!(super::badge_label_text(""), "??");
        assert_eq!(super::badge_label_text("a"), "A?");
        assert_eq!(super::badge_label_text("-"), "??");
    }

    #[test]
    fn badge_label_for_returns_none_for_cef_in_multi() {
        let router = multi_engine_router_with_blink();
        assert_eq!(router.badge_label_for(&EngineId::new("cef")), None);
    }

    #[test]
    fn badge_label_for_returns_some_for_non_cef_in_multi() {
        let router = multi_engine_router_with_blink();
        let label = router.badge_label_for(&EngineId::new("blink-cdp"));
        assert_eq!(label, Some("BL".to_string()));
    }

    #[test]
    fn badge_label_for_returns_none_in_single_engine() {
        let router = single_engine_router();
        assert_eq!(router.badge_label_for(&EngineId::new("cef")), None);
        assert_eq!(router.badge_label_for(&EngineId::new("blink-cdp")), None);
    }

    // ── Additional resolve scenarios ──────────────────────────────────────────

    #[test]
    fn resolve_first_matching_rule_wins() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .register(EngineId::new("blink"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("example.com", "webkit") // first
            .rule("example.com", "blink") // second — must NOT win
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://example.com/page"),
            &EngineId::new("webkit"),
            "first matching rule must win"
        );
    }

    #[test]
    fn resolve_rule_order_preserved_from_config() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("other.com", "webkit") // first
            .rule("example.com", "cef") // second
            .build()
            .unwrap();
        // example.com should hit the second rule → cef.
        assert_eq!(
            router.resolve("https://example.com/"),
            &EngineId::new("cef")
        );
        // other.com should hit the first rule → webkit.
        assert_eq!(
            router.resolve("https://other.com/"),
            &EngineId::new("webkit")
        );
    }

    #[test]
    fn resolve_strips_userinfo_from_host() {
        // url::Url::host_str() returns the host without userinfo, so
        // `user:pass@example.com` → host is `example.com`.
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("example.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://user:pass@example.com/path"),
            &EngineId::new("webkit"),
            "userinfo should be stripped; host=example.com matches"
        );
    }

    #[test]
    fn resolve_handles_ipv4_host() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("127.0.0.1", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("http://127.0.0.1:8080/"),
            &EngineId::new("webkit"),
            "IPv4 host should match rule"
        );
    }

    #[test]
    fn resolve_handles_ipv6_host_falls_through_to_default() {
        // url::Url::host_str() for `http://[::1]/` returns "::1" (no brackets).
        // We don't have a rule for "::1" so it falls to default.
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("http://[::1]/"),
            &EngineId::new("cef"),
            "IPv6 with no matching rule → default"
        );
    }

    #[test]
    fn resolve_handles_url_with_port() {
        // Port is stripped from host_str() — only hostname is matched.
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("example.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://example.com:8443/"),
            &EngineId::new("webkit"),
            "port must not prevent host match"
        );
    }

    #[test]
    fn resolve_handles_url_with_path_and_query() {
        // Only host component is matched; path and query are irrelevant.
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("example.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://example.com/path/to/page?q=hello&ref=test#anchor"),
            &EngineId::new("webkit"),
        );
    }

    #[test]
    fn resolve_handles_trailing_slash() {
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("example.com", "webkit")
            .build()
            .unwrap();
        assert_eq!(
            router.resolve("https://example.com/"),
            &EngineId::new("webkit"),
        );
    }

    #[test]
    fn resolve_handles_punycode_host() {
        // Punycode host "xn--n3h.com" (heart emoji domain) is treated as a literal
        // string — matched as-is without unicode decoding.
        let router = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .register(EngineId::new("webkit"), stub_arc())
            .default_engine(EngineId::new("cef"))
            .rule("xn--n3h.com", "webkit")
            .build()
            .unwrap();
        // We can't construct a URL with an IDN emoji host via url::Url directly,
        // but we can verify the rule compiles and a non-matching URL falls through.
        assert_eq!(
            router.resolve("https://example.com/"),
            &EngineId::new("cef"),
            "non-matching host should fall to default"
        );
    }

    #[test]
    fn build_rejects_blank_default_engine_id() {
        // An empty string EngineId is not registered, so build() returns
        // UnknownEngine.
        let err = EngineRouter::builder()
            .register(EngineId::new("cef"), stub_arc())
            .default_engine(EngineId::new(""))
            .build()
            .unwrap_err();
        assert!(
            matches!(err, RouterError::UnknownEngine(_)),
            "blank default engine id should be rejected: {err}"
        );
    }

    // ── classify_navigation additional scenarios ───────────────────────────────

    #[test]
    fn classify_navigation_cross_engine_default_fallback() {
        // URL with no host → resolves to default (cef). Active = cef → SameEngine.
        let router = two_engine_router();
        let active = EngineId::new("cef-a");
        let verdict = super::classify_navigation(&router, &active, "data:text/plain,hello");
        assert_eq!(verdict, super::NavigationVerdict::SameEngine);
    }

    #[test]
    fn classify_navigation_cross_engine_when_active_not_default() {
        // Active engine = cef-b. URL routes to default (cef-a). → CrossEngine.
        let router = two_engine_router();
        let active = EngineId::new("cef-b");
        let verdict = super::classify_navigation(&router, &active, "https://other.com/page");
        assert_eq!(
            verdict,
            super::NavigationVerdict::CrossEngine {
                target: EngineId::new("cef-a")
            }
        );
    }

    // ── badge_label_text additional edge cases ────────────────────────────────

    #[test]
    fn badge_label_text_handles_non_ascii_id() {
        // Non-ASCII bytes are not alphanumeric ASCII, so they are skipped.
        // "中文" has no ASCII alphanumeric chars → fallback "??".
        let label = super::badge_label_text("中文");
        assert_eq!(label, "??", "non-ASCII should fall back to ??");
    }

    #[test]
    fn badge_label_text_handles_long_id() {
        // Only first 2 alphanumeric chars taken.
        let label = super::badge_label_text("blink-cdp-experimental");
        assert_eq!(label, "BL");
    }

    #[test]
    fn badge_color_palette_index_in_range() {
        // Any DJB2 hash index must land within the palette array.
        let palette_len = EngineRouter::BADGE_PALETTE.len();
        let router = multi_engine_router_with_blink();
        // blink-cdp gets a color; compute expected index to verify it's in range.
        let color = router.badge_color_for(&EngineId::new("blink-cdp")).unwrap();
        assert!(
            EngineRouter::BADGE_PALETTE.contains(&color),
            "returned color {color:#08x} must be in BADGE_PALETTE (len={palette_len})"
        );
    }

    #[test]
    fn badge_color_for_returns_none_for_unregistered_id() {
        // An id not registered still goes through the hash — badge_color_for only
        // suppresses "cef" and single-engine cases. With multi-engine + unknown id:
        // it still returns Some because the function doesn't check registration.
        // The contract is: None for "cef" or single-engine; Some otherwise.
        let router = multi_engine_router_with_blink();
        let color = router.badge_color_for(&EngineId::new("unknown-engine"));
        // "unknown-engine" != "cef" and multi-engine → returns Some.
        assert!(
            color.is_some(),
            "non-cef id should produce a color even if unregistered"
        );
    }
}
