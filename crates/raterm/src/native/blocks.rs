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

impl View {
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
        view.connect_load_changed(|view, event| {
            if event == LoadEvent::Finished {
                view.evaluate_javascript(
                    // The body: the document is never smaller than the view.
                    "document.title = Math.ceil(document.body.getBoundingClientRect().height)",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.heights);
        view.connect_title_notify(move |view| {
            let height = view.title().and_then(|t| t.parse::<f64>().ok());
            if let (Some(h), Some(heights)) = (height, heights.upgrade()) {
                heights.borrow_mut().insert(entry, h);
                redraw.request();
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
"#,
        fg = t.fg,
        bg = t.bg,
        size = config.blocks.font_size,
        family = css_fonts(&config.blocks.font_family, "sans-serif"),
        mono = css_fonts(&config.font.family, "monospace"),
    )
}

/// A CSS font-family from configured names, quoted (`fontFamily` in app.js).
fn css_fonts(names: &str, fallback: &str) -> String {
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
