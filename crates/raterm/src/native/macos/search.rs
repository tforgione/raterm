//! The search field, at the top right of the terminal (an `NSSearchField`).

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSAutoresizingMaskOptions, NSColor, NSControl, NSControlTextEditingDelegate,
    NSEventModifierFlags, NSResponder, NSSearchField, NSSearchFieldDelegate, NSTextFieldDelegate,
    NSTextView, NSView,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize};

use crate::native::theme::theme;

/// What the search field tells the view.
pub(super) enum Typed {
    /// The text changed.
    Changed(String),
    /// Enter, Shift+Enter going the other way.
    Enter {
        text: String,
        shift: bool,
    },
    Escape,
}

pub(super) struct Ivars {
    on: Box<dyn Fn(Typed)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RatermSearchDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSControlTextEditingDelegate for Delegate {
        #[unsafe(method(controlTextDidChange:))]
        fn changed(&self, notification: &NSNotification) {
            let text = notification
                .object()
                .and_then(|o| o.downcast::<NSControl>().ok())
                .map(|c| c.stringValue().to_string())
                .unwrap_or_default();
            (self.ivars().on)(Typed::Changed(text));
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn command(&self, control: &NSControl, _view: &NSTextView, command: Sel) -> bool {
            if command == sel!(insertNewline:) {
                let mtm = self.mtm();
                let shift = NSApplication::sharedApplication(mtm)
                    .currentEvent()
                    .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Shift));
                let text = control.stringValue().to_string();
                (self.ivars().on)(Typed::Enter { text, shift });
                true
            } else if command == sel!(cancelOperation:) {
                (self.ivars().on)(Typed::Escape);
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for Delegate {}

    unsafe impl NSSearchFieldDelegate for Delegate {}
);

pub(super) struct Field {
    field: Retained<NSSearchField>,
    _delegate: Retained<Delegate>,
}

const WIDTH: f64 = 260.0;
const MARGIN: f64 = 8.0;

impl Field {
    /// Shows a search field at the top right of `parent` (the grid's view),
    /// and gives it the keyboard.
    pub(super) fn new(parent: *mut c_void, on: Box<dyn Fn(Typed)>) -> Field {
        let mtm = MainThreadMarker::new().expect("the view runs on the main thread");
        // SAFETY: `parent` is the grid's view, alive with the window.
        let parent: &NSView = unsafe { &*(parent as *const NSView) };
        let width = parent.bounds().size.width;
        let frame = NSRect::new(
            NSPoint::new((width - WIDTH - MARGIN).max(0.0), MARGIN),
            NSSize::new(WIDTH, 24.0),
        );
        let field = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), frame);
        // Its top right corner stays where it is (the grid is flipped).
        field.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        // Light or dark, as the terminal's background.
        let bg = theme().bg;
        let luma = 0.299 * ((bg >> 16) & 0xff) as f64
            + 0.587 * ((bg >> 8) & 0xff) as f64
            + 0.114 * (bg & 0xff) as f64;
        let name = unsafe {
            if luma < 128.0 {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            }
        };
        let appearance = NSAppearance::appearanceNamed(name);
        unsafe {
            let _: () = msg_send![&*field, setAppearance: appearance.as_deref()];
        }
        let delegate = mtm.alloc::<Delegate>().set_ivars(Ivars { on });
        let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
        // SAFETY: the delegate lives as long as the field, in `Field`.
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        parent.addSubview(&field);
        let field = Field {
            field,
            _delegate: delegate,
        };
        field.focus();
        field
    }

    /// Gives the field the keyboard, its text selected.
    pub(super) fn focus(&self) {
        if let Some(window) = self.field.window() {
            window.makeFirstResponder(Some(&self.field));
        }
        unsafe { self.field.selectText(None) };
    }

    /// Colors the text red when nothing matches.
    pub(super) fn set_found(&self, found: bool) {
        let color = if found {
            NSColor::controlTextColor()
        } else {
            NSColor::systemRedColor()
        };
        self.field.setTextColor(Some(&color));
    }

    /// Removes the field, giving the keyboard back to `to` (tao's view).
    pub(super) fn remove(&self, to: *mut c_void) {
        if let Some(window) = self.field.window() {
            // SAFETY: tao's view, alive with the window.
            let to: &NSResponder = unsafe { &*(to as *const NSResponder) };
            window.makeFirstResponder(Some(to));
        }
        self.field.removeFromSuperview();
    }
}
