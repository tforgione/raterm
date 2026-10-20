"use strict";

const flow = document.getElementById("flow");
const screen = document.getElementById("screen");
const alt = document.getElementById("alt");
const input = document.getElementById("input");
const send = (msg) => window.ipc.postMessage(JSON.stringify(msg));

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
  const atBottom = flow.scrollTop + flow.clientHeight >= flow.scrollHeight - 4;
  for (const ev of queue) apply(ev);
  queue = [];
  if (atBottom) toBottom();
}

function apply(ev) {
  switch (ev.type) {
    case "scroll_out": {
      const line = wrapped ?? newLine();
      line.append(...spans(ev.line.runs));
      if (!wrapped) flow.insertBefore(line, screen);
      wrapped = ev.line.wrap ? line : null;
      break;
    }
    case "lines": {
      const grid = altOn ? alt : screen;
      while (grid.children.length > ev.height) grid.lastChild.remove();
      while (grid.children.length < ev.height) grid.append(newLine());
      for (const row of ev.rows) grid.children[row.y].replaceChildren(...spans(row.runs));
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
    span.textContent = run.text;
    if (run.class) span.className = run.class;
    if (run.fg) span.style.color = run.fg;
    if (run.bg) span.style.backgroundColor = run.bg;
    return span;
  });
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
resize();

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

function shortcut(e) {
  const key = e.key.toLowerCase();
  if (e.ctrlKey && e.shiftKey && key === "c") {
    document.execCommand("copy");
  } else if (e.ctrlKey && e.shiftKey && key === "v") {
    input.focus();
    document.execCommand("paste");
  } else if (e.shiftKey && (e.key === "PageUp" || e.key === "PageDown")) {
    flow.scrollBy(0, (e.key === "PageUp" ? -0.9 : 0.9) * flow.clientHeight);
  } else {
    return false;
  }
  return true;
}

document.addEventListener("keydown", (e) => {
  if (e.isComposing) return;
  if (shortcut(e)) {
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
});

function type(data) {
  send({ type: "input", data });
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
// would clear it before it can be copied).
document.addEventListener("mouseup", () => {
  if (window.getSelection().isCollapsed) input.focus({ preventScroll: true });
});
input.focus();
