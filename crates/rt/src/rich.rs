//! Rich output blocks, sent by programs as `ESC ] 7117 ; kind [; k=v]* ; base64 BEL`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

/// Maximum size of a sequence payload; larger blocks are dropped.
pub const MAX_PAYLOAD: usize = 8 << 20;
const PREFIX: &[u8] = b"\x1b]7117;";
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;

#[derive(Debug, PartialEq)]
pub enum Piece {
    /// Regular terminal output.
    Bytes(Vec<u8>),
    Block(Block),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Html,
    Md,
    Svg,
    Img,
}

#[derive(Debug, PartialEq)]
pub struct Block {
    pub kind: Kind,
    pub id: Option<String>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub mime: Option<String>,
    pub data: Vec<u8>,
}

/// A block as sent to the view.
#[derive(Debug, PartialEq, Serialize)]
pub struct Rendered {
    pub kind: Kind,
    /// HTML for `html` and `md`, a data URL for `svg` and `img`.
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
    /// `buf` holds a strict prefix of `PREFIX`.
    Prefix,
    /// `buf` holds the payload read so far.
    Payload,
    /// ESC seen inside the payload, expecting `\` (ST).
    PayloadEsc,
}

/// Extracts rich blocks from a pty byte stream, across read boundaries.
#[derive(Default)]
pub struct Filter {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
}

impl Filter {
    pub fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut text = Vec::new();
        for &b in input {
            self.step(b, &mut text, &mut out);
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
            State::Prefix if b == PREFIX[self.buf.len()] => {
                self.buf.push(b);
                if self.buf.len() == PREFIX.len() {
                    self.buf.clear();
                    self.state = State::Payload;
                }
            }
            State::Prefix => {
                // Not ours: give the bytes back and look at `b` again.
                text.append(&mut self.buf);
                self.state = State::Ground;
                self.step(b, text, out);
            }
            State::Payload => match b {
                BEL => self.finish(text, out),
                ESC => self.state = State::PayloadEsc,
                _ if self.overflow => {}
                _ => {
                    self.buf.push(b);
                    if self.buf.len() > MAX_PAYLOAD {
                        eprintln!("rt: rich block over {MAX_PAYLOAD} bytes, ignored");
                        self.overflow = true;
                        self.buf = Vec::new();
                    }
                }
            },
            State::PayloadEsc if b == b'\\' => self.finish(text, out),
            State::PayloadEsc => {
                // Any other escape aborts the string and starts a new sequence.
                eprintln!("rt: unterminated rich block, ignored");
                self.reset();
                self.state = State::Prefix;
                self.buf.push(ESC);
                self.step(b, text, out);
            }
        }
    }

    fn finish(&mut self, text: &mut Vec<u8>, out: &mut Vec<Piece>) {
        if !self.overflow {
            match parse(&self.buf) {
                Ok(block) => {
                    if !text.is_empty() {
                        out.push(Piece::Bytes(std::mem::take(text)));
                    }
                    out.push(Piece::Block(block));
                }
                Err(e) => eprintln!("rt: invalid rich block: {e}"),
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflow = false;
    }
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
        other => return Err(format!("unknown kind {other:?}")),
    };
    let mut block = Block {
        kind,
        id: None,
        width: None,
        height: None,
        mime: None,
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
            _ => {} // unknown keys are reserved for later versions
        }
    }
    Ok(block)
}

impl Block {
    pub fn render(self) -> Result<Rendered, String> {
        let (content, text) = match self.kind {
            Kind::Html => (String::from_utf8_lossy(&self.data).into_owned(), None),
            Kind::Md => {
                let source = String::from_utf8_lossy(&self.data).into_owned();
                let mut options = comrak::Options::default();
                options.extension.table = true;
                options.extension.autolink = true;
                // Raw HTML is fine: the result is sandboxed exactly like `html`.
                options.render.r#unsafe = true;
                (comrak::markdown_to_html(&source, &options), Some(source))
            }
            Kind::Svg => (data_url("image/svg+xml", &self.data), None),
            Kind::Img => {
                let mime = self.mime.as_deref().ok_or("img without mime")?;
                let valid = mime.strip_prefix("image/").is_some_and(|sub| {
                    !sub.is_empty()
                        && sub
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".+-".contains(&c))
                });
                if !valid {
                    return Err(format!("bad image mime {mime:?}"));
                }
                (data_url(mime, &self.data), None)
            }
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
                    Piece::Block(_) => Vec::new(),
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
    fn other_sequences_pass_through() {
        let input = b"\x1b]7;file:///tmp\x07\x1b]0;title\x07\x1b[31m";
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
        let rendered = b.render().unwrap();
        assert!(rendered.content.contains("<table>"));
        assert_eq!(rendered.text.as_deref(), Some("|a|b|\n|-|-|\n|1|2|\n"));
    }

    #[test]
    fn bare_urls_become_links() {
        let Piece::Block(b) = block(Kind::Md, b"see https://example.com\n") else {
            unreachable!()
        };
        let html = b.render().unwrap().content;
        assert!(html.contains(r#"<a href="https://example.com">"#), "{html}");
    }

    #[test]
    fn image_mime_is_validated() {
        let img = |mime: &str| Block {
            kind: Kind::Img,
            id: None,
            width: None,
            height: None,
            mime: Some(mime.into()),
            data: vec![1, 2],
        };
        assert_eq!(
            img("image/png").render().unwrap().content,
            "data:image/png;base64,AQI="
        );
        assert!(img("text/html").render().is_err());
        assert!(img("image/png\"onerror").render().is_err());
    }
}
