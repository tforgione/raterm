//! The menu bar: macOS applications quit with Cmd+Q, close a window with
//! Cmd+W, and hide or minimize with Cmd+H and Cmd+M; tao makes no menu.

use objc2::runtime::Sel;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

/// Installs the application's menu and the Window menu.
pub(super) fn install() {
    let mtm = MainThreadMarker::new().expect("the view runs on the main thread");
    let item = |title: &str, action: Sel, key: &str| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    let submenu = |title: &str, items: &[objc2::rc::Retained<NSMenuItem>]| {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for item in items {
            menu.addItem(item);
        }
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        (holder, menu)
    };
    let (app, _) = submenu(
        "raterm",
        &[
            item("Hide raterm", sel!(hide:), "h"),
            NSMenuItem::separatorItem(mtm),
            item("Quit raterm", sel!(terminate:), "q"),
        ],
    );
    let (window, window_menu) = submenu(
        "Window",
        &[
            item("Minimize", sel!(performMiniaturize:), "m"),
            item("Close", sel!(performClose:), "w"),
        ],
    );
    let bar = NSMenu::new(mtm);
    bar.addItem(&app);
    bar.addItem(&window);
    let application = NSApplication::sharedApplication(mtm);
    application.setMainMenu(Some(&bar));
    application.setWindowsMenu(Some(&window_menu));
}
