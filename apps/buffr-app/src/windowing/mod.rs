//! Platform-conditional windowing abstraction.
//!
//! buffr's chrome compositor and input dispatch are
//! windowing-toolkit-agnostic above this module. winit is used on all
//! platforms; CEF in OSR mode renders into a wgpu surface, so no
//! native window-embedding is needed.
//!
//! Bridge types live here: [`BuffrWindowEvent`] flattens the few
//! variants that different platforms shape differently into a single
//! enum the rest of buffr-app pattern-matches on. Where platforms
//! agree (e.g. `CursorIcon`, modifier shape, `Size`), this module
//! simply re-exports the native type per target.
//!
//! Migration is incremental — see the `windowing/` module commits
//! for the Phase-by-Phase log.

// Concrete backend re-exports. The `other` module re-exports winit
// types shaped so that buffr-app can use one set of names regardless
// of which toolkit is underneath.

mod other;

pub use other::*;
