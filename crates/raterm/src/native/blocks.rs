//! Rich blocks, whatever their web views are (see Spec-View and
//! Spec-Rich-output): their document is the block alone, without scripts nor
//! network, and raterm's own scripts measure them and search them. A
//! backend's web views are `BlockView`s.

use std::cell::RefCell;
use std::collections::HashMap;

use raterm::config::Config;
use raterm::rich::{Kind, Rendered};

use super::flow::{Find, Flow};
use super::theme::theme;

/// The web view of a block, as a backend makes it (`WebKitWebView` on
/// Linux, `WKWebView` on macOS). Besides these, the backend must:
///
/// - never run the document's own scripts, but let raterm's run;
/// - open a link only on a click (`super::system::open_url`), and refuse
///   any other navigation but the document itself;
/// - run `MEASURE` once the document is loaded, and give each new title of
///   the document to `told`;
/// - be transparent, unless `[blocks] background` is set (`background`);
/// - send the wheel to the flow, and keep a click on an image or SVG for
///   the image viewer (`viewer_document`).
pub(super) trait BlockView {
    /// Loads a document (`Flow::document`).
    fn load(&self, html: &str);
    /// Places the web view at `x`, `y` in the view, `width × height`.
    fn place(&self, x: i32, y: i32, width: i32, height: i32);
    /// Runs one of raterm's scripts, without waiting for its result.
    fn run(&self, script: &str);
    /// Copies the selection of the document to the clipboard.
    fn copy(&self);
}

/// Measures the document, and again when it changes (as a video once it
/// knows its size): its height, then the natural width of its widest
/// table, whatever the width of the view (see `Flow::layout`). The answer
/// comes back through the document's title (`Title::Size`), as reading the
/// result of a script would take the `javascriptcore` crate on Linux.
pub(super) const MEASURE: &str = "(() => {
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
                     })()";

/// Clears the selection of a document.
pub(super) const CLEAR_SELECTION: &str = "getSelection().removeAllRanges()";

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

/// The script of a step of the search in a block. The query goes in as a
/// JSON string: no markup is built from the content of the terminal.
pub(super) fn find_script(find: &Find) -> String {
    (match find {
        Find::Start { query, backwards } => {
            FIND.replace("QUERY", &serde_json::to_string(query).unwrap())
                .replace("BACKWARDS", &backwards.to_string())
                + SHOW
        }
        Find::Next => format!("window.rtFind && window.rtFind.i++; {SHOW}"),
        Find::Previous => format!("window.rtFind && window.rtFind.i--; {SHOW}"),
        Find::Finish => "window.rtFind = null; CSS.highlights.clear();".to_owned(),
    }) + REPAINT
}

/// What a block tells through its document's title.
#[derive(Debug, PartialEq)]
pub(super) enum Title {
    /// Its height, and the natural width of its widest table (`MEASURE`).
    Size(f64, f64),
    /// The top of the current match of the search, in the block.
    Found(f64),
}

impl Title {
    pub(super) fn parse(title: &str) -> Option<Title> {
        if let Some(size) = title.strip_prefix("h:") {
            let (h, w) = size.split_once(':')?;
            return Some(Title::Size(h.parse().ok()?, w.parse().ok()?));
        }
        let top: f64 = title.strip_prefix("f:")?.split(':').next()?.parse().ok()?;
        (top >= 0.0).then_some(Title::Found(top))
    }
}

/// What the backend does once a block told its title (see `told`).
pub(super) enum Told {
    /// The block was measured: lay the flow out again.
    Measured,
    /// Scroll to the current match: `Flow::reveal_in_block`, then lay out.
    Found(f64),
}

/// A block's document has a new title: its measures go in `heights` and
/// `widths`, those of the `Flow`, which the handlers of web views hold
/// without borrowing the view.
pub(super) fn told(
    entry: u64,
    title: &str,
    heights: &RefCell<HashMap<u64, f64>>,
    widths: &RefCell<HashMap<u64, f64>>,
) -> Option<Told> {
    match Title::parse(title)? {
        Title::Size(h, w) => {
            heights.borrow_mut().insert(entry, h);
            widths.borrow_mut().insert(entry, w);
            Some(Told::Measured)
        }
        Title::Found(top) => Some(Told::Found(top)),
    }
}

impl Flow {
    /// The size of an image block: at most `h` lines and
    /// `[images] max_height`, at most `w` columns.
    fn image_style(&self, block: &Rendered) -> String {
        let (cw, ch) = self.cell;
        let lines = match (block.h, self.config.images.max_height) {
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

    /// The document of a block: no network, no script, but raterm's own
    /// media server for local files; `style` is `block_style`'s.
    pub(super) fn document(
        &self,
        block: &Rendered,
        media: &raterm::media::Server,
        style: &str,
    ) -> String {
        let size = self.image_style(block);
        // Local files go through the media server, the rest are data: URLs.
        let src = match block.content.strip_prefix("media/") {
            Some(n) => format!("{}{n}", media.base),
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
            media.origin, style
        )
    }
}

/// The document of the image viewer: an image or SVG block full window,
/// over the terminal, until a click or Escape.
pub(super) fn viewer_document(image: &Rendered) -> String {
    // SVGs have no natural size worth keeping: they fill the window.
    let size = if image.kind == Kind::Svg {
        "width: 95%; height: 95%; object-fit: contain;"
    } else {
        "max-width: 95%; max-height: 95%;"
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
<style>
html, body {{ margin: 0; height: 100%; overflow: hidden; background: rgba(0, 0, 0, 0.5); }}
body {{ display: flex; align-items: center; justify-content: center; }}
img {{ {size} }}
span {{ position: absolute; top: 8px; right: 12px; font: 28px/1 sans-serif; color: #fff; opacity: 0.8; }}
</style></head><body><span>×</span><img src="{}"></body></html>"#,
        image.content
    )
}

/// The background of blocks, as configured, or the terminal's: the color
/// of text drawn on highlights, and of blocks with one.
pub(super) fn background(config: &Config) -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_tell_sizes_and_matches() {
        assert_eq!(Title::parse("h:120:340"), Some(Title::Size(120.0, 340.0)));
        assert_eq!(Title::parse("f:42:1729"), Some(Title::Found(42.0)));
        assert_eq!(Title::parse("f:-3:1729"), None);
        assert_eq!(Title::parse("a block's own title"), None);
    }
}
