//! Rich blocks as child `WebKitWebView`s: their document is the block alone,
//! without scripts nor network (see `native::blocks`).

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use raterm::rich::{Kind, Rendered};
use webkit2gtk::{
    LoadEvent, NavigationPolicyDecisionExt, NavigationType, PolicyDecisionExt, PolicyDecisionType,
    SettingsExt, URIRequestExt, WebView, WebViewExt,
};

use super::View;
use crate::native::blocks::{
    BlockView, CLEAR_SELECTION, MEASURE, Told, background, find_script, told, viewer_document,
};
use crate::native::flow::Find;
use crate::native::system::open_url;

impl BlockView for WebView {
    fn load(&self, html: &str) {
        self.load_html(html, None);
    }

    fn place(&self, _x: i32, _y: i32, width: i32, height: i32) {
        // The position is the `GtkFixed`'s, which holds the web view.
        self.set_size_request(width, height);
    }

    fn run(&self, script: &str) {
        self.evaluate_javascript(script, None, None, None::<&gtk::gio::Cancellable>, |_| {});
    }

    fn copy(&self) {
        self.execute_editing_command("Copy");
    }
}

impl View {
    /// Shows an image full window over the terminal, until a click or Escape.
    /// A new web view each time: one kept hidden showed the previous image
    /// for a frame, before painting the next one.
    pub(super) fn open_viewer(&mut self, image: &Rendered) {
        self.close_viewer();
        let viewer = self.web_view();
        if let Some(settings) = WebViewExt::settings(&viewer) {
            settings.set_enable_javascript_markup(false);
        }
        viewer.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        viewer.set_can_focus(false);
        let me = self.me.clone();
        viewer.connect_button_press_event(move |_, _| {
            if let Some(view) = me.upgrade() {
                view.borrow_mut().close_viewer();
            }
            glib::Propagation::Stop
        });
        viewer.connect_scroll_event(|_, _| glib::Propagation::Stop);
        self.overlay.add_overlay(&viewer);
        viewer.load(&viewer_document(image));
        viewer.show();
        self.viewer = Some(viewer);
    }

    pub(super) fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            self.overlay.remove(&viewer);
        }
        self.area.grab_focus();
    }

    pub(super) fn viewer_open(&self) -> bool {
        self.viewer.as_ref().is_some_and(|v| v.is_visible())
    }

    /// Searches in the web view of a block, which highlights the matches.
    pub(super) fn find_in_block(&self, entry: u64, find: Find) {
        if let Some(view) = self.blocks.get(&entry) {
            view.run(&find_script(&find));
        }
    }

    /// Clears the selection of the grid and those of the blocks, but the
    /// block `keep`.
    pub(super) fn clear_selections(&mut self, keep: Option<u64>) {
        if self.flow.selection.take().is_some() {
            self.area.queue_draw();
        }
        self.flow.block_selected = None;
        for (entry, view) in &self.blocks {
            if Some(*entry) != keep {
                view.run(CLEAR_SELECTION);
            }
        }
    }

    /// A new web view, in the web process of the others: one process each
    /// took about 200 MiB per block in view.
    fn web_view(&mut self) -> WebView {
        let root = self.root.get_or_insert_with(WebView::new);
        // Not `WebView::with_related_view`, which takes one reference too
        // many in webkit2gtk 2.0.2 (GLib criticals on unref).
        WebView::builder().related_view(root).build()
    }

    /// A web view for a block: its document is the block's content alone,
    /// without scripts nor network, links open in the desktop's handler.
    pub(super) fn block_view(&mut self, entry: u64, block: &Rendered) -> WebView {
        let view = self.web_view();
        if let Some(settings) = WebViewExt::settings(&view) {
            // Scripts of the document never run; raterm's own (measuring the
            // height) still can, from the API.
            settings.set_enable_javascript_markup(false);
        }
        // Transparent, the background of the grid showing, unless blocks are
        // opaque (see Spec-View).
        let bg = |shift: u32| ((background(&self.flow.config) >> shift) & 0xff) as f64 / 255.0;
        let alpha = if self.flow.config.opaque_blocks() {
            1.0
        } else {
            0.0
        };
        view.set_background_color(&gdk::RGBA::new(bg(16), bg(8), bg(0), alpha));
        view.set_can_focus(false);
        // One selection at a time: one in a block clears the others.
        let me = self.me.clone();
        let image = matches!(block.kind, Kind::Img | Kind::Svg).then(|| block.clone());
        view.connect_button_press_event(move |block, ev| {
            if ev.button() != 1 {
                return glib::Propagation::Proceed;
            }
            if let Some(image) = &image
                && let Some(view) = me.upgrade()
            {
                view.borrow_mut().open_viewer(image);
                return glib::Propagation::Stop;
            }
            // A click starts a new selection, as in the grid, rather than a
            // drag of the selected text: the script reaches the web process
            // before the click does. Shift extends the selection instead.
            if ev.event_type() == gdk::EventType::ButtonPress
                && !ev.state().contains(gdk::ModifierType::SHIFT_MASK)
            {
                block.run(CLEAR_SELECTION);
            }
            if let Some(view) = me.upgrade() {
                let mut view = view.borrow_mut();
                view.clear_selections(Some(entry));
                view.flow.block_selected = Some(entry);
            }
            glib::Propagation::Proceed
        });
        // Nothing is dragged out of a block (images and links neither).
        view.connect_drag_begin(|_, context| context.drag_cancel());
        // The wheel scrolls the terminal, over blocks too.
        let me = self.me.clone();
        view.connect_scroll_event(move |_, ev| {
            if let Some(view) = me.upgrade() {
                view.borrow_mut().wheel(ev);
            }
            glib::Propagation::Stop
        });
        view.connect_decide_policy(|_, decision, kind| {
            if kind != PolicyDecisionType::NavigationAction {
                return false;
            }
            let action = decision
                .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action());
            let clicked = action
                .as_ref()
                .is_some_and(|a| a.navigation_type() == NavigationType::LinkClicked);
            match action.and_then(|a| a.request()).and_then(|r| r.uri()) {
                Some(uri) if !uri.starts_with("about:") && !uri.starts_with("data:") => {
                    // Without the iframe sandbox, a block could navigate on
                    // its own (`<meta http-equiv="refresh">`): only clicks open.
                    if clicked {
                        open_url(&uri);
                    }
                    decision.ignore();
                    true
                }
                _ => false,
            }
        });
        let me = self.me.clone();
        view.connect_load_changed(move |view, event| {
            // A block made while it holds the current match searches once
            // loaded.
            if event == LoadEvent::Finished
                && let Some(this) = me.upgrade()
                && let Ok(this) = this.try_borrow()
                && let Some(search) = &this.flow.search
                && search.block == Some(entry)
            {
                this.find_in_block(
                    entry,
                    Find::Start {
                        query: this.search_entry.text().to_string(),
                        backwards: true,
                    },
                );
            }
            if event == LoadEvent::Finished {
                view.run(MEASURE);
            }
        });
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.flow.heights);
        let widths = Rc::downgrade(&self.flow.table_widths);
        let me = self.me.clone();
        view.connect_title_notify(move |view| {
            let Some(title) = view.title() else { return };
            let (Some(heights), Some(widths)) = (heights.upgrade(), widths.upgrade()) else {
                return;
            };
            match told(entry, &title, &heights, &widths) {
                Some(Told::Measured) => redraw.request(),
                Some(Told::Found(top)) => {
                    if let Some(this) = me.upgrade()
                        && let Ok(mut this) = this.try_borrow_mut()
                    {
                        this.reveal_in_block(entry, top);
                    }
                }
                None => {}
            }
        });
        view.load(&self.flow.document(block, &self.media, &self.block_style));
        view
    }
}
