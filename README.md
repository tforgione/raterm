<p align="center">
  <img src="https://git.tforgione.fr/tforgione/raterm/wiki/raw/raterm.png" alt="raterm" width="160">
</p>

# raterm

A terminal emulator for Linux, written in Rust, whose screen is a web view.
Regular output (text, ANSI colors, cursor, full-screen programs like vim or
htop) works as in any terminal, but programs can also insert rich blocks at the
cursor: rendered Markdown, HTML, SVG, images and videos.

The original itch: reading Markdown reports with wide tables in a terminal,
without truncation and without switching to a browser.

```sh
rat report.md      # a rendered Markdown block, wide tables included
rat diagram.svg
rat photo.png
rat clip.mp4
```

`rat` is its companion tool, a pun on `cat`: it shows a file as a rich block in
raterm, and falls back to plain text anywhere else.

## Installing

raterm runs on Linux only, on X11 and Wayland. It needs WebKitGTK 4.1 and, for
videos, GStreamer's good plugins (plus libav for H.264). On Arch Linux:

```sh
sudo pacman -S webkit2gtk-4.1 gst-plugins-good gst-libav
```

Then build and install both binaries with a recent stable Rust (edition 2024):

```sh
git clone https://git.tforgione.fr/tforgione/raterm
cd raterm
cargo install --path crates/raterm
cargo install --path crates/rat
```

`extra/raterm.desktop` registers raterm as a terminal emulator for application
launchers, with the icon `extra/raterm.png`:

```sh
cp extra/raterm.desktop ~/.local/share/applications/
cp extra/raterm.png ~/.local/share/icons/hicolor/256x256/apps/
```

## Usage

```sh
raterm                                  # runs $SHELL
raterm --title NAME -e COMMAND ARGS...  # same options as alacritty
```

| Option | |
| --- | --- |
| `-T`, `--title TITLE` | initial window title (default `raterm`) |
| `-e`, `--command COMMAND...` | program and arguments to run instead of the shell; takes all the remaining arguments |

### Default shortcuts

| Keys | Action |
| --- | --- |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | copy / paste |
| `Shift+Insert`, middle click | paste the primary selection |
| `Ctrl+Shift+F` | search the scrollback (`Enter`: older match, `Shift+Enter`: newer, `Esc`: close) |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | zoom in / out / reset |
| `Shift+PageUp` / `Shift+PageDown` | scroll the scrollback (sent to the program on the alternate screen) |
| `Ctrl+click` | open a link (rich block links open on a plain click) |
| click on an image | show it full window (`Esc` or any click to close) |

## rat

```sh
rat FILE              # kind guessed from the extension
rat --md -            # from stdin
```

| Option | |
| --- | --- |
| `--md`, `--html`, `--svg`, `--img`, `--video` | kind of the input, instead of guessing from the extension |
| `--width N`, `--height N` | maximum size, in columns and lines |
| `--id NAME` | replace the previous block with the same id (progress bars, tables that fill up) |
| `--force` | emit the block even when not running in raterm |

Images can be PNG, JPEG, GIF or WebP; videos MP4, WebM, MKV or OGV. A local
video is sent by path, so it can be of any size; from stdin it is sent whole
(8 MiB at most). Over ssh, `rat` refuses to send videos.

Outside raterm, `rat` prints Markdown and HTML as is, and the path of images
and videos. Note that ssh does not forward `RATERM` by default: on a remote
machine, `rat` only emits blocks with `--force`, or if the variable is
forwarded (`SendEnv` on the client, `AcceptEnv` on the server).

## Configuration

raterm reads `$XDG_CONFIG_HOME/raterm/config.toml` (usually
`~/.config/raterm/config.toml`). Every key is optional; the values below are
the defaults, except for the commented-out lines and the `[keys]` examples:

```toml
# shell = "/bin/zsh"      # defaults to $SHELL
scrollback = 10000        # lines and blocks kept
bold_is_bright = false    # bold text in the 8 normal colors uses their bright variant
dynamic_title = true      # the window title follows the program's (OSC 0/2)

[font]
family = "monospace"      # as fontconfig names it: see `fc-list : family`
size = 10.5               # in points

[cursor]
style = "block"           # block, bar or underline; hollow when unfocused
blink = false

[window]
decorations = true

[images]
# max_height = 30         # in lines, for images, SVG and videos; unlimited if unset

[markdown]
max_width = 100           # width of the centered column of text, in columns; 0: no limit

[keys]                    # merged over the default shortcuts; examples:
"ctrl+shift+e" = "new_window"
"f13" = { send = "\u001b[25~" }    # text sent to the program
"shift+insert" = "none"            # removes a default
```

Shortcut actions are `copy`, `paste`, `paste_selection`, `search`, `zoom_in`,
`zoom_out`, `zoom_reset`, `scroll_page_up`, `scroll_page_down`, `new_window`
(a new window in the shell's current directory) and `none`.

Colors, and any other style, go in `user.css` next to `config.toml`, loaded
after raterm's own stylesheet:

```css
:root {
  --fg: #eaeaea;
  --bg: #000000;
  --cursor: #eaeaea;
  --selection: var(--fg);       /* selected text is inverted by default */
  --selection-text: var(--bg);
  --c0: #000000;                /* the 16 ANSI colors, --c0 to --c15 */
  --c1: #cd0000;
  /* ... */
}
```

## Emitting rich blocks

A rich block is a single escape sequence, with a base64 payload so that its
content never needs escaping:

```
ESC ] 7117 ; <kind> [; <key>=<value> ...] ; <base64> BEL
```

`ST` (`ESC \`) works as a terminator too.

| Kind | Payload | Shown as |
| --- | --- | --- |
| `md` | Markdown (CommonMark, GFM tables and autolinks) | rendered HTML |
| `html` | an HTML fragment | HTML |
| `svg` | an SVG document | an image |
| `img` | image bytes, with `mime=image/...` | an image |
| `video` | video bytes, or with `src=path` the absolute path of a local file; `mime=video/...` | a video player |

Optional keys are `w` (maximum width, in columns), `h` (maximum height, in
lines) and `id`. Payloads over 8 MiB are dropped. raterm exports `RATERM=1` and
`TERM_PROGRAM=raterm`, so that programs can fall back to plain text elsewhere.

Without `rat`, a few lines of shell are enough:

```sh
# Shows a Markdown file as a rich block in raterm, as plain text elsewhere.
md() {
    if [ -n "$RATERM" ]; then
        printf '\033]7117;md;%s\a' "$(base64 -w0 < "$1")"
    else
        cat "$1"
    fi
}
```

## Security

Programs writing to the terminal never run code in raterm:

- HTML and Markdown blocks are rendered in sandboxed iframes, without
  `allow-scripts`. SVG is shown as an image, which runs no script either.
- The page's content security policy forbids any network access, apart from
  raterm's own local video server: blocks can only show their own content and
  inline data.
- Links open in the desktop's handler (`xdg-open`), never in the terminal, and
  only `http`, `https` and `mailto` links.
- Local videos are streamed by a small HTTP server on `127.0.0.1`, which only
  serves the files announced by a video block, under a random 128-bit token.

## Development

```sh
cargo test       # no window needed
cargo clippy -- -D warnings
```

The core (`pty`, `term`, `rich`, `media`) is a library tested without a
window; `view` is the only module that knows about wry. Snapshot tests replay
recorded sessions from `crates/raterm/tests/data` (recorded with `record.sh`
there); `RATERM_UPDATE_SNAPSHOTS=1 cargo test` rewrites them. `SPEC.md`
describes the design (in French).

## License

AGPL-3.0-only.
