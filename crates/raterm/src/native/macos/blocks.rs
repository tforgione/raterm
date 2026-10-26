//! Rich blocks as `WKWebView`s: their document is the block alone, without
//! scripts nor network (see `native::blocks` and Spec-macOS).

use std::ffi::c_void;
use std::rc::Rc;

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSEvent, NSEventModifierFlags, NSView};
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    NSKeyValueObservingOptions, NSNumber, NSPoint, NSRect, NSSize, NSString, ns_string,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationType, WKWebView, WKWebViewConfiguration,
};

use crate::native::blocks::{BlockView, CLEAR_SELECTION};
use crate::native::system::open_url;

/// What a block's web view tells the view.
pub(super) enum Event {
    /// The document is loaded (`webView:didFinishNavigation:`).
    Loaded,
    /// The document's title changed (see `native::blocks::told`).
    Title(String),
    /// A click: on an image or SVG it opens the image viewer, elsewhere it
    /// clears the other selections (`View::clear_selections`).
    Click,
}

type Events = Rc<dyn Fn(Event)>;

pub(super) struct ViewIvars {
    events: Option<Events>,
    /// The image viewer's: closes it.
    on_click: Option<Box<dyn Fn()>>,
}

define_class!(
    // A block's web view: it never takes the keyboard from the terminal,
    // and gives it the wheel.
    #[unsafe(super(WKWebView, NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RatermBlockView"]
    #[ivars = ViewIvars]
    struct BlockWebView;

    impl BlockWebView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            false
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let ivars = self.ivars();
            if let Some(on_click) = &ivars.on_click {
                return on_click();
            }
            // A click starts a new selection, as in the grid, rather than a
            // drag of the selected text: the script reaches the web process
            // before the click does. Shift extends the selection instead.
            if event.clickCount() == 1 && !event.modifierFlags().contains(NSEventModifierFlags::Shift) {
                run(self, CLEAR_SELECTION);
            }
            if let Some(events) = ivars.events.clone() {
                events(Event::Click);
            }
            unsafe { msg_send![super(self), mouseDown: event] }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // The wheel scrolls the terminal, over blocks too: tao's view is
            // the grid's parent. The viewer keeps it.
            if self.ivars().on_click.is_some() {
                return;
            }
            if let Some(tao) = unsafe { self.superview().and_then(|grid| grid.superview()) } {
                unsafe { msg_send![&*tao, scrollWheel: event] }
            }
        }
    }
);

pub(super) struct DelegateIvars {
    events: Option<Events>,
}

define_class!(
    // Allows the document itself only, opens links on a click, and tells
    // the view when the document is loaded and when its title changes.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RatermBlockDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl WKNavigationDelegate for Delegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide(
            &self,
            _web_view: &WKWebView,
            action: &WKNavigationAction,
            handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let url = unsafe { action.request().URL() }
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string())
                .unwrap_or_default();
            if url.starts_with("about:") || url.starts_with("data:") {
                return handler.call((WKNavigationActionPolicy::Allow,));
            }
            // Without the iframe sandbox, a block could navigate on its own
            // (`<meta http-equiv="refresh">`): only clicks open.
            if unsafe { action.navigationType() } == WKNavigationType::LinkActivated {
                open_url(&url);
            }
            handler.call((WKNavigationActionPolicy::Cancel,));
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn finished(&self, _web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            if let Some(events) = self.ivars().events.clone() {
                events(Event::Loaded);
            }
        }
    }

    impl Delegate {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(
            &self,
            _path: Option<&NSString>,
            object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            _context: *mut c_void,
        ) {
            let Some(view) = object.and_then(|o| o.downcast_ref::<WKWebView>()) else {
                return;
            };
            let title = unsafe { view.title() }.map(|t| t.to_string());
            if let (Some(title), Some(events)) = (title, self.ivars().events.clone()) {
                events(Event::Title(title));
            }
        }
    }
);

fn run(view: &WKWebView, script: &str) {
    unsafe { view.evaluateJavaScript_completionHandler(&NSString::from_str(script), None) };
}

/// The `WKWebView` of a block.
pub(super) struct WebView {
    view: Retained<BlockWebView>,
    delegate: Retained<Delegate>,
}

impl WebView {
    /// Makes a web view in `parent`, above the grid, which then loads its
    /// document (`BlockView::load`). Every web view of raterm shares the
    /// default `WKProcessPool`. Transparent, unless blocks are opaque in
    /// the color `opaque`.
    pub(super) fn new(
        parent: *mut c_void,
        opaque: Option<u32>,
        events: Box<dyn Fn(Event)>,
    ) -> WebView {
        let events: Events = Rc::from(events);
        make(
            parent,
            ViewIvars {
                events: Some(events.clone()),
                on_click: None,
            },
            opaque,
            DelegateIvars {
                events: Some(events),
            },
        )
    }

    /// The image viewer, full window over the terminal, transparent, closed
    /// on a click (`on_click`) or Escape; it then loads
    /// `blocks::viewer_document`.
    pub(super) fn viewer(parent: *mut c_void, on_click: Box<dyn Fn()>) -> WebView {
        let viewer = make(
            parent,
            ViewIvars {
                events: None,
                on_click: Some(on_click),
            },
            None,
            DelegateIvars { events: None },
        );
        // SAFETY: `parent` is the grid's view, alive with the window.
        let parent: &NSView = unsafe { &*(parent as *const NSView) };
        viewer.view.setFrame(parent.bounds());
        viewer.view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        viewer
    }

    /// Removes the web view from its parent.
    pub(super) fn remove(&self) {
        self.view.removeFromSuperview();
    }
}

fn make(
    parent: *mut c_void,
    ivars: ViewIvars,
    opaque: Option<u32>,
    delegate: DelegateIvars,
) -> WebView {
    let mtm = MainThreadMarker::new().expect("the view runs on the main thread");
    // SAFETY: `parent` is the grid's view, alive with the window.
    let parent: &NSView = unsafe { &*(parent as *const NSView) };
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // Scripts of the document never run; raterm's own (measuring the
    // height, searching) still can, from `evaluateJavaScript`.
    unsafe {
        config
            .defaultWebpagePreferences()
            .setAllowsContentJavaScript(false)
    };
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let this = mtm.alloc::<BlockWebView>().set_ivars(ivars);
    let view: Retained<BlockWebView> =
        unsafe { msg_send![super(this), initWithFrame: frame, configuration: &*config] };
    // Transparent, the grid showing through, unless blocks are opaque.
    unsafe {
        let _: () = msg_send![&*view, setValue: &*NSNumber::new_bool(false), forKey: ns_string!("drawsBackground")];
    }
    if let Some(rgb) = opaque {
        let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
        let color = CGColor::new_srgb(c(16), c(8), c(0), 1.0);
        view.setWantsLayer(true);
        unsafe {
            let layer: *mut AnyObject = msg_send![&*view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setBackgroundColor: &*color];
            }
        }
    }
    let delegate = mtm.alloc::<Delegate>().set_ivars(delegate);
    let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
    unsafe {
        view.setNavigationDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*delegate)));
        let _: () = msg_send![
            &*view,
            addObserver: &*delegate,
            forKeyPath: ns_string!("title"),
            options: NSKeyValueObservingOptions::New,
            context: std::ptr::null_mut::<c_void>()
        ];
    }
    parent.addSubview(&view);
    WebView { view, delegate }
}

impl Drop for WebView {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.view, removeObserver: &*self.delegate, forKeyPath: ns_string!("title")];
            self.view.setNavigationDelegate(None);
        }
        self.view.removeFromSuperview();
    }
}

impl BlockView for WebView {
    fn load(&self, html: &str) {
        unsafe {
            self.view
                .loadHTMLString_baseURL(&NSString::from_str(html), None)
        };
    }

    fn place(&self, x: i32, y: i32, width: i32, height: i32) {
        // The grid, their parent, is flipped: its origin is at the top left.
        let frame = NSRect::new(
            NSPoint::new(x as f64, y as f64),
            NSSize::new(width as f64, height as f64),
        );
        self.view.setFrame(frame);
    }

    fn run(&self, script: &str) {
        run(&self.view, script);
    }

    fn copy(&self) {
        unsafe {
            let _: () = msg_send![&*self.view, copy: None::<&AnyObject>];
        }
    }
}
