"use strict";

const flow = document.getElementById("flow");
const screen = document.getElementById("screen");
const alt = document.getElementById("alt");
const input = document.getElementById("input");
const searchBox = document.getElementById("search");
const send = (msg) => window.ipc.postMessage(JSON.stringify(msg));

// Configuration (rtConfig comes from config.js, generated from config.toml)

const config = rtConfig;
const root = document.documentElement;
root.style.setProperty("--font-family", `${config.font.family}, monospace`);
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
let appCursor = false;
let altOn = false;
// Last scrollback line when it soft-wraps, so its continuation joins it.
let wrapped = null;

window.rt = {
  events(list) {
    if (!queue.length) requestAnimationFrame(flush);
    queue.push(...list);
  },
};

function flush() {
  for (const ev of queue) apply(ev);
  queue = [];
  follow();
}

function apply(ev) {
  switch (ev.type) {
    case "scroll_out": {
      const line = wrapped ?? newLine();
      line.append(...spans(ev.line.runs));
      if (!wrapped) flow.insertBefore(line, screen);
      wrapped = ev.line.wrap ? line : null;
      trimScrollback();
      break;
    }
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
      appCursor = ev.app_cursor;
      break;
    case "block": {
      const block = richBlock(ev);
      const old = ev.replace && flow.querySelector(`.rich[data-id="${CSS.escape(ev.id)}"]`);
      if (old) old.replaceWith(block);
      else flow.insertBefore(block, screen);
      wrapped = null;
      trimScrollback();
      break;
    }
    case "title":
      if (config.dynamic_title) document.title = ev.title;
      break;
  }
}

// Rich blocks

function richBlock(ev) {
  const div = document.createElement("div");
  div.className = `rich ${ev.kind}`;
  if (ev.id) div.dataset.id = ev.id;
  if (ev.text) div.dataset.text = ev.text;
  if (ev.w) div.style.maxWidth = `${ev.w}ch`;
  const maxHeight = ev.h ? `${ev.h}lh` : "";
  if (ev.kind === "img" || ev.kind === "svg") {
    // An SVG loaded as an image never runs scripts nor fetches resources.
    const img = document.createElement("img");
    img.addEventListener("load", follow);
    img.src = ev.content;
    img.style.maxHeight = maxHeight;
    div.append(img);
    return div;
  }
  // No allow-scripts: the content can never run JavaScript. allow-same-origin
  // only lets this page measure the content and read its selection. WebKit
  // runs no listener at all on such a document, even one added from here.
  const frame = document.createElement("iframe");
  frame.sandbox = "allow-same-origin";
  frame.style.maxHeight = maxHeight;
  frame.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><style>${richStyle()}</style></head><body>${ev.content}</body></html>`;
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
  const vars = ["--fg", "--bg", "--selection", "--c4", "--c8"]
    .map((name) => `${name}: ${root.getPropertyValue(name)};`)
    .join(" ");
  return `
    :root { ${vars} color-scheme: dark; }
    html, body { margin: 0; color: var(--fg); background: transparent; overflow-y: hidden; }
    body { font: 14px/1.5 sans-serif; padding: 4px 0; overflow-x: auto; }
    a { color: var(--c4); }
    pre, code { font-family: monospace; }
    pre { padding: 8px; background: rgba(255, 255, 255, 0.05); overflow-x: auto; }
    table { border-collapse: collapse; margin: 8px 0; }
    th, td { border: 1px solid var(--c8); padding: 4px 10px; text-align: left; vertical-align: top; }
    th { background: rgba(255, 255, 255, 0.08); }
    tbody tr:nth-child(even) { background: rgba(255, 255, 255, 0.04); }
    img { max-width: 100%; }
    ::selection { background: var(--selection); }
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

function trimScrollback() {
  // Every child but the screen is a scrollback line or a rich block.
  while (flow.children.length - 1 > config.scrollback) flow.firstElementChild.remove();
}

function newLine() {
  const div = document.createElement("div");
  div.className = "line";
  return div;
}

function spans(runs) {
  return runs.map((run) => {
    const span = document.createElement("span");
    span.textContent = run.text;
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

// Whether the view follows new output. Only the user's scrolling changes it:
// content growing on its own (images, iframes loading) does not fire scroll.
let stick = true;
flow.addEventListener("scroll", () => {
  stick = flow.scrollTop + flow.clientHeight >= flow.scrollHeight - 4;
});

function follow() {
  if (stick) toBottom();
}

function toBottom() {
  flow.scrollTop = flow.scrollHeight;
}

// Size

let size = {};

function resize() {
  const probe = newLine();
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  probe.textContent = "X".repeat(100);
  document.body.append(probe);
  const cell = probe.getBoundingClientRect();
  probe.remove();
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
  return (appCursor ? "\x1bO" : "\x1b[") + final;
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
let matches = [];
let current = -1;

function openSearch() {
  searchBox.hidden = false;
  searchInput.select();
  searchInput.focus();
}

function closeSearch() {
  searchBox.hidden = true;
  CSS.highlights.clear();
  matches = [];
  input.focus();
}

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

/// Moves to the previous (older) match, or the next one.
function step(backwards) {
  searchBox.classList.toggle("none", !matches.length && searchInput.value !== "");
  if (!matches.length) return CSS.highlights.clear();
  current = (current + (backwards ? -1 : 1) + matches.length) % matches.length;
  CSS.highlights.set("search", new Highlight(...matches));
  CSS.highlights.set("search-current", new Highlight(matches[current]));
  matches[current].startContainer.parentElement.scrollIntoView({ block: "center" });
}

searchInput.addEventListener("input", () => {
  matches = findAll(searchInput.value);
  current = matches.length;
  step(true);
});

searchInput.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Escape") closeSearch();
  else if (e.key === "Enter") {
    // Output may have changed since the last search.
    const at = current;
    matches = findAll(searchInput.value);
    current = Math.min(at, matches.length);
    step(!e.shiftKey);
  } else return;
  e.preventDefault();
});

// Keyboard

function onKeydown(e) {
  if (e.isComposing) return;
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

// Middle click pastes the primary selection, as everywhere on Linux.
document.addEventListener("auxclick", (e) => {
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
