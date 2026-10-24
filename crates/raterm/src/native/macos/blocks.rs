//! Rich blocks as `WKWebView`s: their document is the block alone, without
//! scripts nor network (see `native::blocks` and Spec-macOS).
//!
//! To write on a Mac: add `objc2-web-kit` and `block2` to the macOS
//! dependencies of `Cargo.toml`.

use std::ffi::c_void;

use crate::native::blocks::BlockView;

/// What a block's web view tells the view.
#[expect(dead_code, reason = "made by the web view's delegate, once written")]
pub(super) enum Event {
    /// The document is loaded (`webView:didFinishNavigation:`).
    Loaded,
    /// The document's title changed (see `native::blocks::told`).
    Title(String),
    /// A click: on an image or SVG it opens the image viewer, elsewhere it
    /// clears the other selections (`View::clear_selections`).
    Click,
}

/// The `WKWebView` of a block.
pub(super) struct WebView {}

impl WebView {
    /// Makes a web view in `parent`, above the grid, which then loads its
    /// document (`BlockView::load`). Every web view of raterm shares one
    /// `WKProcessPool` (the default).
    /// `transparent` unless `[blocks] background` is set. It must:
    ///
    /// - disable the document's scripts:
    ///   `WKWebpagePreferences.allowsContentJavaScript = false`, which still
    ///   lets `evaluateJavaScript` run raterm's;
    /// - in `webView:decidePolicyForNavigationAction:decisionHandler:`,
    ///   allow the document itself (`about:` and `data:`), open a link with
    ///   `system::open_url` only when the navigation type is
    ///   `WKNavigationTypeLinkActivated`, and cancel anything else;
    /// - send `Event::Loaded` from `webView:didFinishNavigation:`, each new
    ///   `title` (key-value observing) as `Event::Title`, and clicks as
    ///   `Event::Click`, through `events`;
    /// - send the wheel to the flow (the terminal scrolls, not the block),
    ///   and not let anything be dragged out of it.
    pub(super) fn new(
        _parent: *mut c_void,
        _transparent: bool,
        _events: Box<dyn Fn(Event)>,
    ) -> WebView {
        todo!("macOS: a WKWebView with its WKWebViewConfiguration and WKNavigationDelegate")
    }

    /// The image viewer, full window over the terminal, transparent, closed
    /// on a click (`on_click`) or Escape; it then loads
    /// `blocks::viewer_document`.
    pub(super) fn viewer(_parent: *mut c_void, _on_click: Box<dyn Fn()>) -> WebView {
        todo!("macOS: as new, filling the window, its scripts disabled too")
    }

    /// Removes the web view from its parent.
    pub(super) fn remove(&self) {
        todo!("macOS: removeFromSuperview")
    }
}

impl BlockView for WebView {
    fn load(&self, _html: &str) {
        todo!("macOS: loadHTMLString:baseURL: with no base URL")
    }

    fn place(&self, _x: i32, _y: i32, _width: i32, _height: i32) {
        todo!("macOS: setFrame: in the (flipped) parent's coordinates")
    }

    fn run(&self, _script: &str) {
        todo!("macOS: evaluateJavaScript:completionHandler: with no handler")
    }

    fn copy(&self) {
        todo!("macOS: the copy: action of the web view (NSResponder)")
    }
}
