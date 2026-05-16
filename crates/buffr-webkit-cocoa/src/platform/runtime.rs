//! Per-tab WKWebView + delegate ownership. Lives on the macOS main thread.
//!
//! # Threading model
//!
//! WKWebView is a main-thread-only object. `TabEntry` is `!Send + !Sync`
//! (held in an `Arc<Mutex<RuntimeState>>` inside main-queue closures). Commands
//! are sent to the main thread via `dispatch_async(dispatch_get_main_queue(),…)`.
//!
//! # Delegates
//!
//! Each WKWebView gets its own `BuffrNavigationDelegate` (WKNavigationDelegate)
//! and `BuffrUiDelegate` (WKUIDelegate) installed as strong Objective-C
//! references.
//!
//! Delegate methods update a shared `Arc<Mutex<EngineState>>` so the engine
//! thread can read the latest URL / title / load state without blocking on
//! the main queue.
//!
//! # API verification notes (grepped from registry crate source)
//!
//! - `declare_class!` renamed to `define_class!` in objc2 0.6; the old macro
//!   now emits a compile_error. Confirmed in:
//!   objc2-0.6.4/src/macros/define_class.rs line 504.
//!
//! - `DeclaredClass` was renamed to `DefinedClass`; re-exported as `DeclaredClass`
//!   for compatibility (objc2-0.6.4/src/lib.rs line 201). We use `DefinedClass`
//!   directly to avoid the deprecation alias.
//!
//! - `ClassType` + `Mutability` removed from `define_class!`. New syntax uses
//!   `#[unsafe(super(NSObject))]` and `#[thread_kind = MainThreadOnly]` attrs.
//!
//! - `WKWebViewConfiguration::new(mtm)` requires `MainThreadMarker`.
//!   Confirmed: objc2-web-kit-0.3.2/src/generated/WKWebViewConfiguration.rs:371.
//!
//! - `WKWebView::initWithFrame_configuration(alloc, CGRect, config)` requires
//!   features `WKWebViewConfiguration` + `objc2-core-foundation`. Frame type
//!   is `CGRect` (from objc2-core-foundation), not `NSRect`.
//!   Confirmed: objc2-web-kit-0.3.2/src/generated/WKWebView.rs:238-242.
//!
//! - `setNavigationDelegate` takes `Option<&ProtocolObject<dyn WKNavigationDelegate>>`.
//!   Confirmed: WKWebView.rs:190-193.
//!
//! - `setUIDelegate` takes `Option<&ProtocolObject<dyn WKUIDelegate>>`.
//!   Confirmed: WKWebView.rs:205-207.
//!
//! - `NSURLRequest::requestWithURL(&NSURL)` → `Retained<Self>`.
//!   Confirmed: NSURLRequest.rs:252-254.
//!
//! - `NSURL::URLWithString(&NSString)` → `Option<Retained<NSURL>>`.
//!   Confirmed: NSURL.rs:1178-1181.
//!
//! - `WKWebView::loadRequest(&NSURLRequest)` → `Option<Retained<WKNavigation>>`.
//!   Confirmed: WKWebView.rs:260-262.
//!
//! - `WKWebView::goBack/goForward/reload` → `Option<Retained<WKNavigation>>`.
//!   Confirmed: WKWebView.rs:463-482.
//!
//! - `WKWebView::stopLoading(&self)` → `()`. No navigation return.
//!   Confirmed: WKWebView.rs:493-496.
//!
//! - `WKWebView::canGoBack/canGoForward(&self)` → `bool`.
//!   Confirmed: WKWebView.rs:438-456.
//!
//! - `WKWebView::title(&self)` → `Option<Retained<NSString>>`.
//!   Confirmed: WKWebView.rs:348-350.
//!
//! - `WKWebView::URL(&self)` → `Option<Retained<NSURL>>`.
//!   Confirmed: WKWebView.rs:363-365.
//!
//! - `NSURL::absoluteString(&self)` → `Option<Retained<NSString>>`.
//!   Confirmed: NSURL.rs:1264-1266.
//!
//! - `NSView::setFrame(&self, NSRect)` confirmed in NSView.rs:410-412.
//!
//! - `NSString::from_str(&str)` confirmed in objc2-foundation-0.3.2/src/string.rs:113.
//!
//! - `ProtocolObject::from_ref(obj)` for delegate coercion.
//!   Confirmed: objc2-0.6.4/src/runtime/protocol_object.rs:88-97.
//!
//! - `msg_send_id!` still available in objc2 0.6 (deprecated alias for
//!   `msg_send!` with retained return). We keep it for `init` since it avoids
//!   the retained-return dance. See objc2-0.6.4/src/macros/mod.rs:1306.

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::sync::{Arc, Mutex};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
    use objc2::{ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class};
    use objc2_foundation::{NSError, NSString, NSURL, NSURLRequest};
    use objc2_web_kit::{
        WKNavigation, WKNavigationDelegate, WKScriptMessage, WKScriptMessageHandler, WKUIDelegate,
        WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
        WKWebViewConfiguration,
    };

    use buffr_core::hint::{HintEventSink, parse_console_event};
    use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId};
    use buffr_history::Transition as HistoryTransition;

    use super::super::error::WebKitCocoaError;
    use super::super::worker::EngineState;

    // ── Console-bridge JS injected at document start ──────────────────────────
    //
    // Overrides console.log so that messages starting with the
    // `__buffr_hint__:` sentinel are forwarded to the native handler
    // `window.webkit.messageHandlers.buffrHint.postMessage(msg)`.
    //
    // This mirrors the webkitgtk approach (runtime.rs HINT_CONSOLE_BRIDGE_JS)
    // and the CEF reference impl (host.rs sentinel scraping).
    const HINT_CONSOLE_BRIDGE_JS: &str = r#"(function() {
  var orig = console.log;
  console.log = function() {
    orig.apply(console, arguments);
    var msg = arguments[0];
    if (typeof msg === 'string' && msg.indexOf('__buffr_hint__:') === 0) {
      try { window.webkit.messageHandlers.buffrHint.postMessage(msg); } catch(e) {}
    }
  };
})();"#;

    // ── TabEntry ──────────────────────────────────────────────────────────────

    /// One open tab: owns the WKWebView and its delegates.
    ///
    /// Must only live on the macOS main queue.
    pub(crate) struct TabEntry {
        pub id: TabId,
        /// Strong reference to the WKWebView.
        /// `Retained<WKWebView>` keeps the view alive via ARC.
        pub web_view: Retained<WKWebView>,
        /// Navigation delegate — kept alive by strong ref here.
        /// WKWebView holds only a weak ref (as per Apple docs).
        _nav_delegate: Retained<BuffrNavigationDelegate>,
        /// UI delegate — kept alive by strong ref here.
        _ui_delegate: Retained<BuffrUiDelegate>,
        /// Hint script-message handler — kept alive by strong ref here.
        /// `WKUserContentController` holds only a weak ref internally.
        _hint_handler: Retained<BuffrHintMessageHandler>,
        pub url: String,
        pub title: String,
        pub is_loading: bool,
        /// Current zoom level applied to this tab's WKWebView.
        /// Updated by `set_zoom`; mirrored into `TabState` by
        /// `RuntimeState::snapshot_to_engine_state()`.
        pub zoom: f64,
    }

    impl TabEntry {
        /// Create a new WKWebView, configure it, and load `url`.
        ///
        /// # Safety invariants
        ///
        /// Must be called on the main thread. WKWebView is `MainThreadOnly`.
        ///
        /// # API verification
        ///
        /// - `WKWebViewConfiguration::alloc().init()` confirmed valid constructor
        ///   (objc2-web-kit-0.3.2/src/generated/WKWebViewConfiguration.rs:364-367).
        ///   `new(mtm)` requires `MainThreadMarker` which we don't have in scope
        ///   without passing it through; using `alloc().init()` instead.
        ///
        /// - `WKWebView::alloc().initWithFrame_configuration(CGRect, config)`:
        ///   requires `objc2-core-foundation` feature for `CGRect`.
        ///   Confirmed: WKWebView.rs:215-242.
        ///
        /// - `ProtocolObject::from_ref` converts `&BuffrNavigationDelegate`
        ///   to `&ProtocolObject<dyn WKNavigationDelegate>`.
        ///   Confirmed: objc2/src/runtime/protocol_object.rs:88-97.
        pub(crate) fn open(
            id: TabId,
            url: &str,
            width: u32,
            height: u32,
            state: Arc<Mutex<EngineState>>,
            hint_sink: HintEventSink,
        ) -> Result<Self, WebKitCocoaError> {
            use objc2_core_foundation::CGRect;

            // SAFETY: This function is only called from the GCD main queue
            // (inside dispatch_main_async closures in worker.rs). We are
            // guaranteed to be on the main thread here.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };

            // Build the configuration.
            // WKWebViewConfiguration is MainThreadOnly; alloc() requires mtm.
            // init(this: Allocated<Self>) confirmed: WKWebViewConfiguration.rs:364-367.
            let config =
                unsafe { WKWebViewConfiguration::init(WKWebViewConfiguration::alloc(mtm)) };

            // ── Hint console bridge ───────────────────────────────────────────
            //
            // Get the default WKUserContentController from config and install:
            //   1. A WKUserScript that wraps console.log (injected at document start).
            //   2. A WKScriptMessageHandler named "buffrHint" that receives messages
            //      when the JS sentinel `__buffr_hint__:` is detected.
            //
            // WKWebViewConfiguration::userContentController confirmed:
            //   objc2-web-kit-0.3.2/src/generated/WKWebViewConfiguration.rs:143-145.
            // WKUserContentController::addUserScript confirmed:
            //   objc2-web-kit-0.3.2/src/generated/WKUserContentController.rs:44-50.
            // WKUserContentController::addScriptMessageHandler_name confirmed:
            //   objc2-web-kit-0.3.2/src/generated/WKUserContentController.rs:130-145.
            let hint_handler = BuffrHintMessageHandler::new(mtm, hint_sink);
            unsafe {
                let ucm = config.userContentController();

                // Inject the console.log bridge at document start, all frames.
                // WKUserScript::initWithSource_injectionTime_forMainFrameOnly confirmed:
                //   WKUserScript.rs:90-95.
                let ns_bridge = NSString::from_str(HINT_CONSOLE_BRIDGE_JS);
                let bridge_script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    WKUserScript::alloc(mtm),
                    &ns_bridge,
                    WKUserScriptInjectionTime::AtDocumentStart,
                    false, // inject into all frames
                );
                ucm.addUserScript(&bridge_script);

                // Register the native message handler for "buffrHint".
                // addScriptMessageHandler_name adds
                //   window.webkit.messageHandlers.buffrHint.postMessage to all frames.
                let handler_name = NSString::from_str("buffrHint");
                ucm.addScriptMessageHandler_name(
                    ProtocolObject::from_ref(&*hint_handler),
                    &handler_name,
                );
            }

            // Off-screen rect. WKWebView does not need to be attached to a
            // visible window for snapshots to work.
            // CGRect confirmed via objc2-core-foundation (feature enabled in Cargo.toml).
            let frame = CGRect {
                origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
                size: objc2_core_foundation::CGSize {
                    width: width as f64,
                    height: height as f64,
                },
            };

            // initWithFrame_configuration(this, CGRect, config) confirmed: WKWebView.rs:236-242.
            // Takes CGRect (not NSRect) and requires objc2-core-foundation feature.
            // WKWebView is MainThreadOnly; alloc() requires mtm.
            let web_view = unsafe {
                WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
            };

            // Install navigation delegate.
            // BuffrNavigationDelegate implements WKNavigationDelegate.
            // setNavigationDelegate takes Option<&ProtocolObject<dyn WKNavigationDelegate>>.
            // We hold a strong Retained here; WKWebView holds weak ref internally.
            let nav_delegate = BuffrNavigationDelegate::new(id, Arc::clone(&state), mtm);
            unsafe {
                // ProtocolObject::from_ref coerces &BuffrNavigationDelegate to
                // &ProtocolObject<dyn WKNavigationDelegate>. Confirmed:
                // objc2/src/runtime/protocol_object.rs:88.
                web_view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*nav_delegate)));
            }

            // Install UI delegate.
            let ui_delegate = BuffrUiDelegate::new(mtm);
            unsafe {
                web_view.setUIDelegate(Some(ProtocolObject::from_ref(&*ui_delegate)));
            }

            // Navigate to the initial URL.
            let ns_url = nsurl_from_str(url)
                .ok_or_else(|| WebKitCocoaError::InitFailed(format!("invalid URL: {url}")))?;
            unsafe {
                // NSURLRequest::requestWithURL confirmed: NSURLRequest.rs:252-254.
                // Returns Retained<NSURLRequest> (not Option).
                let req = NSURLRequest::requestWithURL(&ns_url);
                // WKWebView::loadRequest confirmed: WKWebView.rs:260-262.
                // Returns Option<Retained<WKNavigation>>; we discard it.
                let _ = web_view.loadRequest(&req);
            }

            tracing::info!("webkit-cocoa runtime: opened tab {id:?} → {url}");
            Ok(TabEntry {
                id,
                web_view,
                _nav_delegate: nav_delegate,
                _ui_delegate: ui_delegate,
                _hint_handler: hint_handler,
                url: url.to_owned(),
                title: String::new(),
                is_loading: true,
                zoom: 1.0,
            })
        }

        /// Resize the WKWebView's frame.
        ///
        /// `NSView::setFrame` confirmed: NSView.rs:410-412.
        /// WKWebView inherits NSView; frame changes are honoured even off-screen.
        ///
        /// # TODO: off-screen frame fidelity
        ///
        /// Apple's documentation does not guarantee that WKWebView will re-render
        /// at the new frame size without being part of a live view hierarchy.
        /// In practice (macOS 11+) it does. If snapshots come back at the old
        /// size after resize, attach the view to an off-screen NSWindow.
        pub(crate) fn resize(&self, width: u32, height: u32) {
            use objc2_app_kit::NSView;
            use objc2_foundation::{NSPoint, NSRect, NSSize};
            let frame = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: width as f64,
                    height: height as f64,
                },
            };
            // Cast: WKWebView → NSView via Deref chain.
            // NSView::setFrame is safe (confirmed: NSView.rs:410-412).
            (self.web_view.as_ref() as &NSView).setFrame(frame);
        }

        /// Navigate to a new URL.
        ///
        /// `WKWebView::loadRequest` confirmed: WKWebView.rs:260-262.
        pub(crate) fn navigate(&self, url: &str) {
            let Some(ns_url) = nsurl_from_str(url) else {
                tracing::warn!("webkit-cocoa runtime: invalid URL: {url}");
                return;
            };
            unsafe {
                let req = NSURLRequest::requestWithURL(&ns_url);
                let _ = self.web_view.loadRequest(&req);
            }
        }

        /// Go back one step in history.
        ///
        /// `WKWebView::goBack` confirmed: WKWebView.rs:463-465.
        /// Returns `Option<Retained<WKNavigation>>`; we discard it.
        pub(crate) fn go_back(&self) {
            unsafe {
                let _ = self.web_view.goBack();
            }
        }

        /// Go forward one step in history.
        ///
        /// `WKWebView::goForward` confirmed: WKWebView.rs:472-474.
        pub(crate) fn go_forward(&self) {
            unsafe {
                let _ = self.web_view.goForward();
            }
        }

        /// Reload the current page.
        ///
        /// `WKWebView::reload` confirmed: WKWebView.rs:480-482.
        pub(crate) fn reload(&self) {
            unsafe {
                let _ = self.web_view.reload();
            }
        }

        /// Stop loading.
        ///
        /// `WKWebView::stopLoading` confirmed: WKWebView.rs:493-496.
        /// Returns `()`, unlike goBack/reload which return `Option<WKNavigation>`.
        pub(crate) fn stop(&self) {
            unsafe {
                self.web_view.stopLoading();
            }
        }

        /// Whether the back-stack is non-empty.
        ///
        /// `WKWebView::canGoBack` confirmed: WKWebView.rs:438-440.
        pub(crate) fn can_go_back(&self) -> bool {
            unsafe { self.web_view.canGoBack() }
        }

        /// Whether the forward-stack is non-empty.
        ///
        /// `WKWebView::canGoForward` confirmed: WKWebView.rs:453-456.
        pub(crate) fn can_go_forward(&self) -> bool {
            unsafe { self.web_view.canGoForward() }
        }

        /// Evaluate `code` in the WKWebView's JS context (fire-and-forget).
        ///
        /// API: `WKWebView::evaluateJavaScript:completionHandler:`.
        /// Source: objc2-web-kit-0.3.2/src/generated/WKWebView.rs:499-505.
        /// The completion handler is `None` — matching the trait's fire-and-forget
        /// contract (no synchronous return value is surfaced).
        ///
        /// TODO(phase-c): thread the completion handler through to surface errors
        ///   via `tracing::warn!` if the script evaluation fails.
        pub(crate) fn eval_js(&self, code: &str) {
            let ns_code = NSString::from_str(code);
            // SAFETY: evaluateJavaScript:completionHandler: is a well-known
            // WKWebView ObjC method. Called exclusively on the GCD main queue.
            unsafe {
                self.web_view
                    .evaluateJavaScript_completionHandler(&ns_code, None);
            }
        }

        /// Set the page zoom factor on the WKWebView.
        ///
        /// API: `WKWebView::setPageZoom` (macOS 11+).
        /// Source: objc2-web-kit-0.3.2/src/generated/WKWebView.rs:809-811
        ///   `#[unsafe(method(setPageZoom:))]`
        ///   `pub unsafe fn setPageZoom(&self, page_zoom: CGFloat)`
        ///
        /// The companion read accessor `pageZoom()` returns the same value.
        pub(crate) fn set_zoom(&mut self, level: f64) {
            let clamped = level.clamp(0.25, 5.0);
            // SAFETY: setPageZoom is a standard WKWebView ObjC method (macOS 11+).
            // Called exclusively on the GCD main queue.
            unsafe {
                self.web_view.setPageZoom(clamped);
            }
            self.zoom = clamped;
        }

        /// Request an OSR snapshot of this tab and write BGRA into `frame`.
        ///
        /// Fires asynchronously; the completion handler lands on the main queue.
        pub(crate) fn request_snapshot(&self, frame: SharedOsrFrame, view: SharedOsrViewState) {
            super::super::osr::macos::request_snapshot(&self.web_view, frame, view);
        }
    }

    // ── NSURL helper ─────────────────────────────────────────────────────────

    /// Construct an `NSURL` from a `&str`.
    ///
    /// `NSString::from_str` confirmed: objc2-foundation-0.3.2/src/string.rs:113.
    /// `NSURL::URLWithString` confirmed: NSURL.rs:1178-1181.
    /// Returns `Option<Retained<NSURL>>` — nil if the string is malformed.
    fn nsurl_from_str(s: &str) -> Option<Retained<NSURL>> {
        let ns = NSString::from_str(s);
        // URLWithString is safe (no `unsafe` in objc2-foundation binding).
        NSURL::URLWithString(&ns)
    }

    // ── BuffrNavigationDelegate ───────────────────────────────────────────────
    //
    // Implements WKNavigationDelegate to track URL / title / load state.
    //
    // `define_class!` is the correct macro in objc2 0.6.
    // `declare_class!` now emits compile_error (objc2-0.6.4/src/macros/define_class.rs:504).
    //
    // New syntax:
    //   #[unsafe(super(NSObject))]   — replaces `type Super = NSObject`
    //   #[thread_kind = MainThreadOnly] — replaces `type Mutability = MainThreadOnly`
    //   #[ivars = NavDelegateIvars]  — replaces `impl DeclaredClass { type Ivars = … }`
    //
    // Protocol implementation blocks use `unsafe impl Protocol for Class`.
    // Method attributes use `#[unsafe(method(selector:))]` pattern.
    //
    // Selector names confirmed against WKNavigationDelegate.rs:
    //   webView:didStartProvisionalNavigation:  → line ~220
    //   webView:didCommitNavigation:            → line ~261
    //   webView:didFinishNavigation:            → line ~278
    //   webView:didFailNavigation:withError:    → line ~296
    //   webView:didFailProvisionalNavigation:withError: → line ~240

    /// Ivars stored inside `BuffrNavigationDelegate` Objective-C instances.
    pub struct NavDelegateIvars {
        tab_id: TabId,
        state: Arc<Mutex<EngineState>>,
    }

    define_class!(
        /// Navigation delegate that writes URL/title/load into `EngineState`.
        ///
        /// Inherits NSObject. MainThreadOnly because WKNavigationDelegate callbacks
        /// arrive on the main thread and we access WKWebView properties directly.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = NavDelegateIvars]
        #[name = "BuffrNavigationDelegate"]
        pub struct BuffrNavigationDelegate;

        // NSObjectProtocol required by WKNavigationDelegate's bound.
        unsafe impl NSObjectProtocol for BuffrNavigationDelegate {}

        /// WKNavigationDelegate — track load progress.
        ///
        /// All method selectors confirmed against:
        /// objc2-web-kit-0.3.2/src/generated/WKNavigationDelegate.rs
        unsafe impl WKNavigationDelegate for BuffrNavigationDelegate {
            /// webView:didStartProvisionalNavigation:
            /// Called when main-frame navigation starts.
            /// Confirmed selector + signature: WKNavigationDelegate.rs ~line 220.
            #[unsafe(method(webView:didStartProvisionalNavigation:))]
            #[allow(non_snake_case)]
            unsafe fn webView_didStartProvisionalNavigation(
                &self,
                web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
            ) {
                let url = unsafe { get_url_string(web_view) };
                tracing::debug!("webkit-cocoa nav: didStartProvisionalNavigation url={url}");
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.url = url;
                        tab.is_loading = true;
                    }
                    st.sync_loading_active();
                }
            }

            /// webView:didCommitNavigation:
            /// Called when content starts arriving for the main frame.
            /// Confirmed selector + signature: WKNavigationDelegate.rs ~line 261.
            #[unsafe(method(webView:didCommitNavigation:))]
            #[allow(non_snake_case)]
            unsafe fn webView_didCommitNavigation(
                &self,
                web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
            ) {
                let url = unsafe { get_url_string(web_view) };
                tracing::debug!("webkit-cocoa nav: didCommitNavigation url={url}");
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.url = url;
                        tab.is_loading = true;
                    }
                    st.sync_loading_active();
                }
            }

            /// webView:didFinishNavigation:
            /// Called when main-frame navigation completes.
            /// Confirmed selector + signature: WKNavigationDelegate.rs ~line 278.
            #[unsafe(method(webView:didFinishNavigation:))]
            #[allow(non_snake_case)]
            unsafe fn webView_didFinishNavigation(
                &self,
                web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
            ) {
                let url = unsafe { get_url_string(web_view) };
                let title = unsafe { get_title_string(web_view) };
                tracing::debug!("webkit-cocoa nav: didFinishNavigation url={url} title={title}");
                let tab_id = self.ivars().tab_id;
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.url = url.clone();
                        tab.title = title.clone();
                        tab.is_loading = false;
                    }
                    st.sync_loading_active();

                    // ── Favicon heuristic ────────────────────────────────────────
                    // WKWebView does not expose a public favicon API. We push a
                    // navigation-completion sentinel using the `/favicon.ico`
                    // convention.  Pixel data is intentionally left empty
                    // (width=0, height=0, pixels=[]) — the apps layer can use the
                    // tab URL to initiate a separate fetch if desired.
                    //
                    // TODO(phase-d): perform an async URLSession fetch of
                    //   `<url_origin>/favicon.ico`, decode to BGRA, and push a
                    //   FaviconUpdate with real pixel data.
                    if let Ok(mut fav) = st.favicon_updates.lock() {
                        fav.push(buffr_engine::FaviconUpdate {
                            browser_id: tab_id.0 as i32,
                            width: 0,
                            height: 0,
                            pixels: Vec::new(),
                        });
                    }

                    // ── History recording ────────────────────────────────────────
                    //
                    // Record a visit for this navigation completion. `record_visit`
                    // handles URL canonicalisation, skip-scheme filtering, and
                    // dedupe internally — we just pass what we have. The title may
                    // be empty for pages that set it asynchronously; a future
                    // `on_title_change` equivalent can call `update_latest_title`.
                    if let Some(history) = &st.history {
                        let title_opt = if title.is_empty() {
                            None
                        } else {
                            Some(title.as_str())
                        };
                        if let Err(e) =
                            history.record_visit(&url, title_opt, HistoryTransition::Link)
                        {
                            tracing::debug!(
                                error = %e,
                                url = url.as_str(),
                                "webkit-cocoa: history record_visit failed"
                            );
                        }
                    }
                }
            }

            /// webView:didFailNavigation:withError:
            /// Called when a committed main-frame navigation fails.
            /// Confirmed selector + signature: WKNavigationDelegate.rs ~line 296.
            #[unsafe(method(webView:didFailNavigation:withError:))]
            #[allow(non_snake_case)]
            unsafe fn webView_didFailNavigation_withError(
                &self,
                _web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &NSError,
            ) {
                tracing::warn!("webkit-cocoa nav: didFailNavigation error={:?}", error);
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.is_loading = false;
                    }
                    st.sync_loading_active();
                }
            }

            /// webView:didFailProvisionalNavigation:withError:
            /// Called when provisional navigation fails (e.g. DNS error).
            /// Confirmed selector + signature: WKNavigationDelegate.rs ~line 240.
            #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
            #[allow(non_snake_case)]
            unsafe fn webView_didFailProvisionalNavigation_withError(
                &self,
                _web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &NSError,
            ) {
                tracing::warn!(
                    "webkit-cocoa nav: didFailProvisionalNavigation error={:?}",
                    error
                );
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.is_loading = false;
                    }
                    st.sync_loading_active();
                }
            }
        }
    );

    impl BuffrNavigationDelegate {
        /// Allocate and initialise a new `BuffrNavigationDelegate`.
        ///
        /// Pattern: `Class::alloc(mtm).set_ivars(…)` then `msg_send![super(this), init]`.
        /// - `alloc(mtm)` required because class is `MainThreadOnly`.
        ///   Confirmed: objc2-0.6.4/src/top_level_traits.rs:547.
        /// - `set_ivars` confirmed: objc2-0.6.4/src/rc/allocated_partial_init.rs:200.
        /// - `msg_send![super(this), init]` required for `PartialInit<T>` receivers.
        ///   The `super` keyword routes via `IsSuper` semantics.
        ///   Confirmed: objc2-0.6.4/src/__macro_helpers/retain_semantics.rs:474+.
        fn new(
            tab_id: TabId,
            state: Arc<Mutex<EngineState>>,
            mtm: MainThreadMarker,
        ) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(NavDelegateIvars { tab_id, state });
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    // ── BuffrUiDelegate ───────────────────────────────────────────────────────
    //
    // Implements WKUIDelegate to suppress JS dialogs (alert/confirm/prompt).
    //
    // WKUIDelegate protocol binding confirmed:
    // objc2-web-kit-0.3.2/src/generated/WKUIDelegate.rs:83 — protocol exists.
    //
    // Without overriding the alert/confirm/prompt methods, WKWebView will
    // silently discard JavaScript dialogs when a WKUIDelegate is set but does
    // not implement those selectors. That is the desired behaviour here.
    //
    // # TODO: JS dialog suppression runtime verification
    //
    // If integration tests show that window.alert() blocks the page (e.g. in
    // off-screen mode on an older macOS), add:
    //   webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:
    // and call the completionHandler immediately with no action.
    // The selector and block signature are in WKUIDelegate.rs.

    /// Ivars for `BuffrUiDelegate` — none needed, so use the unit type.
    pub struct UiDelegateIvars;

    define_class!(
        /// UI delegate that suppresses JavaScript dialogs.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = UiDelegateIvars]
        #[name = "BuffrUiDelegate"]
        pub struct BuffrUiDelegate;

        unsafe impl NSObjectProtocol for BuffrUiDelegate {}

        // No WKUIDelegate methods overridden. Default behaviour drops JS dialogs
        // silently when the delegate does not implement them.
        unsafe impl WKUIDelegate for BuffrUiDelegate {}
    );

    impl BuffrUiDelegate {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(UiDelegateIvars);
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    // ── BuffrHintMessageHandler ───────────────────────────────────────────────
    //
    // Implements WKScriptMessageHandler to receive sentinel messages from the
    // hint console bridge injected via HINT_CONSOLE_BRIDGE_JS.
    //
    // When the JS renderer emits `console.log("__buffr_hint__:<json>")`, the
    // bridge calls `window.webkit.messageHandlers.buffrHint.postMessage(msg)`,
    // which invokes `userContentController:didReceiveScriptMessage:` here.
    //
    // We parse the body (an NSString) via `buffr_core::hint::parse_console_event`
    // and write the result into the one-slot `HintEventSink` (Arc<Mutex<Option>>).
    // The engine polls it via `pump_hint_events()`.
    //
    // WKScriptMessageHandler protocol:
    //   Confirmed: objc2-web-kit-0.3.2/src/generated/WKScriptMessageHandler.rs
    //   Selector: userContentController:didReceiveScriptMessage:
    //   Feature gate: WKScriptMessage + WKUserContentController
    //
    // WKScriptMessage::body() returns Retained<AnyObject> (NSString when JS
    // posts a string). We downcast via Retained::downcast::<NSString>().
    //   Confirmed: objc2-web-kit-0.3.2/src/generated/WKScriptMessage.rs:31-34.
    //   Downcast API: objc2-0.6.4/src/rc/retained.rs:325.

    /// Ivars stored inside `BuffrHintMessageHandler`.
    pub struct HintMsgHandlerIvars {
        sink: HintEventSink,
    }

    define_class!(
        /// Script-message handler that receives hint sentinel messages from JS.
        ///
        /// MainThreadOnly because WKScriptMessageHandler callbacks arrive on
        /// the main thread and WKScriptMessage is MainThreadOnly.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = HintMsgHandlerIvars]
        #[name = "BuffrHintMessageHandler"]
        pub struct BuffrHintMessageHandler;

        unsafe impl NSObjectProtocol for BuffrHintMessageHandler {}

        /// WKScriptMessageHandler — receive JS→native hint messages.
        ///
        /// Feature gate: WKScriptMessage + WKUserContentController required by
        /// the binding at WKScriptMessageHandler.rs:15-16.
        unsafe impl WKScriptMessageHandler for BuffrHintMessageHandler {
            #[unsafe(method(userContentController:didReceiveScriptMessage:))]
            #[allow(non_snake_case)]
            unsafe fn userContentController_didReceiveScriptMessage(
                &self,
                _controller: &WKUserContentController,
                message: &WKScriptMessage,
            ) {
                use objc2::msg_send;

                // body() returns Retained<AnyObject>. JS string posts arrive as
                // NSString. We check with isKindOfClass: then cast unchecked.
                // SAFETY: body() is a standard WKScriptMessage accessor; called
                // on the main thread where WKScriptMessage is alive.
                let body: Retained<AnyObject> = unsafe { message.body() };

                // Check if body is an NSString using isKindOfClass:
                // Confirmed: AnyObject::isKindOfClass: (objc2-0.6.4/src/runtime/mod.rs:1385).
                let is_string: bool = unsafe {
                    // Use NSString::class() to get the class pointer.
                    // ClassType::class() confirmed: objc2 top_level_traits.rs.
                    msg_send![&*body, isKindOfClass: NSString::class()]
                };
                if !is_string {
                    tracing::debug!(
                        "webkit-cocoa: buffrHint message body is not NSString (ignored)"
                    );
                    return;
                }
                // SAFETY: we verified isKindOfClass: NSString above.
                let ns_str: Retained<NSString> = unsafe { Retained::cast_unchecked(body) };
                let raw = ns_str.to_string();
                match parse_console_event(&raw) {
                    Some(Ok(event)) => {
                        if let Ok(mut guard) = self.ivars().sink.lock() {
                            *guard = Some(event);
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, raw = raw.as_str(), "webkit-cocoa: malformed hint event");
                    }
                    None => {
                        tracing::debug!(
                            "webkit-cocoa: buffrHint message missing sentinel (ignored)"
                        );
                    }
                }
            }
        }
    );

    impl BuffrHintMessageHandler {
        fn new(mtm: MainThreadMarker, sink: HintEventSink) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(HintMsgHandlerIvars { sink });
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    // ── URL / title helpers ───────────────────────────────────────────────────

    /// Read the current URL from a WKWebView.
    ///
    /// `WKWebView::URL()` → `Option<Retained<NSURL>>`. Confirmed: WKWebView.rs:363-365.
    /// `NSURL::absoluteString()` → `Option<Retained<NSString>>`. Confirmed: NSURL.rs:1264-1266.
    /// `NSString::to_string()` via the `Display` impl in objc2-foundation.
    unsafe fn get_url_string(web_view: &WKWebView) -> String {
        // SAFETY: caller guarantees web_view is a live WKWebView on the main thread.
        unsafe {
            web_view
                .URL()
                .as_ref()
                .and_then(|u| u.absoluteString())
                .map(|s| s.to_string())
                .unwrap_or_default()
        }
    }

    /// Read the current page title from a WKWebView.
    ///
    /// `WKWebView::title()` → `Option<Retained<NSString>>`. Confirmed: WKWebView.rs:348-350.
    unsafe fn get_title_string(web_view: &WKWebView) -> String {
        // SAFETY: caller guarantees web_view is a live WKWebView on the main thread.
        unsafe { web_view.title().map(|s| s.to_string()).unwrap_or_default() }
    }

    // ── TabState (for EngineState below) ─────────────────────────────────────

    /// Thread-safe tab snapshot mirrored from the main-thread `TabEntry`.
    ///
    /// Written by delegate callbacks on the main queue; read from any thread
    /// by the engine behind `Mutex<EngineState>`.
    #[derive(Clone)]
    #[allow(dead_code)] // can_go_back/forward read in Phase D via dispatch override.
    pub(crate) struct TabState {
        pub id: TabId,
        pub url: String,
        pub title: String,
        pub is_loading: bool,
        pub can_go_back: bool,
        pub can_go_forward: bool,
        /// Current zoom level mirrored from the main-thread `TabEntry` after
        /// every `WKWebView::setPageZoom` call.
        /// Source: objc2-web-kit-0.3.2/src/generated/WKWebView.rs
        ///   `pub unsafe fn setPageZoom(&self, page_zoom: CGFloat)` / `pageZoom()`
        pub zoom: f64,
    }
}
