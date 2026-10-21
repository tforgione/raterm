//! Rich blocks as child web views: their document is the block alone,
//! without scripts nor network, styled like the page's iframes.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use raterm::config::Config;
use raterm::rich::{Kind, Rendered};
use webkit2gtk::{
    LoadEvent, NavigationPolicyDecisionExt, PolicyDecisionExt, PolicyDecisionType, SettingsExt,
    URIRequestExt, WebContext, WebView, WebViewExt,
};

use super::View;
use super::draw::theme;

/// A step of the search in a block.
pub(super) enum Find<'a> {
    Start { query: &'a str, backwards: bool },
    Next,
    Previous,
    Finish,
}

/// Finds the matches in the text of a block (each text node, ignoring case),
/// as `findAll` in app.js; WebKit's own search left slices of old highlights
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

    /// A web view for a block: its document is the block's content alone,
    /// without scripts nor network, links open in the desktop's handler.
    pub(super) fn block_view(&mut self, entry: u64, block: &Rendered) -> WebView {
        let context = self.context.get_or_insert_with(WebContext::new);
        let view = WebView::with_context(context);
        if let Some(settings) = WebViewExt::settings(&view) {
            // Scripts of the document never run; raterm's own (measuring the
            // height) still can, from the API.
            settings.set_enable_javascript_markup(false);
        }
        // Opaque: WebKit repaints only what changes (a selection), and a
        // transparent view lost the rest of the block.
        let bg = |shift: u32| ((theme().bg >> shift) & 0xff) as f64 / 255.0;
        view.set_background_color(&gdk::RGBA::new(bg(16), bg(8), bg(0), 1.0));
        view.set_can_focus(false);
        // One selection at a time: one in a block clears the others.
        let me = self.me.clone();
        view.connect_button_press_event(move |block, ev| {
            if ev.button() != 1 {
                return glib::Propagation::Proceed;
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
            let uri = decision
                .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .and_then(|a| a.request())
                .and_then(|r| r.uri());
            match uri {
                Some(uri) if !uri.starts_with("about:") && !uri.starts_with("data:") => {
                    crate::view::open_url(&uri);
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
                    "document.title = 'h:' + Math.ceil(document.body.getBoundingClientRect().height)",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.heights);
        let me = self.me.clone();
        view.connect_title_notify(move |view| {
            let Some(title) = view.title() else { return };
            if let Some(h) = title.strip_prefix("h:").and_then(|h| h.parse::<f64>().ok())
                && let Some(heights) = heights.upgrade()
            {
                heights.borrow_mut().insert(entry, h);
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
        view.load_html(&document(block, &self.block_style), None);
        view
    }
}

/// The stylesheet of blocks, the same as `richStyle` in app.js.
pub(super) fn block_style(config: &Config, cell_width: f64) -> String {
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
html, body {{ margin: 0; color: var(--fg); background: var(--bg); overflow: hidden; }}
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
img {{ max-width: 100%; }}
body.md > * {{ max-width: {text_width}; margin-left: auto; margin-right: auto; }}
body.md > table {{ max-width: none; margin: 8px auto; }}
* {{ -webkit-user-drag: none; }}
::highlight(rt-search) {{ background-color: var(--c3); color: var(--bg); }}
::highlight(rt-search-current) {{ background-color: var(--c9); color: var(--bg); }}
::selection {{ background: rgba({sel_r}, {sel_g}, {sel_b}, 0.99); color: #{sel_text:06x}; }}
"#,
        fg = t.fg,
        bg = t.bg,
        // Not quite opaque: WebKit makes an opaque selection color translucent.
        sel_r = t.selection >> 16,
        sel_g = t.selection >> 8 & 0xff,
        sel_b = t.selection & 0xff,
        sel_text = t.selection_text,
        size = config.blocks.font_size,
        family = css_fonts(&config.blocks.font_family, "sans-serif"),
        mono = css_fonts(&config.font.family, "monospace"),
    )
}

/// A CSS font-family from configured names, quoted (`fontFamily` in app.js).
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

/// The document of a block, as the page's iframes have it: no network, no
/// script.
fn document(block: &Rendered, style: &str) -> String {
    let body = match block.kind {
        Kind::Img | Kind::Svg => format!(r#"<img src="{}">"#, block.content),
        _ => block.content.clone(),
    };
    let kind = format!("{:?}", block.kind).to_lowercase();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
<style>{style}</style></head><body class="{kind}">{body}</body></html>"#
    )
}
