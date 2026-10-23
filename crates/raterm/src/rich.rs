//! Rich output blocks, sent by programs as `ESC ] 7117 ; kind [; k=v]* ; base64 BEL`.
//!
//! The filter that takes them out of the output also takes the sequences of
//! the graphics protocols, and the queries `alacritty_terminal` leaves out.

use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use comrak::plugins::syntect::SyntectAdapter;
use serde::Serialize;

use crate::media::Media;

/// Maximum size of a sequence payload; larger blocks are dropped.
pub const MAX_PAYLOAD: usize = 8 << 20;
/// Maximum size of the data of a Sixel image.
pub const MAX_SIXEL: usize = crate::graphics::MAX_IMAGE;
const PREFIX: &[u8] = b"\x1b]7117;";
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
/// Longest parameters of the CSI and DCS sequences the filter takes.
const MAX_PARAMS: usize = 32;

/// Pieces of output, in order: queries are answered in order with the
/// replies of `alacritty_terminal`.
#[derive(Debug, PartialEq)]
pub enum Piece {
    /// Regular terminal output.
    Bytes(Vec<u8>),
    Block(Block),
    /// `CSI 16 t`: the size of a cell in pixels.
    CellSize,
    /// `CSI ? Pi ; Pa ; Pv S` (XTSMGRAPHICS), its parameters.
    Graphics(Vec<usize>),
    /// `DCS P1 ; P2 ; P3 q … ST`: a Sixel image, its data after `q`.
    Sixel(Vec<u8>),
}

/// What the bytes held by the filter start.
enum Prefix {
    Partial,
    Other,
    Block,
    CellSize,
    Graphics(Vec<usize>),
    Sixel,
}

impl Prefix {
    fn of(buf: &[u8]) -> Prefix {
        let param = |b: &u8| b.is_ascii_digit() || *b == b';';
        match buf.get(1) {
            None => Prefix::Partial,
            Some(b']') if !PREFIX.starts_with(buf) => Prefix::Other,
            Some(b']') if buf.len() == PREFIX.len() => Prefix::Block,
            Some(b']') => Prefix::Partial,
            Some(&kind @ (b'[' | b'P')) => {
                let rest = &buf[2..];
                // `?` opens the parameters of XTSMGRAPHICS.
                let params = match rest.strip_prefix(b"?") {
                    Some(params) if kind == b'[' => params,
                    _ => rest,
                };
                let private = params.len() < rest.len();
                // Colors and moves flood the output: they go on at once.
                if kind == b'[' && !private && !b"16t".starts_with(params) {
                    return Prefix::Other;
                }
                let Some((&last, body)) = params.split_last() else {
                    return Prefix::Partial;
                };
                if param(&last) {
                    return match rest.len() > MAX_PARAMS {
                        true => Prefix::Other,
                        false => Prefix::Partial,
                    };
                }
                match (kind, private, last) {
                    (b'[', false, b't') if body == b"16" => Prefix::CellSize,
                    (b'[', true, b'S') => Prefix::Graphics(numbers(body)),
                    (b'P', false, b'q') => Prefix::Sixel,
                    _ => Prefix::Other,
                }
            }
            Some(_) => Prefix::Other,
        }
    }
}

/// The numbers of `1;;3`, missing ones as 0.
pub fn numbers(params: &[u8]) -> Vec<usize> {
    params
        .split(|&b| b == b';')
        .map(|n| {
            n.iter().fold(0usize, |v, d| {
                v.saturating_mul(10).saturating_add((d - b'0') as usize)
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Html,
    Md,
    Svg,
    Img,
    Video,
    Audio,
}

#[derive(Debug, PartialEq)]
pub struct Block {
    pub kind: Kind,
    pub id: Option<String>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub mime: Option<String>,
    /// `src=path`: `data` is the absolute path of a local file, not its bytes.
    pub path: bool,
    pub data: Vec<u8>,
}

/// A block as sent to the view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rendered {
    pub kind: Kind,
    /// HTML for `html` and `md`, a data URL for `svg` and `img`, a data URL
    /// or `media/<n>` (a file of the media server) for `video` and `audio`.
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<u16>,
    /// Plain text copied in place of the block (the Markdown source).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    /// `buf` holds the start of an escape sequence the filter may take.
    Prefix,
    /// `buf` holds the payload read so far.
    Payload,
    /// ESC seen inside the payload, expecting `\` (ST).
    PayloadEsc,
}

/// Extracts rich blocks, Sixel images and queries from a pty byte stream,
/// across read boundaries.
#[derive(Default)]
pub struct Filter {
    state: State,
    buf: Vec<u8>,
    /// The payload is a Sixel image's, rather than a block's.
    sixel: bool,
    overflow: bool,
}

impl Filter {
    pub fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut text = Vec::new();
        let mut i = 0;
        while i < input.len() {
            // Text up to the next escape goes on at once.
            if self.state == State::Ground {
                let end =
                    (input[i..].iter().position(|&b| b == ESC)).map_or(input.len(), |n| i + n);
                text.extend_from_slice(&input[i..end]);
                i = end;
                if i == input.len() {
                    break;
                }
            }
            self.step(input[i], &mut text, &mut out);
            i += 1;
        }
        if !text.is_empty() {
            out.push(Piece::Bytes(text));
        }
        out
    }

    fn step(&mut self, b: u8, text: &mut Vec<u8>, out: &mut Vec<Piece>) {
        match self.state {
            State::Ground if b == ESC => {
                self.state = State::Prefix;
                self.buf.push(b);
            }
            State::Ground => text.push(b),
            State::Prefix => {
                self.buf.push(b);
                let piece = match Prefix::of(&self.buf) {
                    Prefix::Partial => return,
                    Prefix::Other => {
                        // Not ours: give the bytes back and look at `b` again.
                        self.buf.pop();
                        text.append(&mut self.buf);
                        self.state = State::Ground;
                        return self.step(b, text, out);
                    }
                    Prefix::Block | Prefix::Sixel => {
                        self.sixel = self.buf[1] == b'P';
                        self.buf.clear();
                        self.state = State::Payload;
                        return;
                    }
                    Prefix::CellSize => Piece::CellSize,
                    Prefix::Graphics(params) => Piece::Graphics(params),
                };
                self.reset();
                push(text, out, piece);
            }
            State::Payload => match b {
                BEL if !self.sixel => self.finish(text, out),
                ESC => self.state = State::PayloadEsc,
                _ if self.overflow => {}
                _ => {
                    self.buf.push(b);
                    let max = if self.sixel { MAX_SIXEL } else { MAX_PAYLOAD };
                    if self.buf.len() > max {
                        eprintln!("raterm: sequence over {max} bytes, ignored");
                        self.overflow = true;
                        self.buf = Vec::new();
                    }
                }
            },
            State::PayloadEsc if b == b'\\' => self.finish(text, out),
            State::PayloadEsc => {
                // Any other escape aborts the string and starts a new sequence.
                eprintln!("raterm: unterminated sequence, ignored");
                self.reset();
                self.state = State::Prefix;
                self.buf.push(ESC);
                self.step(b, text, out);
            }
        }
    }

    fn finish(&mut self, text: &mut Vec<u8>, out: &mut Vec<Piece>) {
        if self.overflow {
        } else if self.sixel {
            push(text, out, Piece::Sixel(std::mem::take(&mut self.buf)));
        } else {
            match parse(&self.buf) {
                Ok(block) => push(text, out, Piece::Block(block)),
                Err(e) => eprintln!("raterm: invalid rich block: {e}"),
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.sixel = false;
        self.overflow = false;
    }
}

/// Pushes `piece` after the text before it.
fn push(text: &mut Vec<u8>, out: &mut Vec<Piece>, piece: Piece) {
    if !text.is_empty() {
        out.push(Piece::Bytes(std::mem::take(text)));
    }
    out.push(piece);
}

fn parse(payload: &[u8]) -> Result<Block, String> {
    let payload = std::str::from_utf8(payload).map_err(|e| e.to_string())?;
    let mut parts: Vec<&str> = payload.split(';').collect();
    let data = parts
        .pop()
        .filter(|_| !parts.is_empty())
        .ok_or("no payload")?;
    let kind = match parts[0] {
        "html" => Kind::Html,
        "md" => Kind::Md,
        "svg" => Kind::Svg,
        "img" => Kind::Img,
        "video" => Kind::Video,
        "audio" => Kind::Audio,
        other => return Err(format!("unknown kind {other:?}")),
    };
    let mut block = Block {
        kind,
        id: None,
        width: None,
        height: None,
        mime: None,
        path: false,
        data: STANDARD.decode(data.trim()).map_err(|e| e.to_string())?,
    };
    for param in &parts[1..] {
        let (key, value) = param.split_once('=').ok_or("parameter without '='")?;
        let number = || value.parse().map_err(|_| format!("bad {key}: {value:?}"));
        match key {
            "id" => block.id = Some(value.to_owned()),
            "w" => block.width = Some(number()?),
            "h" => block.height = Some(number()?),
            "mime" => block.mime = Some(value.to_owned()),
            "src" if value == "path" => block.path = true,
            "src" => return Err(format!("bad src: {value:?}")),
            _ => {} // unknown keys are reserved for later versions
        }
    }
    Ok(block)
}

impl Block {
    /// Renders the block; local files of `src=path` videos and audio are
    /// registered in `media`, which the view serves them from.
    pub fn render(self, media: &Media) -> Result<Rendered, String> {
        let (content, text) = match self.kind {
            Kind::Html => (String::from_utf8_lossy(&self.data).into_owned(), None),
            Kind::Md => {
                let source = String::from_utf8_lossy(&self.data).into_owned();
                let mut options = comrak::Options::default();
                options.extension.table = true;
                options.extension.autolink = true;
                // Raw HTML is fine: the result is sandboxed exactly like `html`.
                options.render.r#unsafe = true;
                let mut plugins = comrak::options::Plugins::default();
                plugins.render.codefence_syntax_highlighter = Some(highlighter());
                let html = comrak::markdown_to_html_with_plugins(&source, &options, &plugins);
                (html, Some(source))
            }
            Kind::Svg => (data_url("image/svg+xml", &self.data), None),
            Kind::Img => (data_url(mime(&self, "image/")?, &self.data), None),
            Kind::Video | Kind::Audio if self.path => {
                let mime = mime(&self, self.kind.media_type())?.to_owned();
                let path = std::path::PathBuf::from(String::from_utf8_lossy(&self.data).as_ref());
                if !path.is_absolute() || !path.is_file() {
                    return Err(format!(
                        "not an absolute path to a file: {}",
                        path.display()
                    ));
                }
                (media.add(path, mime), None)
            }
            Kind::Video | Kind::Audio => (
                data_url(mime(&self, self.kind.media_type())?, &self.data),
                None,
            ),
        };
        Ok(Rendered {
            kind: self.kind,
            content,
            id: self.id,
            w: self.width,
            h: self.height,
            text,
        })
    }
}

impl Kind {
    /// The MIME type prefix of a video or audio block.
    fn media_type(self) -> &'static str {
        if self == Kind::Audio {
            "audio/"
        } else {
            "video/"
        }
    }
}

/// The block's MIME type, which must be `prefix` (`image/`...) and a subtype.
fn mime<'a>(block: &'a Block, prefix: &str) -> Result<&'a str, String> {
    let mime = block.mime.as_deref().ok_or("no mime")?;
    let valid = mime.strip_prefix(prefix).is_some_and(|sub| {
        !sub.is_empty()
            && sub
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".+-".contains(&c))
    });
    valid
        .then_some(mime)
        .ok_or_else(|| format!("bad mime {mime:?}"))
}

/// Highlights fenced code blocks with CSS classes named after syntax scopes
/// (`keyword`, `string`...), which the view colors with the terminal palette.
/// Loading the syntax definitions takes a while: only once.
fn highlighter() -> &'static SyntectAdapter {
    static HIGHLIGHTER: OnceLock<SyntectAdapter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| SyntectAdapter::new(None))
}

fn data_url(mime: &str, data: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(kind: Kind, data: &[u8]) -> Piece {
        Piece::Block(Block {
            kind,
            id: None,
            width: None,
            height: None,
            mime: None,
            path: false,
            data: data.to_vec(),
        })
    }

    #[test]
    fn extracts_block_between_text() {
        let pieces = Filter::default().feed(b"a\x1b]7117;html;PGI+\x07b");
        assert_eq!(
            pieces,
            [
                Piece::Bytes(b"a".to_vec()),
                block(Kind::Html, b"<b>"),
                Piece::Bytes(b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn survives_any_split_and_st_terminator() {
        let input = b"x\x1b]7117;md;id=t;w=40;IyBo\x1b\\y";
        for cut in 0..input.len() {
            let mut filter = Filter::default();
            let mut pieces = filter.feed(&input[..cut]);
            pieces.extend(filter.feed(&input[cut..]));
            let text: Vec<u8> = pieces
                .iter()
                .flat_map(|p| match p {
                    Piece::Bytes(b) => b.clone(),
                    _ => Vec::new(),
                })
                .collect();
            assert_eq!(text, b"xy", "cut at {cut}");
            let blocks: Vec<_> = pieces
                .into_iter()
                .filter_map(|p| match p {
                    Piece::Block(b) => Some(b),
                    _ => None,
                })
                .collect();
            assert_eq!(blocks.len(), 1, "cut at {cut}");
            assert_eq!(blocks[0].id.as_deref(), Some("t"));
            assert_eq!(blocks[0].width, Some(40));
            assert_eq!(blocks[0].data, b"# h");
        }
    }

    #[test]
    fn extracts_cell_size_query_across_splits() {
        let input = b"a\x1b[16tb";
        for cut in 0..input.len() {
            let mut filter = Filter::default();
            let mut pieces = filter.feed(&input[..cut]);
            pieces.extend(filter.feed(&input[cut..]));
            pieces.retain(|p| p != &Piece::Bytes(Vec::new()));
            let at = pieces.iter().position(|p| p == &Piece::CellSize);
            assert!(at.is_some(), "cut at {cut}");
            let text = |ps: &[Piece]| -> Vec<u8> {
                ps.iter()
                    .flat_map(|p| match p {
                        Piece::Bytes(b) => b.clone(),
                        _ => Vec::new(),
                    })
                    .collect()
            };
            let (before, after) = pieces.split_at(at.unwrap());
            assert_eq!(text(before), b"a", "cut at {cut}");
            assert_eq!(text(after), b"b", "cut at {cut}");
        }
    }

    #[test]
    fn extracts_sixel_and_graphics_queries() {
        let input = b"a\x1bP0;1;0q#0~\x1b\\b\x1b[?2;1;0Sc";
        for cut in 0..input.len() {
            let mut filter = Filter::default();
            let mut pieces = filter.feed(&input[..cut]);
            pieces.extend(filter.feed(&input[cut..]));
            let mut text = Vec::new();
            let mut taken = Vec::new();
            for piece in pieces {
                match piece {
                    Piece::Bytes(b) => text.extend(b),
                    other => taken.push(other),
                }
            }
            assert_eq!(text, b"abc", "cut at {cut}");
            assert_eq!(
                taken,
                [
                    Piece::Sixel(b"#0~".to_vec()),
                    Piece::Graphics(vec![2, 1, 0])
                ],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn other_sequences_pass_through() {
        // DECRQSS, a DCS too, as yazi sends it; private modes; colors.
        let input = b"\x1b]7;file:///tmp\x07\x1b]0;title\x07\x1b[31m\x1b[14t\x1b[166t\
            \x1bP$q q\x1b\\\x1b[?25h\x1b[?1049l\x1b[38;2;1;2;3m\x1b[6 q";
        assert_eq!(
            Filter::default().feed(input),
            [Piece::Bytes(input.to_vec())]
        );
    }

    #[test]
    fn invalid_and_oversized_blocks_are_dropped() {
        let mut filter = Filter::default();
        assert_eq!(filter.feed(b"\x1b]7117;nope;AA==\x07"), []);
        assert_eq!(filter.feed(b"\x1b]7117;html;!!\x07"), []);
        let mut big = b"\x1b]7117;html;".to_vec();
        big.resize(big.len() + MAX_PAYLOAD + 1, b'A');
        big.extend_from_slice(b"\x07ok");
        assert_eq!(filter.feed(&big), [Piece::Bytes(b"ok".to_vec())]);
    }

    #[test]
    fn renders_markdown_tables() {
        let Piece::Block(b) = block(Kind::Md, b"|a|b|\n|-|-|\n|1|2|\n") else {
            unreachable!()
        };
        let rendered = b.render(&Media::default()).unwrap();
        assert!(rendered.content.contains("<table>"));
        assert_eq!(rendered.text.as_deref(), Some("|a|b|\n|-|-|\n|1|2|\n"));
    }

    #[test]
    fn code_blocks_are_highlighted() {
        let Piece::Block(b) = block(Kind::Md, b"```rust\nfn main() {}\n```\n") else {
            unreachable!()
        };
        let html = b.render(&Media::default()).unwrap().content;
        assert!(
            html.contains(r#"<span class="storage type function rust">fn</span>"#),
            "{html}"
        );
    }

    #[test]
    fn bare_urls_become_links() {
        let Piece::Block(b) = block(Kind::Md, b"see https://example.com\n") else {
            unreachable!()
        };
        let html = b.render(&Media::default()).unwrap().content;
        assert!(html.contains(r#"<a href="https://example.com">"#), "{html}");
    }

    #[test]
    fn local_videos_are_registered_not_inlined() {
        let path = std::env::current_exe().unwrap();
        let payload = format!(
            "video;mime=video/mp4;src=path;{}",
            STANDARD.encode(path.to_str().unwrap())
        );
        let block = parse(payload.as_bytes()).unwrap();
        let media = Media::default();
        assert_eq!(block.render(&media).unwrap().content, "media/0");
        assert_eq!(media.get(0).unwrap(), (path, "video/mp4".to_owned()));
        let relative = format!("video;mime=video/mp4;src=path;{}", STANDARD.encode("a.mp4"));
        assert!(parse(relative.as_bytes()).unwrap().render(&media).is_err());
    }

    #[test]
    fn audio_is_a_media_of_its_own_type() {
        let path = std::env::current_exe().unwrap();
        let payload = format!(
            "audio;mime=audio/ogg;src=path;{}",
            STANDARD.encode(path.to_str().unwrap())
        );
        let media = Media::default();
        let rendered = parse(payload.as_bytes()).unwrap().render(&media).unwrap();
        assert_eq!(
            (rendered.kind, rendered.content.as_str()),
            (Kind::Audio, "media/0")
        );
        assert_eq!(media.get(0).unwrap(), (path, "audio/ogg".to_owned()));
        let inline = parse(b"audio;mime=audio/mpeg;AQI=").unwrap();
        assert_eq!(
            inline.render(&media).unwrap().content,
            "data:audio/mpeg;base64,AQI="
        );
        let video_type = parse(b"audio;mime=video/mp4;AQI=").unwrap();
        assert!(video_type.render(&media).is_err());
    }

    #[test]
    fn image_mime_is_validated() {
        let img = |mime: &str| Block {
            kind: Kind::Img,
            id: None,
            width: None,
            height: None,
            mime: Some(mime.into()),
            path: false,
            data: vec![1, 2],
        };
        assert_eq!(
            img("image/png").render(&Media::default()).unwrap().content,
            "data:image/png;base64,AQI="
        );
        assert!(img("text/html").render(&Media::default()).is_err());
        assert!(img("image/png\"onerror").render(&Media::default()).is_err());
    }
}
