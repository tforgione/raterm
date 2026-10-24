//! Rich blocks as child web views: their document is the block alone,
//! without scripts nor network.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use raterm::config::Config;
use raterm::rich::{Kind, Rendered};
use webkit2gtk::{
    LoadEvent, NavigationPolicyDecisionExt, NavigationType, PolicyDecisionExt, PolicyDecisionType,
    SettingsExt, URIRequestExt, WebView, WebViewExt,
};

use super::View;
use crate::native::theme::theme;

/// A step of the search in a block.
pub(super) enum Find<'a> {
    Start { query: &'a str, backwards: bool },
    Next,
    Previous,
    Finish,
}

/// Finds the matches in the text of a block (each text node, ignoring case),
/// with raterm's own script: WebKit's own search left slices of old highlights
/// on screen.
const FIND: &str = "window.rtFind = (() => {
  const needle = QUERY.toLowerCase(), ranges = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); needle && node; node = walker.nextNode()) {
    const text = node.data.toLowerCase();
    for (let i = text.indexOf(needle); i !== -1; i = text.indexOf(needle, i + needle.length)) {
      const range = document.createRange();
      range.setStart(node, i);
      range.setEnd(node, i + needle.length);
      ranges.push(range);
    }
  }
  return { ranges, i: BACKWARDS ? ranges.length - 1 : 0 };
})();";

/// Highlights the matches and the current one, and tells where it is through
/// the title: the flow scrolls to it, the block does not scroll itself. The
/// time makes each answer a new title.
const SHOW: &str = "(() => {
  const f = window.rtFind, n = f ? f.ranges.length : 0;
  if (!n) {
    CSS.highlights.clear();
    return;
  }
  f.i = (f.i + n) % n;
  CSS.highlights.set('rt-search', new Highlight(...f.ranges));
  const current = new Highlight(f.ranges[f.i]);
  current.priority = 1;
  CSS.highlights.set('rt-search-current', current);
  document.title = 'f:' + Math.round(f.ranges[f.i].getBoundingClientRect().top) + ':' + Date.now();
})();";

/// WebKit repaints the text a highlight left, not all of its line: slices
/// of it stayed on screen. Switching the opacity of the document repaints it
/// all.
const REPAINT: &str = "(() => {
  const s = document.documentElement.style;
  s.opacity = s.opacity ? '' : '0.999';
})();";

impl View {
    /// The size of an image block: at most `h` lines and
    /// `[images] max_height`, at most `w` columns.
    fn image_style(&self, block: &Rendered) -> String {
        let (cw, ch) = self.cell;
        let lines = match (block.h, self.max_image_lines) {
            (Some(h), Some(max)) => Some(h.min(max)),
            (h, max) => h.or(max),
        };
        let mut style = String::new();
        if let Some(lines) = lines {
            style += &format!("max-height: {}px;", lines as f64 * ch);
        }
        if let Some(w) = block.w {
            style += &format!("max-width: {}px;", w as f64 * cw);
        }
        style
    }

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
        // SVGs have no natural size worth keeping: they fill the window.
        let size = if image.kind == Kind::Svg {
            "width: 95%; height: 95%; object-fit: contain;"
        } else {
            "max-width: 95%; max-height: 95%;"
        };
        viewer.load_html(
            &format!(
                r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
<style>
html, body {{ margin: 0; height: 100%; overflow: hidden; background: rgba(0, 0, 0, 0.5); }}
body {{ display: flex; align-items: center; justify-content: center; }}
img {{ {size} }}
span {{ position: absolute; top: 8px; right: 12px; font: 28px/1 sans-serif; color: #fff; opacity: 0.8; }}
</style></head><body><span>×</span><img src="{}"></body></html>"#,
                image.content
            ),
            None,
        );
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
        let Some(view) = self.blocks.get(&entry) else {
            return;
        };
        let script = match find {
            Find::Start { query, backwards } => {
                FIND.replace("QUERY", &serde_json::to_string(query).unwrap())
                    .replace("BACKWARDS", &backwards.to_string())
                    + SHOW
            }
            Find::Next => format!("window.rtFind && window.rtFind.i++; {SHOW}"),
            Find::Previous => format!("window.rtFind && window.rtFind.i--; {SHOW}"),
            Find::Finish => "window.rtFind = null; CSS.highlights.clear();".to_owned(),
        } + REPAINT;
        view.evaluate_javascript(&script, None, None, None::<&gtk::gio::Cancellable>, |_| {});
    }

    /// Clears the selection of the grid and those of the blocks, but the
    /// block `keep`.
    pub(super) fn clear_selections(&mut self, keep: Option<u64>) {
        if self.selection.take().is_some() {
            self.area.queue_draw();
        }
        self.block_selected = None;
        for (entry, view) in &self.blocks {
            if Some(*entry) != keep {
                view.evaluate_javascript(
                    "getSelection().removeAllRanges()",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
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
        // Transparent, the background of the grid showing, unless blocks have
        // a color of their own (see Spec-View).
        let bg = |shift: u32| ((background(&self.config) >> shift) & 0xff) as f64 / 255.0;
        let alpha = if self.config.blocks.background.is_some() {
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
                block.evaluate_javascript(
                    "getSelection().removeAllRanges()",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
            if let Some(view) = me.upgrade() {
                let mut view = view.borrow_mut();
                view.clear_selections(Some(entry));
                view.block_selected = Some(entry);
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
                        super::open_url(&uri);
                    }
                    decision.ignore();
                    true
                }
                _ => false,
            }
        });
        // The height comes back through the document's title: reading the
        // result of a script would take the javascriptcore crate.
        let me = self.me.clone();
        view.connect_load_changed(move |view, event| {
            // A block made while it holds the current match searches once
            // loaded.
            if event == LoadEvent::Finished
                && let Some(this) = me.upgrade()
                && let Ok(this) = this.try_borrow()
                && let Some(search) = &this.search
                && search.block == Some(entry)
            {
                this.find_in_block(
                    entry,
                    Find::Start {
                        query: &this.search_entry.text(),
                        backwards: true,
                    },
                );
            }
            if event == LoadEvent::Finished {
                view.evaluate_javascript(
                    // The body: the document is never smaller than the view.
                    // Again when it changes, as a video once it knows its size.
                    // Then the natural width of the widest table, whatever
                    // the width of the view (see View::block_place).
                    "(() => {
                       const natural = t => {
                         const width = t.style.width;
                         t.style.width = 'max-content';
                         const w = t.getBoundingClientRect().width;
                         t.style.width = width;
                         return Math.ceil(w);
                       };
                       const fit = () => document.title =
                         'h:' + Math.ceil(document.body.getBoundingClientRect().height) + ':' +
                         Math.max(0, ...Array.from(document.querySelectorAll('body > table'), natural));
                       new ResizeObserver(fit).observe(document.body);
                       fit();
                     })()",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.heights);
        let widths = Rc::downgrade(&self.table_widths);
        let me = self.me.clone();
        view.connect_title_notify(move |view| {
            let Some(title) = view.title() else { return };
            if let Some(size) = title.strip_prefix("h:")
                && let Some((h, w)) = size.split_once(':')
                && let (Ok(h), Ok(w)) = (h.parse::<f64>(), w.parse::<f64>())
                && let (Some(heights), Some(widths)) = (heights.upgrade(), widths.upgrade())
            {
                heights.borrow_mut().insert(entry, h);
                widths.borrow_mut().insert(entry, w);
                redraw.request();
            }
            if let Some(top) = title
                .strip_prefix("f:")
                .and_then(|f| f.split(':').next()?.parse::<f64>().ok())
                && top >= 0.0
                && let Some(this) = me.upgrade()
                && let Ok(mut this) = this.try_borrow_mut()
            {
                this.reveal_in_block(entry, top);
            }
        });
        view.load_html(&self.document(block), None);
        view
    }
}

/// The background of blocks, as configured, or the terminal's: the color
/// of text drawn on highlights, and of blocks with one.
fn background(config: &Config) -> u32 {
    config.blocks.background.map_or(theme().bg, |c| c.0)
}

/// The stylesheet of blocks; `scale` is that of the zoom.
pub(super) fn block_style(config: &Config, cell_width: f64, scale: f64) -> String {
    let t = theme();
    let vars: String = (0..16)
        .map(|i| format!("--c{i}: #{:06x}; ", t.palette[i]))
        .collect();
    let columns = config.markdown.max_width;
    let text_width = if columns > 0 {
        format!("{}px", columns as f64 * cell_width)
    } else {
        "none".into()
    };
    format!(
        r#":root {{ --fg: #{fg:06x}; --bg: #{bg:06x}; --inline-code: #e8857f; {vars} color-scheme: dark; }}
html, body {{ margin: 0; color: var(--fg); background: {page}; overflow: hidden; }}
body {{ font: {size}pt/1.5 {family}; padding: 4px 0; overflow-x: auto; }}
a {{ color: var(--c4); }}
pre, code {{ font-family: {mono}; font-size: 0.9em; }}
:not(pre) > code {{ color: var(--inline-code); background: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.1); border-radius: 4px; padding: 0.1em 0.35em;
  -webkit-box-decoration-break: clone; box-decoration-break: clone; }}
pre {{ padding: 8px; background: rgba(255, 255, 255, 0.05); overflow-x: auto; }}
.comment {{ color: var(--c8); font-style: italic; }}
.string {{ color: var(--c2); }}
.constant {{ color: var(--c3); }}
.keyword, .storage {{ color: var(--c5); }}
.keyword.operator {{ color: inherit; }}
.entity.name.function, .support.function {{ color: var(--c4); }}
.entity.name.type, .entity.name.class, .support.type, .support.class {{ color: var(--c6); }}
.entity.name.tag {{ color: var(--c1); }}
.entity.other.attribute-name {{ color: var(--c3); }}
table {{ border-collapse: collapse; margin: 8px 0; }}
th, td {{ border: 1px solid var(--c8); padding: 4px 10px; text-align: left; vertical-align: top; }}
th {{ background: rgba(255, 255, 255, 0.08); }}
tbody tr:nth-child(even) {{ background: rgba(255, 255, 255, 0.04); }}
.markdown-alert {{ --alert: #4493f8; --icon: "ℹ️"; border-left: 4px solid var(--alert); padding: 0 1em; margin: 1em auto; }}
.markdown-alert-tip {{ --alert: #3fb950; --icon: "💡"; }}
.markdown-alert-important {{ --alert: #ab7df8; --icon: "❗"; }}
.markdown-alert-warning {{ --alert: #d29922; --icon: "⚠️"; }}
.markdown-alert-caution {{ --alert: #f85149; --icon: "🛑"; }}
.markdown-alert-title {{ color: var(--alert); font-weight: bold; }}
.markdown-alert-title::before {{ content: var(--icon) " "; }}
img {{ max-width: 100%; }}
body.img img, body.svg img {{ display: block; object-fit: contain; object-position: left top; cursor: zoom-in; }}
body.svg img {{ width: 100%; }}
video, audio {{ display: block; max-width: 100%; }}
body.md > * {{ max-width: {text_width}; margin-left: auto; margin-right: auto; }}
body.md > table {{ max-width: none; margin: 8px auto; }}
* {{ -webkit-user-drag: none; }}
::highlight(rt-search) {{ background-color: var(--c3); color: var(--bg); }}
::highlight(rt-search-current) {{ background-color: var(--c9); color: var(--bg); }}
::selection {{ background: rgba({sel_r}, {sel_g}, {sel_b}, 0.99); color: #{sel_text:06x}; }}
"#,
        fg = t.fg,
        bg = background(config),
        page = if config.blocks.background.is_some() {
            "var(--bg)"
        } else {
            "transparent"
        },
        // Not quite opaque: WebKit makes an opaque selection color translucent.
        sel_r = t.selection >> 16,
        sel_g = t.selection >> 8 & 0xff,
        sel_b = t.selection & 0xff,
        sel_text = t.selection_text,
        size = config.blocks.font_size as f64 * scale,
        family = css_fonts(&config.blocks.font_family, "sans-serif"),
        mono = css_fonts(&config.font.family, "monospace"),
    )
}

/// A CSS font-family from configured names, quoted: unquoted, a name like
/// `Source Serif 4` is invalid CSS.
pub(super) fn css_fonts(names: &str, fallback: &str) -> String {
    const GENERIC: [&str; 6] = [
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
    ];
    names
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| {
            if GENERIC.contains(&n) {
                n.to_owned()
            } else {
                format!("{n:?}")
            }
        })
        .chain([fallback.to_owned()])
        .collect::<Vec<_>>()
        .join(", ")
}

impl View {
    /// The document of a block: no network,
    /// no script, but raterm's own media server for local files.
    fn document(&self, block: &Rendered) -> String {
        let size = self.image_style(block);
        // Local files go through the media server, the rest are data: URLs.
        let src = match block.content.strip_prefix("media/") {
            Some(n) => format!("{}{n}", self.media.base),
            None => block.content.clone(),
        };
        let body = match block.kind {
            Kind::Img | Kind::Svg => format!(r#"<img src="{src}" style="{size}">"#),
            Kind::Video => {
                format!(r#"<video controls preload="metadata" src="{src}" style="{size}"></video>"#)
            }
            Kind::Audio => format!(r#"<audio controls preload="metadata" src="{src}"></audio>"#),
            _ => block.content.clone(),
        };
        let kind = format!("{:?}", block.kind).to_lowercase();
        format!(
            r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; media-src data: {}; style-src 'unsafe-inline'">
<style>{}</style></head><body class="{kind}">{body}</body></html>"#,
            self.media.origin, self.block_style
        )
    }
}
