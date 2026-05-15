//! Per-tab WKWebView + delegate ownership. Lives on the macOS main thread.
//!
//! # Threading model
//!
//! WKWebView is a main-thread-only object. `Runtime` is `!Send + !Sync`
//! (held in a `Rc<RefCell<Runtime>>` inside the main-thread closure). Commands
//! are sent to the main thread via `dispatch_async(dispatch_get_main_queue(),…)`.
//!
//! The `worker` module submits closures to GCD's main queue; `Runtime` methods
//! execute from inside those closures.
//!
//! # Delegates
//!
//! Each WKWebView gets its own `BuffrNavigationDelegate` (WKNavigationDelegate)
//! and `BuffrUIDelegate` (WKUIDelegate) installed as strong Objective-C
//! references.
//!
//! Delegate methods update a shared `Arc<Mutex<EngineState>>` so the engine
//! thread can read the latest URL / title / load state without blocking on
//! the main queue.
//!
//! # TODO(verify-on-mac)
//!
//! Every call into the objc2 stack is annotated with `// TODO(verify-on-mac):`.

#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Arc, Mutex};

    use objc2::rc::Retained;
    use objc2_foundation::{NSString, NSURL};
    use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

    use buffr_engine::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState, TabId};

    use super::super::error::WebKitCocoaError;
    use super::super::worker::EngineState;

    // ── TabEntry ──────────────────────────────────────────────────────────────

    /// One open tab: owns the WKWebView and its delegates.
    pub(crate) struct TabEntry {
        pub id: TabId,
        /// Strong reference to the WKWebView.
        ///
        /// TODO(verify-on-mac): Confirm that `Retained<WKWebView>` keeps the
        /// view alive correctly and that dropping it sends the -dealloc message.
        pub web_view: Retained<WKWebView>,
        pub url: String,
        pub title: String,
        pub is_loading: bool,
    }

    impl TabEntry {
        /// Create a new WKWebView, configure it, and load `url`.
        ///
        /// # TODO(verify-on-mac)
        ///
        /// - `WKWebViewConfiguration::new()` — confirm constructor name in
        ///   objc2-web-kit 0.3. May need `WKWebViewConfiguration::alloc().init()`.
        /// - `WKWebView::alloc().initWithFrame_configuration(…)` — confirm the
        ///   binding name; Apple selector is `initWithFrame:configuration:`.
        /// - `NSRect::new(0, 0, width, height)` — confirm NSRect constructor.
        /// - `web_view.loadRequest(NSURLRequest::requestWithURL(url))` — confirm
        ///   binding names for NSURLRequest and WKWebView::loadRequest.
        pub(crate) fn open(
            id: TabId,
            url: &str,
            width: u32,
            height: u32,
            state: Arc<Mutex<EngineState>>,
        ) -> Result<Self, WebKitCocoaError> {
            use objc2_foundation::{NSRect, NSURLRequest};

            // TODO(verify-on-mac): Confirm WKWebViewConfiguration constructor.
            let config = unsafe {
                WKWebViewConfiguration::new() // TODO(verify-on-mac)
            };

            let frame = NSRect {
                origin: objc2_foundation::NSPoint { x: 0.0, y: 0.0 },
                size: objc2_foundation::NSSize {
                    width: width as f64,
                    height: height as f64,
                },
            };

            // TODO(verify-on-mac): Confirm WKWebView::initWithFrame_configuration binding.
            let web_view =
                unsafe { WKWebView::alloc().initWithFrame_configuration(frame, &config) };

            // Install navigation delegate.
            // TODO(verify-on-mac): BuffrNavigationDelegate is declared via
            // declare_class! below. Confirm that setNavigationDelegate accepts
            // an &dyn WKNavigationDelegate or a concrete retained type.
            let nav_delegate = BuffrNavigationDelegate::new(id, Arc::clone(&state));
            unsafe {
                // TODO(verify-on-mac): Confirm setNavigationDelegate binding name.
                web_view.setNavigationDelegate(Some(&*nav_delegate));
            }

            // Install UI delegate.
            let ui_delegate = BuffrUiDelegate::new();
            unsafe {
                // TODO(verify-on-mac): Confirm setUIDelegate binding name.
                web_view.setUIDelegate(Some(&*ui_delegate));
            }

            // Navigate to the initial URL.
            let ns_url = nsurl_from_str(url)
                .ok_or_else(|| WebKitCocoaError::InitFailed(format!("invalid URL: {url}")))?;
            unsafe {
                // TODO(verify-on-mac): Confirm NSURLRequest::requestWithURL binding.
                let req = NSURLRequest::requestWithURL(&ns_url);
                // TODO(verify-on-mac): Confirm WKWebView::loadRequest binding.
                web_view.loadRequest(&req);
            }

            tracing::info!("webkit-cocoa runtime: opened tab {id:?} → {url}");
            Ok(TabEntry {
                id,
                web_view,
                url: url.to_owned(),
                title: String::new(),
                is_loading: true,
            })
        }

        /// Resize the WKWebView's frame.
        ///
        /// TODO(verify-on-mac): Confirm `-[NSView setFrame:]` binding in
        /// objc2-app-kit 0.3 and that WKWebView honours frame changes without
        /// a live NSWindow.
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
            unsafe {
                // TODO(verify-on-mac): Confirm NSView::setFrame_ binding.
                (self.web_view.as_ref() as &NSView).setFrame(frame);
            }
        }

        /// Navigate to a new URL.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::loadRequest binding.
        pub(crate) fn navigate(&self, url: &str) {
            let Some(ns_url) = nsurl_from_str(url) else {
                tracing::warn!("webkit-cocoa runtime: invalid URL: {url}");
                return;
            };
            unsafe {
                use objc2_foundation::NSURLRequest;
                let req = NSURLRequest::requestWithURL(&ns_url);
                self.web_view.loadRequest(&req); // TODO(verify-on-mac)
            }
        }

        /// Go back one step.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::goBack binding.
        pub(crate) fn go_back(&self) {
            unsafe {
                self.web_view.goBack(); // TODO(verify-on-mac)
            }
        }

        /// Go forward one step.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::goForward binding.
        pub(crate) fn go_forward(&self) {
            unsafe {
                self.web_view.goForward(); // TODO(verify-on-mac)
            }
        }

        /// Reload the current page.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::reload binding.
        pub(crate) fn reload(&self) {
            unsafe {
                self.web_view.reload(); // TODO(verify-on-mac)
            }
        }

        /// Stop loading.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::stopLoading binding.
        pub(crate) fn stop(&self) {
            unsafe {
                self.web_view.stopLoading(); // TODO(verify-on-mac)
            }
        }

        /// Whether the back-stack is non-empty.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::canGoBack binding (property getter).
        pub(crate) fn can_go_back(&self) -> bool {
            unsafe { self.web_view.canGoBack() } // TODO(verify-on-mac)
        }

        /// Whether the forward-stack is non-empty.
        ///
        /// TODO(verify-on-mac): Confirm WKWebView::canGoForward binding.
        pub(crate) fn can_go_forward(&self) -> bool {
            unsafe { self.web_view.canGoForward() } // TODO(verify-on-mac)
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
    /// TODO(verify-on-mac): Confirm NSURL::URLWithString binding in
    /// objc2-foundation 0.3. If `URLWithString` returns `Option<Retained<NSURL>>`
    /// then the `?` already handles the nil case.
    fn nsurl_from_str(s: &str) -> Option<Retained<NSURL>> {
        let ns = NSString::from_str(s); // TODO(verify-on-mac)
        unsafe {
            NSURL::URLWithString(&ns) // TODO(verify-on-mac): returns Option<Retained<NSURL>>
        }
    }

    // ── BuffrNavigationDelegate ───────────────────────────────────────────────
    //
    // Implements WKNavigationDelegate to track URL / title / load state.
    //
    // TODO(verify-on-mac): Confirm that `declare_class!` is the correct macro
    // from objc2 0.6 for declaring a new Objective-C class with protocol
    // conformances. The macro signature changed between objc2 0.5 and 0.6.
    // Reference: https://docs.rs/objc2/latest/objc2/macro.declare_class.html

    use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, mutability};
    use objc2_web_kit::{WKNavigation, WKNavigationDelegate};

    /// Ivars for `BuffrNavigationDelegate`.
    pub(crate) struct NavDelegateIvars {
        tab_id: TabId,
        state: Arc<Mutex<EngineState>>,
    }

    // TODO(verify-on-mac): Confirm declare_class! macro syntax for objc2 0.6.
    // The pattern below follows https://docs.rs/objc2/0.6/objc2/macro.declare_class.html
    declare_class!(
        /// Navigation delegate that writes URL/title/load into `EngineState`.
        pub struct BuffrNavigationDelegate;

        unsafe impl ClassType for BuffrNavigationDelegate {
            type Super = objc2::runtime::NSObject;
            type Mutability = mutability::MainThreadOnly; // TODO(verify-on-mac): confirm MainThreadOnly vs Mutable
            const NAME: &'static str = "BuffrNavigationDelegate";
        }

        impl DeclaredClass for BuffrNavigationDelegate {
            type Ivars = NavDelegateIvars;
        }

        // TODO(verify-on-mac): Confirm unsafe impl WKNavigationDelegate for the
        // declare_class! macro pattern. The protocol impl block may need
        // `#[objc(protocol)]` or just `unsafe impl WKNavigationDelegate`.
        unsafe impl WKNavigationDelegate for BuffrNavigationDelegate {
            /// Called when navigation has committed (the new document starts loading).
            ///
            /// TODO(verify-on-mac): Confirm selector spelling:
            ///   webView:didCommitNavigation:
            /// and that the Rust method signature matches the generated binding.
            #[objc(optional)]
            #[allow(non_snake_case)]
            unsafe fn webView_didCommitNavigation(
                &self,
                web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
            ) {
                let url = get_url_string(web_view);
                tracing::debug!("webkit-cocoa nav: didCommitNavigation url={url}");
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.url = url;
                        tab.is_loading = true;
                    }
                }
            }

            /// Called when navigation finishes successfully.
            ///
            /// TODO(verify-on-mac): Confirm selector spelling:
            ///   webView:didFinishNavigation:
            #[objc(optional)]
            #[allow(non_snake_case)]
            unsafe fn webView_didFinishNavigation(
                &self,
                web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
            ) {
                let url = get_url_string(web_view);
                let title = get_title_string(web_view);
                tracing::debug!("webkit-cocoa nav: didFinishNavigation url={url} title={title}");
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.url = url;
                        tab.title = title;
                        tab.is_loading = false;
                    }
                }
            }

            /// Called when navigation fails.
            ///
            /// TODO(verify-on-mac): Confirm selector spelling:
            ///   webView:didFailNavigation:withError:
            #[objc(optional)]
            #[allow(non_snake_case)]
            unsafe fn webView_didFailNavigation_withError(
                &self,
                _web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &objc2_foundation::NSError,
            ) {
                tracing::warn!("webkit-cocoa nav: didFailNavigation error={:?}", error);
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.is_loading = false;
                    }
                }
            }

            /// Called when provisional navigation fails.
            ///
            /// TODO(verify-on-mac): Confirm selector spelling:
            ///   webView:didFailProvisionalNavigation:withError:
            #[objc(optional)]
            #[allow(non_snake_case)]
            unsafe fn webView_didFailProvisionalNavigation_withError(
                &self,
                _web_view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &objc2_foundation::NSError,
            ) {
                tracing::warn!(
                    "webkit-cocoa nav: didFailProvisionalNavigation error={:?}",
                    error
                );
                if let Ok(mut st) = self.ivars().state.lock() {
                    if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == self.ivars().tab_id) {
                        tab.is_loading = false;
                    }
                }
            }
        }
    );

    impl BuffrNavigationDelegate {
        fn new(tab_id: TabId, state: Arc<Mutex<EngineState>>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(NavDelegateIvars { tab_id, state });
            // TODO(verify-on-mac): Confirm unsafe msg_send_id!(this, init) pattern
            // for a declare_class! type in objc2 0.6.
            unsafe { msg_send_id![this, init] }
        }
    }

    // ── BuffrUiDelegate ───────────────────────────────────────────────────────
    //
    // Implements WKUIDelegate to suppress JS dialogs (alert/confirm/prompt).
    //
    // TODO(verify-on-mac): Confirm WKUIDelegate protocol binding in
    // objc2-web-kit 0.3.

    use objc2_web_kit::WKUIDelegate;

    pub(crate) struct UiDelegateIvars;

    declare_class!(
        /// UI delegate that suppresses JavaScript dialogs.
        pub struct BuffrUiDelegate;

        unsafe impl ClassType for BuffrUiDelegate {
            type Super = objc2::runtime::NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "BuffrUiDelegate";
        }

        impl DeclaredClass for BuffrUiDelegate {
            type Ivars = UiDelegateIvars;
        }

        unsafe impl WKUIDelegate for BuffrUiDelegate {
            // Phase B: no WKUIDelegate methods overridden.
            // JS alert/confirm/prompt will be silently dropped by the default
            // WKWebView implementation when no delegate methods are provided.
            //
            // TODO(verify-on-mac): If WKWebView shows a modal dialog on
            // window.alert() even without a delegate, add:
            //   webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:
            // and call the completionHandler immediately with no action.
        }
    );

    impl BuffrUiDelegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(UiDelegateIvars);
            unsafe { msg_send_id![this, init] }
        }
    }

    // ── URL / title helpers ───────────────────────────────────────────────────

    /// Read the current URL from a WKWebView.
    ///
    /// TODO(verify-on-mac): Confirm WKWebView::URL returns Option<Retained<NSURL>>
    /// and that NSURL::absoluteString returns NSString in objc2-foundation 0.3.
    unsafe fn get_url_string(web_view: &WKWebView) -> String {
        web_view
            .URL() // TODO(verify-on-mac): property getter binding name
            .as_ref()
            .and_then(|u| u.absoluteString()) // TODO(verify-on-mac)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// Read the current page title from a WKWebView.
    ///
    /// TODO(verify-on-mac): Confirm WKWebView::title returns Option<Retained<NSString>>.
    unsafe fn get_title_string(web_view: &WKWebView) -> String {
        web_view
            .title() // TODO(verify-on-mac): property getter binding name
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    // ── TabState (for EngineState below) ─────────────────────────────────────

    /// Thread-safe tab snapshot mirrored from the main-thread `TabEntry`.
    ///
    /// Written by delegate callbacks on the main queue; read from any thread
    /// by the engine behind `Mutex<EngineState>`.
    #[derive(Clone)]
    pub(crate) struct TabState {
        pub id: TabId,
        pub url: String,
        pub title: String,
        pub is_loading: bool,
        pub can_go_back: bool,
        pub can_go_forward: bool,
    }
}
