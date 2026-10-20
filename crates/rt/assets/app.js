"use strict";

const out = document.getElementById("out");
const send = (msg) => window.ipc.postMessage(JSON.stringify(msg));

window.rt = {
  event(ev) {
    if (ev.type === "raw") {
      out.append(ev.data);
      window.scrollTo(0, document.body.scrollHeight);
    }
  },
};

const KEYS = {
  Enter: "\r",
  Backspace: "\x7f",
  Tab: "\t",
  Escape: "\x1b",
  ArrowUp: "\x1b[A",
  ArrowDown: "\x1b[B",
  ArrowRight: "\x1b[C",
  ArrowLeft: "\x1b[D",
  Home: "\x1b[H",
  End: "\x1b[F",
  Delete: "\x1b[3~",
};

function encode(e) {
  if (e.ctrlKey && e.key.length === 1) {
    const code = e.key.toUpperCase().charCodeAt(0);
    if (code >= 0x40 && code <= 0x5f) return String.fromCharCode(code - 0x40);
  }
  if (e.key in KEYS) return KEYS[e.key];
  if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) return (e.altKey ? "\x1b" : "") + e.key;
  return null;
}

document.addEventListener("keydown", (e) => {
  const data = encode(e);
  if (data !== null) {
    e.preventDefault();
    send({ type: "input", data });
  }
});
