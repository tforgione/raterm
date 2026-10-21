"use strict";

const flow = document.getElementById("flow");
const screen = document.getElementById("screen");
const alt = document.getElementById("alt");
const input = document.getElementById("input");
const searchBox = document.getElementById("search");
const send = (msg) => window.ipc.postMessage(JSON.stringify(msg));

// Configuration (ratermConfig comes from config.js, generated from config.toml)

const config = ratermConfig;
const root = document.documentElement;
const GENERIC_FONTS = ["serif", "sans-serif", "monospace", "cursive", "fantasy", "system-ui"];

/// A CSS font-family value from a configured list of names: names are quoted,
/// as unquoted ones cannot hold words starting with a digit (Source Serif 4).
function fontFamily(names, fallback) {
  const quoted = names
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean)
    .map((name) => (GENERIC_FONTS.includes(name) ? name : JSON.stringify(name)));
  return [...quoted, fallback].join(", ");
}

root.style.setProperty("--font-family", fontFamily(config.font.family, "monospace"));
// [colors] of config.toml: inline, so over user.css.
const COLOR_VARS = { foreground: "--fg", background: "--bg", cursor: "--cursor", selection: "--selection", selection_text: "--selection-text" };
for (const [key, name] of Object.entries(COLOR_VARS)) {
  if (config.colors[key]) root.style.setProperty(name, config.colors[key]);
}
config.colors.normal.forEach((color, i) => root.style.setProperty(`--c${i}`, color));
config.colors.bright.forEach((color, i) => root.style.setProperty(`--c${i + 8}`, color));
document.body.classList.add(`cursor-${config.cursor.style}`);
if (config.cursor.blink) {
  setInterval(() => document.body.classList.toggle("blink-off"), 530);
}
window.addEventListener("blur", () => {
  document.body.classList.add("unfocused");
  // A click in a rich block focuses its frame, which would swallow the keys
  // (no listener runs there, see richBlock): take the focus back. The text
  // selected in the block stays selected, for copy.
  setTimeout(() => {
    if (document.activeElement instanceof HTMLIFrameElement) {
      selectedFrame = document.activeElement;
      input.focus({ preventScroll: true });
    }
  });
});
window.addEventListener("focus", () => document.body.classList.remove("unfocused"));

// Last rich block clicked, whose selection copy falls back to.
let selectedFrame = null;
let zoom = 0;

function setZoom(level) {
  zoom = level;
  root.style.setProperty("--font-size", `${config.font.size * 1.1 ** zoom}pt`);
  resize();
}

let queue = [];
// Same initial modes as the core's terminal (see Modes::of).
let modes = { app_cursor: false, mouse: 0, sgr_mouse: false, alt_scroll: true };
let altOn = false;

// Output after a quiet moment, like the echo of a key, is applied at once:
// waiting for the next animation frame cost half a frame on average. Output
// that keeps coming is applied once per frame, as laying the page out for
// every batch made a flood 25 times slower to display.
const QUIET_MS = 50;
let scheduled = false;
let lastFlush = 0;

window.raterm = {
  events(list) {
    queue.push(...list);
    if (scheduled) return;
    if (performance.now() - lastFlush > QUIET_MS) return flush();
    scheduled = true;
    requestAnimationFrame(() => {
      scheduled = false;
      flush();
    });
  },
};

function flush() {
  const anchor = stick || altOn ? null : anchorAt();
  for (const ev of coalesce(queue)) apply(ev);
  queue = [];
  sizeSpacers();
  if (jumpTo !== null) {
    const node = nodeOf(jumpTo);
    if (node) flow.scrollTop += node.getBoundingClientRect().top - flow.getBoundingClientRect().top;
    jumpTo = null;
  } else if (anchor?.node.isConnected) {
    flow.scrollTop += anchor.node.getBoundingClientRect().top - anchor.top;
  }
  if (cur?.show) showMatch();
  follow();
  requestMore();
  lastFlush = performance.now();
}

/// Merges the screen updates of a batch, as only the last state of each row
/// gets painted: a program redrawing fast sends many per frame. A switch of
/// screen ends a merge, as the rows that follow belong to the other grid.
/// Entries of the scrollback that the window would drop within the frame are
/// left out.
function coalesce(events) {
  const added = (ev) => ev.type === "scroll_out" || (ev.type === "block" && !ev.replace);
  let newest = -1;
  for (const ev of events) if (added(ev)) newest = Math.max(newest, ev.entry);
  const out = [];
  let merged = null;
  for (const ev of events) {
    if (added(ev) && ev.entry <= newest - WINDOW) continue;
    if (ev.type !== "lines") {
      if (ev.type === "alt") merged = null;
      out.push(ev);
      continue;
    }
    if (!merged) {
      merged = { type: "lines", rows: new Map() };
      out.push(merged);
    }
    merged.height = ev.height;
    // Rows cut by a smaller screen are resent by the core if it grows back.
    for (const y of merged.rows.keys()) if (y >= ev.height) merged.rows.delete(y);
    for (const row of ev.rows) merged.rows.set(row.y, row);
  }
  for (const ev of out) if (ev.rows instanceof Map) ev.rows = [...ev.rows.values()];
  return out;
}

function apply(ev) {
  switch (ev.type) {
    case "scroll_out":
      if (ev.entry < scrollback.next) {
        // A soft-wrapped line continues its entry, the last one.
        if (win.next === scrollback.next && win.next > win.first) {
          lower.previousElementSibling.append(...spans(ev.line.runs));
        }
      } else {
        const follows = win.next === scrollback.next;
        scrollback.next = ev.entry + 1;
        if (follows) push(ev.entry, lineNode(ev.line.runs));
      }
      break;
    case "lines": {
      const grid = altOn ? alt : screen;
      while (grid.children.length > ev.height) grid.lastChild.remove();
      while (grid.children.length < ev.height) grid.append(newLine());
      for (const row of ev.rows) grid.children[row.y].replaceChildren(...spans(row.runs));
      if (!altOn) collapseTrailing(grid);
      break;
    }
    case "alt":
      altOn = ev.on;
      alt.hidden = !altOn;
      flow.hidden = altOn;
      alt.replaceChildren();
      break;
    case "mode":
      modes = ev;
      break;
    case "block":
      if (ev.replace) {
        const block = richBlock(ev);
        block.dataset.entry = ev.entry;
        nodeOf(ev.entry)?.replaceWith(block);
      } else {
        const follows = win.next === scrollback.next;
        scrollback.next = ev.entry + 1;
        if (follows) push(ev.entry, richBlock(ev));
      }
      break;
    case "history":
      scrollback = { first: ev.first, next: ev.next };
      // Entries dropped by the core's cap leave the window too.
      while (win.first < Math.min(scrollback.first, win.next)) {
        upper.nextElementSibling.remove();
        win.first++;
      }
      win = { first: Math.max(win.first, scrollback.first), next: Math.max(win.next, scrollback.first) };
      break;
    case "entries":
      place(ev.entries);
      break;
    case "found":
      found(ev);
      break;
    case "title":
      if (config.dynamic_title) document.title = ev.title;
      break;
  }
}

// Scrollback: the core keeps it whole; the page holds a window of it, entries
// win.first..win.next, between two spacers that stand for the other entries
// (see Spec-Scrollback in the wiki).

const WINDOW = ratermWindow;
// Entries asked for at once, when the view comes close to an end of the window.
const CHUNK = 1000;
let scrollback = { first: 0, next: 0 };
let win = { first: 0, next: 0 };
const upper = spacer();
const lower = spacer();
flow.prepend(upper);
screen.before(lower);
// The request in flight, and the entry to show at the top of the view once
// a jump has loaded it.
let pending = null;
let jumpTo = null;

function spacer() {
  const div = document.createElement("div");
  div.className = "spacer";
  return div;
}

function lineNode(runs) {
  const line = newLine();
  line.append(...spans(runs));
  return line;
}

/// Appends entry `n` to the window, which ends at the end of the scrollback.
function push(n, node) {
  // The core or coalesce skipped the entries that would not be shown.
  if (n !== win.next) clearWindow(n);
  node.dataset.entry = n;
  lower.before(node);
  win.next = n + 1;
  if (win.next - win.first > WINDOW) {
    upper.nextElementSibling.remove();
    win.first++;
  }
}

function clearWindow(n) {
  const range = document.createRange();
  range.setStartAfter(upper);
  range.setEndBefore(lower);
  range.deleteContents();
  win = { first: n, next: n };
}

function nodeOf(n) {
  if (n < win.first || n >= win.next) return null;
  return flow.querySelector(`:scope > [data-entry="${n}"]`);
}

/// Height of an entry that is not loaded. One row: exact for most lines, and
/// stable, so that the view does not jump as entries load. Smaller past the
/// height an element can have.
function rowHeight() {
  return Math.min(cellHeight, 16e6 / Math.max(1, scrollback.next - scrollback.first));
}

function sizeSpacers() {
  upper.style.height = `${(win.first - scrollback.first) * rowHeight()}px`;
  lower.style.height = `${(scrollback.next - win.next) * rowHeight()}px`;
}

/// The entry or screen row at the top of the view, to keep in place while
/// the window changes around it.
function anchorAt() {
  const box = flow.getBoundingClientRect();
  const hit = document.elementFromPoint(box.left + box.width / 2, box.top + 4);
  const node = hit?.closest("[data-entry], #screen > .line");
  return node && { node, top: node.getBoundingClientRect().top };
}

/// The entry at height `y` of the view, or null within the window.
function entryAt(y) {
  const start = upper.getBoundingClientRect();
  const end = lower.getBoundingClientRect();
  if (y < start.bottom) return scrollback.first + Math.floor((y - start.top) / rowHeight());
  if (y < end.top) return null;
  if (y < end.bottom) return win.next + Math.floor((y - end.top) / rowHeight());
  return scrollback.next;
}

/// Asks the core for the entries that the view shows or is about to show:
/// next to the window, or in place of it when the view is far from it.
function requestMore() {
  if (pending || altOn || !cellHeight) return;
  const view = flow.getBoundingClientRect();
  const margin = 2 * view.height;
  // With the screen at the top of the view, the last entries are next.
  const at = Math.min(entryAt(view.top) ?? win.first, scrollback.next - 1);
  if (at >= scrollback.first && (at < win.first - CHUNK || at >= win.next + CHUNK)) {
    return request(Math.max(scrollback.first, at - CHUNK / 2), CHUNK, { jump: at });
  }
  if (win.first > scrollback.first && view.top < upper.getBoundingClientRect().bottom + margin) {
    const from = Math.max(scrollback.first, win.first - CHUNK);
    return request(from, win.first - from, { before: win.first });
  }
  if (win.next < scrollback.next && view.bottom > lower.getBoundingClientRect().top - margin) {
    request(win.next, CHUNK, { after: win.next });
  }
}

function request(from, count, what) {
  pending = what;
  send({ type: "entries", from, count });
}

/// Puts the entries the core sent in the window: next to it when they extend
/// it, in place of it after a jump.
function place(entries) {
  const what = pending;
  pending = null;
  if (!what || !entries.length) return;
  const nodes = entries.map(({ entry, line, block }) => {
    const node = line ? lineNode(line.runs) : richBlock(block);
    node.dataset.entry = entry;
    return node;
  });
  const first = entries[0].entry;
  const next = entries.at(-1).entry + 1;
  if (what.before !== undefined) {
    // The window may have moved since the request.
    if (what.before !== win.first || next !== win.first) return;
    upper.after(...nodes);
    win.first = first;
    while (win.next - win.first > WINDOW) {
      lower.previousElementSibling.remove();
      win.next--;
    }
  } else if (what.after !== undefined) {
    if (what.after !== win.next || first !== win.next) return;
    lower.before(...nodes);
    win.next = next;
    while (win.next - win.first > WINDOW) {
      upper.nextElementSibling.remove();
      win.first++;
    }
  } else {
    clearWindow(first);
    lower.before(...nodes);
    win.next = next;
    jumpTo = what.jump;
  }
}

// Rich blocks

function richBlock(ev) {
  const div = document.createElement("div");
  div.className = `rich ${ev.kind}`;
  if (ev.id) div.dataset.id = ev.id;
  if (ev.text) div.dataset.text = ev.text;
  if (ev.w) div.style.maxWidth = `${ev.w}ch`;
  let lines = ev.h;
  if (ev.kind === "video" || ev.kind === "audio") {
    const player = document.createElement(ev.kind);
    player.controls = true;
    player.preload = "metadata";
    player.addEventListener("loadedmetadata", follow);
    // Local files are streamed by raterm's media server, the rest are data: URLs.
    player.src = ev.content.startsWith("media/") ? ratermMedia + ev.content.slice("media/".length) : ev.content;
    const limit = ev.kind === "video" && config.images.max_height;
    if (limit) lines = Math.min(lines ?? limit, limit);
    if (lines && ev.kind === "video") player.style.maxHeight = `${lines}lh`;
    div.append(player);
    return div;
  }
  if (ev.kind === "img" || ev.kind === "svg") {
    // An SVG loaded as an image never runs scripts nor fetches resources.
    const img = document.createElement("img");
    img.addEventListener("load", follow);
    img.addEventListener("click", () => openViewer(img));
    img.src = ev.content;
    const limit = config.images.max_height;
    if (limit) lines = Math.min(lines ?? limit, limit);
    if (lines) img.style.maxHeight = `${lines}lh`;
    div.append(img);
    return div;
  }
  const maxHeight = lines ? `${lines}lh` : "";
  // No allow-scripts: the content can never run JavaScript. allow-same-origin
  // only lets this page measure the content and read its selection. WebKit
  // runs no listener at all on such a document, even one added from here.
  const frame = document.createElement("iframe");
  frame.sandbox = "allow-same-origin";
  frame.style.maxHeight = maxHeight;
  frame.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><style>${richStyle()}</style></head><body class="${ev.kind}">${ev.content}</body></html>`;
  frame.addEventListener("load", () => {
    const doc = frame.contentDocument;
    const fit = () => {
      // The body, not the document, which never gets smaller than the frame.
      frame.style.height = `${Math.ceil(doc.body.getBoundingClientRect().height)}px`;
      follow();
    };
    new ResizeObserver(fit).observe(doc.body);
    fit();
    if (ev.kind === "html") div.dataset.text = doc.body.innerText;
  });
  div.append(frame);
  return div;
}

/// Stylesheet of rich blocks, using the terminal's current colors.
function richStyle() {
  const root = getComputedStyle(document.documentElement);
  const vars = ["--fg", "--bg", "--selection", "--selection-text", "--inline-code", "--c1", "--c2", "--c3", "--c4", "--c5", "--c6", "--c8"]
    .map((name) => `${name}: ${root.getPropertyValue(name)};`)
    .join(" ");
  const columns = config.markdown.max_width;
  // Zooming applies to blocks too (those created afterwards).
  const scale = 1.1 ** zoom;
  const text = `${config.blocks.font_size * scale}pt/1.5 ${fontFamily(config.blocks.font_family, "sans-serif")}`;
  const textWidth = columns ? `${columns * cellWidth}px` : "none";
  return `
    :root { ${vars} color-scheme: dark; }
    html, body { margin: 0; color: var(--fg); background: transparent; overflow-y: hidden; }
    body { font: ${text}; padding: 4px 0; overflow-x: auto; }
    a { color: var(--c4); }
    pre, code { font-family: ${fontFamily(config.font.family, "monospace")}; font-size: 0.9em; }
    :not(pre) > code {
      color: var(--inline-code);
      background: rgba(255, 255, 255, 0.06);
      border: 1px solid rgba(255, 255, 255, 0.1);
      border-radius: 4px;
      padding: 0.1em 0.35em;
      /* When the code wraps, each line gets a closed box of its own. */
      -webkit-box-decoration-break: clone;
      box-decoration-break: clone;
    }
    pre { padding: 8px; background: rgba(255, 255, 255, 0.05); overflow-x: auto; }
    /* Syntax highlighting: classes are TextMate scopes, colored with the
       terminal palette so that user.css themes them too. */
    .comment { color: var(--c8); font-style: italic; }
    .string { color: var(--c2); }
    .constant { color: var(--c3); }
    .keyword, .storage { color: var(--c5); }
    .keyword.operator { color: inherit; }
    .entity.name.function, .support.function { color: var(--c4); }
    .entity.name.type, .entity.name.class, .support.type, .support.class { color: var(--c6); }
    .entity.name.tag { color: var(--c1); }
    .entity.other.attribute-name { color: var(--c3); }
    table { border-collapse: collapse; margin: 8px 0; }
    th, td { border: 1px solid var(--c8); padding: 4px 10px; text-align: left; vertical-align: top; }
    th { background: rgba(255, 255, 255, 0.08); }
    tbody tr:nth-child(even) { background: rgba(255, 255, 255, 0.04); }
    img { max-width: 100%; }
    /* Markdown reads in a centered column; tables keep the whole width, as
       wide tables are what rendering Markdown is for. */
    body.md > * { max-width: ${textWidth}; margin-left: auto; margin-right: auto; }
    body.md > table { max-width: none; margin: 8px auto; }
    ::selection { background: var(--selection); color: var(--selection-text); }
  `;
}

// Copying a selection that spans rich blocks puts their text (the Markdown
// source, the rendered text of HTML) in place of the empty iframes.
document.addEventListener("copy", (e) => {
  const selection = window.getSelection();
  if (selection.isCollapsed || !flow.contains(selection.anchorNode)) return;
  const fragment = selection.getRangeAt(0).cloneContents();
  if (!fragment.querySelector(".rich[data-text]")) return;
  const text = [...fragment.childNodes]
    .map((node) => (node.classList?.contains("rich") ? node.dataset.text ?? "" : node.textContent))
    .join("\n");
  e.clipboardData.setData("text/plain", text);
  e.preventDefault();
});

/// Hides the empty rows below the last used one, so that the output above
/// (rich blocks in particular) stays in view right above the prompt.
function collapseTrailing(grid) {
  let trailing = true;
  for (let i = grid.children.length - 1; i >= 0; i--) {
    const row = grid.children[i];
    trailing &&= !row.firstChild;
    row.hidden = trailing;
  }
}

function newLine() {
  const div = document.createElement("div");
  div.className = "line";
  return div;
}

function spans(runs) {
  return runs.map((run) => {
    const span = document.createElement("span");
    span.append(...cellText(run.text, run.class?.includes("wide") ? 2 : 1));
    if (run.class) span.className = run.class;
    if (run.fg) span.style.color = run.fg;
    if (run.bg) span.style.backgroundColor = run.bg;
    if (run.link) {
      span.classList.add("link");
      span.dataset.href = run.link;
    }
    return span;
  });
}

// Glyphs missing from the configured font come from fallback fonts, whose
// widths differ: in plain text, they would shift the rest of the line off the
// grid. Such glyphs are boxed to their cells, like a terminal draws them.

let cellWidth = 0;
const glyphFits = new Map();
const graphemes = new Intl.Segmenter();
const ruler = document.createElement("span");
ruler.style.cssText = "position: absolute; visibility: hidden; white-space: pre";
document.body.append(ruler);

function fits(glyph, cells) {
  if (glyph < "\u0080") return true;
  const key = glyph + cells;
  let ok = glyphFits.get(key);
  if (ok === undefined) {
    ruler.textContent = glyph;
    ok = Math.abs(ruler.getBoundingClientRect().width - cells * cellWidth) < 0.5;
    glyphFits.set(key, ok);
  }
  return ok;
}

/// The text of a run, with glyphs that do not fit their cells boxed.
function cellText(text, cells) {
  // ASCII always fits (see fits): skip segmenting, the bulk of the work.
  if (/^[\0-\x7f]*$/.test(text)) return [text];
  const nodes = [];
  let plain = "";
  for (const { segment } of graphemes.segment(text)) {
    if (fits(segment, cells)) {
      plain += segment;
      continue;
    }
    if (plain) nodes.push(plain);
    plain = "";
    const box = document.createElement("span");
    box.className = "glyph";
    box.style.width = `${cells}ch`;
    box.textContent = segment;
    nodes.push(box);
  }
  if (plain) nodes.push(plain);
  return nodes;
}

// Whether the view follows new output. Only the user's scrolling changes it:
// content growing on its own (images, iframes loading) does not fire scroll.
let stick = true;
let scrolled = false;
flow.addEventListener("scroll", () => {
  stick = flow.scrollTop + flow.clientHeight >= flow.scrollHeight - 4;
  if (scrolled) return;
  scrolled = true;
  requestAnimationFrame(() => {
    scrolled = false;
    requestMore();
  });
});

function follow() {
  if (stick) toBottom();
}

/// Scrolls to the end; the last entries load if the window is elsewhere.
function toBottom() {
  flow.scrollTop = flow.scrollHeight;
  requestMore();
}

// Size

let size = {};
let cellHeight = 0;

function resize() {
  const probe = newLine();
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  // The font's natural line height, which lines are then fixed to.
  probe.style.lineHeight = "normal";
  probe.textContent = "X".repeat(100);
  document.body.append(probe);
  const cell = probe.getBoundingClientRect();
  root.style.setProperty("--cell-height", `${cell.height}px`);
  probe.remove();
  cellHeight = cell.height;
  sizeSpacers();
  if (cell.width / 100 !== cellWidth) {
    cellWidth = cell.width / 100;
    glyphFits.clear();
  }
  const box = (altOn ? alt : flow).getBoundingClientRect();
  const cols = Math.max(1, Math.floor((box.width - 8) / (cell.width / 100)));
  const rows = Math.max(1, Math.floor((box.height - 4) / cell.height));
  if (cols !== size.cols || rows !== size.rows) {
    size = { cols, rows };
    send({ type: "resize", cols, rows });
  }
}

window.addEventListener("resize", () => requestAnimationFrame(resize));
setZoom(0);

// Keyboard

function csi(final, mod, tilde) {
  if (tilde) return mod > 1 ? `\x1b[${final};${mod}~` : `\x1b[${final}~`;
  if (mod > 1) return `\x1b[1;${mod}${final}`;
  return (modes.app_cursor ? "\x1bO" : "\x1b[") + final;
}

const CURSOR_KEYS = { ArrowUp: "A", ArrowDown: "B", ArrowRight: "C", ArrowLeft: "D", Home: "H", End: "F" };
const TILDE_KEYS = { Insert: 2, Delete: 3, PageUp: 5, PageDown: 6, F5: 15, F6: 17, F7: 18, F8: 19, F9: 20, F10: 21, F11: 23, F12: 24 };
const SS3_KEYS = { F1: "P", F2: "Q", F3: "R", F4: "S" };

/// Escape sequence for a key, or null when it produces plain text (handled
/// through the textarea so that dead keys and input methods work).
function encode(e) {
  const mod = 1 + e.shiftKey + 2 * e.altKey + 4 * e.ctrlKey;
  if (e.key in CURSOR_KEYS) return csi(CURSOR_KEYS[e.key], mod);
  if (e.key in TILDE_KEYS) return csi(TILDE_KEYS[e.key], mod, true);
  if (e.key in SS3_KEYS) return mod > 1 ? `\x1b[1;${mod}${SS3_KEYS[e.key]}` : `\x1bO${SS3_KEYS[e.key]}`;
  const esc = e.altKey ? "\x1b" : "";
  switch (e.key) {
    case "Enter": return esc + "\r";
    case "Backspace": return esc + (e.ctrlKey ? "\x08" : "\x7f");
    case "Tab": return e.shiftKey ? "\x1b[Z" : esc + "\t";
    case "Escape": return "\x1b";
  }
  if (e.key.length !== 1) return null;
  if (e.ctrlKey) {
    if (e.key === " ") return esc + "\x00";
    if (e.key === "/") return esc + "\x1f";
    const code = e.key.toUpperCase().charCodeAt(0);
    if (code >= 0x40 && code <= 0x5f) return esc + String.fromCharCode(code - 0x40);
    return null;
  }
  return e.altKey ? esc + e.key : null;
}

// Shortcuts

const MODIFIERS = ["ctrl", "alt", "shift", "super"];

const DEFAULT_KEYS = {
  "ctrl+shift+c": "copy",
  "ctrl+shift+v": "paste",
  "shift+insert": "paste_selection",
  "ctrl+shift+f": "search",
  "ctrl+=": "zoom_in",
  "ctrl++": "zoom_in",
  "ctrl+-": "zoom_out",
  "ctrl+0": "zoom_reset",
  "shift+pageup": "scroll_page_up",
  "shift+pagedown": "scroll_page_down",
};

/// Canonical form of a shortcut such as "Shift+Ctrl+C": "ctrl+shift+c".
function normalize(spec) {
  const parts = spec.toLowerCase().split("+");
  let key = parts.pop();
  if (key === "") {
    parts.pop();
    key = "+";
  }
  return [...MODIFIERS.filter((m) => parts.includes(m)), key].join("+");
}

const keys = Object.fromEntries(
  Object.entries({ ...DEFAULT_KEYS, ...config.keys }).map(([spec, binding]) => [normalize(spec), binding]),
);

function keyName(e) {
  // Digits by position, so that ctrl+0 works on layouts where 0 is shifted.
  let key = /^(Digit|Numpad)/.test(e.code) ? e.code.replace("Digit", "") : e.key;
  if (key === " ") key = "space";
  const mods = [e.ctrlKey, e.altKey, e.shiftKey, e.metaKey];
  return normalize([...MODIFIERS.filter((_, i) => mods[i]), key].join("+"));
}

/// Runs a bound action; false lets the key reach the program instead.
function runAction(binding) {
  if (typeof binding === "object") {
    type(binding.send);
    return true;
  }
  switch (binding) {
    case "copy": {
      const frame = selectedFrame?.isConnected && selectedFrame.contentDocument;
      const inFrame = window.getSelection().isCollapsed && frame && !frame.getSelection().isCollapsed;
      (inFrame ? frame : document).execCommand("copy");
      break;
    }
    case "paste":
      input.focus();
      document.execCommand("paste");
      break;
    case "paste_selection":
      send({ type: "paste_primary" });
      break;
    case "search":
      openSearch();
      break;
    case "zoom_in":
      setZoom(zoom + 1);
      break;
    case "zoom_out":
      setZoom(zoom - 1);
      break;
    case "zoom_reset":
      setZoom(0);
      break;
    case "scroll_page_up":
    case "scroll_page_down":
      // The alternate screen has no scrollback: the program gets the key.
      if (altOn) return false;
      flow.scrollBy(0, (binding === "scroll_page_up" ? -0.9 : 0.9) * flow.clientHeight);
      break;
    case "new_window":
      send({ type: "new_window" });
      break;
    default:
      return false;
  }
  return true;
}

// Search, highlighted with the CSS Custom Highlight API so that neither the
// selection, the focus nor the DOM change.

const searchInput = searchBox.querySelector("input");
// The current match: an entry of the scrollback or a row of the screen (see
// Event::Found in term.rs), and offsets in its text; `show` until shown.
let cur = null;

function openSearch() {
  searchBox.hidden = false;
  searchInput.select();
  searchInput.focus();
}

function closeSearch() {
  searchBox.hidden = true;
  CSS.highlights.clear();
  cur = null;
  input.focus();
}

/// Matches in the lines loaded in the page, all highlighted.
function findAll(query) {
  const needle = query.toLowerCase();
  const found = [];
  if (!needle) return found;
  for (const line of flow.querySelectorAll(".line:not([hidden])")) {
    const text = line.textContent.toLowerCase();
    for (let i = text.indexOf(needle); i !== -1; i = text.indexOf(needle, i + needle.length)) {
      found.push(rangeIn(line, i, i + needle.length));
    }
  }
  return found;
}

/// Range over characters start..end of the text of `el`, across its spans.
function rangeIn(el, start, end) {
  const range = document.createRange();
  const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
  let pos = 0;
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const next = pos + node.length;
    if (start >= pos && start < next) range.setStart(node, start - pos);
    if (end > pos && end <= next) {
      range.setEnd(node, end - pos);
      break;
    }
    pos = next;
  }
  return range;
}

/// Asks the core for the previous (older) match, or the next one, over the
/// whole scrollback.
function step(backwards) {
  send({ type: "search", query: searchInput.value, entry: cur?.entry ?? null, at: cur?.start ?? 0, backwards });
}

function found(ev) {
  searchBox.classList.toggle("none", ev.entry === null && searchInput.value !== "");
  if (ev.entry === null) {
    cur = null;
    return CSS.highlights.clear();
  }
  cur = { entry: ev.entry, start: ev.start, end: ev.end, show: true };
}

/// Highlights the current match, loading the window around it if needed.
function showMatch() {
  const node = cur.entry >= scrollback.next ? screen.children[cur.entry - scrollback.next] : nodeOf(cur.entry);
  if (!node) {
    if (!pending) request(Math.max(scrollback.first, cur.entry - CHUNK / 2), CHUNK, { jump: cur.entry });
    return;
  }
  cur.show = false;
  CSS.highlights.set("search", new Highlight(...findAll(searchInput.value)));
  CSS.highlights.set("search-current", new Highlight(rangeIn(node, cur.start, cur.end)));
  node.scrollIntoView({ block: "center" });
  // The scroll event comes later: new output must not bring the view back.
  stick = false;
}

searchInput.addEventListener("input", () => {
  cur = null;
  step(true);
});

searchInput.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Escape") closeSearch();
  else if (e.key === "Enter") step(!e.shiftKey);
  else return;
  e.preventDefault();
});

// Keyboard

// Image viewer: a clicked image, full window, over the terminal.

const viewer = document.getElementById("viewer");

function openViewer(img) {
  const shown = viewer.querySelector("img");
  shown.src = img.src;
  shown.classList.toggle("vector", img.closest(".rich").classList.contains("svg"));
  viewer.hidden = false;
}

function closeViewer() {
  viewer.hidden = true;
  viewer.querySelector("img").removeAttribute("src");
  input.focus({ preventScroll: true });
}

// Any click closes it: on the cross, the image or around it.
viewer.addEventListener("click", closeViewer);

function onKeydown(e) {
  if (e.isComposing) return;
  if (!viewer.hidden) {
    // Keys belong to the viewer while it is open, not to the program.
    if (e.key === "Escape") closeViewer();
    e.preventDefault();
    return;
  }
  const binding = keys[keyName(e)];
  if (binding !== undefined && runAction(binding)) {
    e.preventDefault();
    return;
  }
  const data = encode(e);
  if (data !== null) {
    e.preventDefault();
    type(data);
  } else if (document.activeElement !== input) {
    // Focus was lost to a mouse selection: refocus, and do not drop this key.
    input.focus();
    if (e.key.length === 1) {
      e.preventDefault();
      type(e.key);
    }
  }
}

document.addEventListener("keydown", onKeydown);

function type(data) {
  send({ type: "input", data });
  input.focus({ preventScroll: true });
  toBottom();
}

function flushInput() {
  if (input.value) type(input.value);
  input.value = "";
}

input.addEventListener("input", (e) => {
  if (!e.isComposing) flushInput();
});
input.addEventListener("compositionend", flushInput);

input.addEventListener("paste", (e) => {
  e.preventDefault();
  const data = e.clipboardData.getData("text/plain");
  if (data) send({ type: "paste", data });
  toBottom();
});

// Keep the keyboard on the textarea, except while a selection exists (focusing
// would clear it before it can be copied) or the search box is used.
document.addEventListener("mouseup", (e) => {
  if (window.getSelection().isCollapsed && !searchBox.contains(e.target)) {
    input.focus({ preventScroll: true });
  }
});

// Mouse reporting: programs that ask for it (vim, htop, tmux...) get clicks,
// drags and the wheel over the grid. Shift keeps the mouse for selecting.

let pressed = null;
let lastCell = null;

/// The grid cell under the pointer, or null outside the grid.
function cellAt(e) {
  const first = (altOn ? alt : screen).firstElementChild;
  if (!first) return null;
  const origin = first.getBoundingClientRect();
  const x = Math.floor((e.clientX - origin.left) / cellWidth);
  const y = Math.floor((e.clientY - origin.top) / origin.height);
  return x >= 0 && y >= 0 && x < size.cols && y < size.rows ? { x, y } : null;
}

function reporting(e) {
  return modes.mouse > 0 && !e.shiftKey && viewer.hidden && cellAt(e) !== null;
}

/// xterm's encoding of a mouse event: `button` is 0-2 for the buttons, 64-65
/// for the wheel, plus 32 for motion.
function sendMouse(e, button, cell, release) {
  const code = button + 4 * e.shiftKey + 8 * e.altKey + 16 * e.ctrlKey;
  if (modes.sgr_mouse) {
    send({ type: "input", data: `\x1b[<${code};${cell.x + 1};${cell.y + 1}${release ? "m" : "M"}` });
  } else if (cell.x < 223 && cell.y < 223) {
    // The legacy encoding has no release per button, and stops at 223.
    const b = release ? (code & ~3) | 3 : code;
    send({ type: "input", data: "\x1b[M" + String.fromCharCode(32 + b, 33 + cell.x, 33 + cell.y) });
  }
}

document.addEventListener("mousedown", (e) => {
  if (!reporting(e) || e.button > 2) return;
  // No selection, and the keyboard stays on the terminal.
  e.preventDefault();
  pressed = e.button;
  lastCell = cellAt(e);
  sendMouse(e, pressed, lastCell, false);
});

document.addEventListener("mouseup", (e) => {
  if (pressed === null) return;
  const cell = cellAt(e) ?? lastCell;
  sendMouse(e, pressed, cell, true);
  pressed = null;
});

document.addEventListener("mousemove", (e) => {
  if (modes.mouse < 2 || (modes.mouse === 2 && pressed === null) || e.shiftKey) return;
  const cell = cellAt(e);
  if (!cell || (lastCell && cell.x === lastCell.x && cell.y === lastCell.y)) return;
  lastCell = cell;
  sendMouse(e, 32 + (pressed ?? 3), cell, false);
});

document.addEventListener(
  "wheel",
  (e) => {
    if (!e.deltaY) return;
    if (reporting(e)) {
      sendMouse(e, e.deltaY < 0 ? 64 : 65, cellAt(e), false);
    } else if (altOn && modes.alt_scroll && !e.shiftKey) {
      // Full-screen programs without mouse reporting (less, man) scroll with
      // arrow keys.
      send({ type: "input", data: csi(e.deltaY < 0 ? "A" : "B", 1).repeat(3) });
    } else {
      return;
    }
    e.preventDefault();
  },
  { passive: false },
);

document.addEventListener("contextmenu", (e) => {
  if (reporting(e)) e.preventDefault();
});

// Middle click pastes the primary selection, as everywhere on Linux.
document.addEventListener("auxclick", (e) => {
  if (e.button === 1 && reporting(e)) return;
  if (e.button === 1) {
    e.preventDefault();
    send({ type: "paste_primary" });
  }
});

// Ctrl+click opens links (a plain click is left to selecting text).
document.addEventListener("click", (e) => {
  const link = e.ctrlKey && e.target.closest?.("[data-href]");
  if (link) send({ type: "open_url", url: link.dataset.href });
});
input.focus();
