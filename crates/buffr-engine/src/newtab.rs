//! Engine-agnostic new-tab page constants.
//!
//! These are plain string constants — no CEF dependency. The CEF backend
//! (`buffr-cef::new_tab`) imports them and wires them into its scheme handler.
//! The apps layer can read them directly without importing `buffr-cef`.

/// The URL opened when the user presses `t` (TabNew).
pub const NEW_TAB_URL: &str = "buffr://new";

/// The URL for the engine-routing settings scaffold.
pub const SETTINGS_URL: &str = "buffr://settings";

/// Embedded new-tab HTML template. Contains marker strings that the apps
/// layer fills in at request time, so a config hot-reload is reflected on
/// the next page visit without a binary rebuild.
pub static NEW_TAB_HTML_TEMPLATE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/new_tab.html"));

/// The marker the apps layer replaces with rendered keybinding rows.
pub const NEW_TAB_KEYBINDS_MARKER: &str = "<!--KEYBINDS-->";

/// Marker the apps layer replaces with the static splash wordmark
/// (per-cell HTML grid). Substituted once per page request alongside
/// the keybindings; the splash overlay (`#buffr-splash`) is updated
/// per tick by the host via execute_javascript.
pub const NEW_TAB_SPLASH_ART_MARKER: &str = "<!--SPLASH-ART-->";
