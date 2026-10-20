//! Shows a file as a rich block in raterm, or as plain text anywhere else.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use clap::{ArgGroup, Parser};

/// Must match raterm's limit, which drops larger blocks.
const MAX_PAYLOAD: usize = 8 << 20;

#[derive(Parser)]
#[command(about = "Show a file as a rich block in the raterm terminal")]
#[command(group(ArgGroup::new("kind").args(["md", "html", "svg", "img", "video"])))]
struct Args {
    /// File to show, or `-` for stdin.
    file: PathBuf,
    /// Treat the input as Markdown.
    #[arg(long)]
    md: bool,
    /// Treat the input as HTML.
    #[arg(long)]
    html: bool,
    /// Treat the input as SVG.
    #[arg(long)]
    svg: bool,
    /// Treat the input as an image (png, jpg, gif or webp).
    #[arg(long)]
    img: bool,
    /// Treat the input as a video (mp4, webm, mkv or ogv). A file is sent by
    /// path, so it can be of any size, stdin is sent whole; refused over ssh.
    #[arg(long)]
    video: bool,
    /// Maximum width, in columns.
    #[arg(long, value_name = "N")]
    width: Option<u16>,
    /// Maximum height, in lines.
    #[arg(long, value_name = "N")]
    height: Option<u16>,
    /// Replace the previous block with the same id.
    #[arg(long, value_name = "NAME")]
    id: Option<String>,
    /// Download the http(s) images of Markdown files, to inline them like
    /// local ones (raterm itself never accesses the network).
    #[arg(long)]
    allow_network: bool,
    /// Emit the block even when not running in raterm.
    #[arg(long)]
    force: bool,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rat: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    if let Some(id) = &args.id
        && id.chars().any(|c| c == ';' || c.is_control())
    {
        return Err("the id cannot contain ';' or control characters".into());
    }
    let stdin = args.file == Path::new("-");
    let kind = if args.md {
        "md"
    } else if args.html {
        "html"
    } else if args.svg {
        "svg"
    } else if args.img {
        "img"
    } else if args.video {
        "video"
    } else {
        let ext = args.file.extension().and_then(|e| e.to_str());
        match ext.map(str::to_ascii_lowercase).as_deref() {
            Some("md" | "markdown") => "md",
            Some("html" | "htm") => "html",
            Some("svg") => "svg",
            Some("png" | "jpg" | "jpeg" | "gif" | "webp") => "img",
            Some("mp4" | "m4v" | "webm" | "mkv" | "ogv") => "video",
            _ => {
                return Err(
                    "cannot guess the kind, use --md, --html, --svg, --img or --video".into(),
                );
            }
        }
    };

    let in_raterm = std::env::var_os("RATERM").is_some();
    let emit = in_raterm || args.force;
    // Over ssh, raterm cannot read a remote file by path, and sending the video
    // whole would push megabytes through the connection.
    if emit && kind == "video" && std::env::var_os("SSH_CONNECTION").is_some() {
        return Err("videos cannot be shown over ssh".into());
    }
    // A local video is sent by path: raterm reads it itself, so it can be large.
    let by_path = kind == "video" && !stdin;
    let error = |e: io::Error| format!("{}: {e}", args.file.display());
    let data = if stdin {
        let mut data = Vec::new();
        io::stdin().read_to_end(&mut data).map(|_| data)
    } else if by_path {
        // Only the start, to recognize the format.
        let mut head = Vec::new();
        File::open(&args.file)
            .and_then(|f| f.take(16).read_to_end(&mut head))
            .map(|_| head)
    } else {
        std::fs::read(&args.file)
    }
    .map_err(error)?;
    let mime = match kind {
        "img" | "video" => Some(
            sniff(&data)
                .filter(|mime| mime.starts_with(if kind == "img" { "image/" } else { "video/" }))
                .ok_or(format!("unsupported {kind} format"))?,
        ),
        _ => None,
    };

    let mut out = io::stdout().lock();
    let written = if emit {
        let payload = if by_path {
            let path = std::fs::canonicalize(&args.file).map_err(error)?;
            STANDARD.encode(path.as_os_str().as_encoded_bytes())
        } else if kind == "md" {
            let dir = match args.file.parent() {
                Some(dir) if !stdin && !dir.as_os_str().is_empty() => dir,
                _ => Path::new("."),
            };
            let markdown = String::from_utf8_lossy(&data);
            STANDARD.encode(inline_images(&markdown, dir, args.allow_network))
        } else {
            STANDARD.encode(&data)
        };
        if payload.len() > MAX_PAYLOAD {
            return Err(format!(
                "too large: {} bytes encoded, max {MAX_PAYLOAD}",
                payload.len()
            ));
        }
        let mut params = String::new();
        let mut param = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                params += &format!(";{key}={value}");
            }
        };
        param("mime", mime.map(String::from));
        param("src", by_path.then(|| "path".into()));
        param("w", args.width.map(|w| w.to_string()));
        param("h", args.height.map(|h| h.to_string()));
        param("id", args.id);
        write!(out, "\x1b]7117;{kind}{params};{payload}\x07")
    } else if kind == "md" || kind == "html" {
        out.write_all(&data)
    } else {
        writeln!(out, "{}", args.file.display())
    };
    written.and_then(|_| out.flush()).map_err(|e| e.to_string())
}

/// Markdown with its images inlined as data: URLs: blocks can neither read
/// files nor access the network, and paths only make sense where rat runs
/// (maybe over ssh). Code blocks and code spans are left alone.
fn inline_images(markdown: &str, dir: &Path, network: bool) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut fence = None;
    for line in markdown.split_inclusive('\n') {
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|m| line.trim_start().starts_with(m));
        match (fence, marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(open), Some(marker)) if open == marker => fence = None,
            (Some(_), _) => {}
            (None, None) => {
                // Splitting on backticks, odd pieces are code spans.
                for (i, piece) in line.split('`').enumerate() {
                    if i > 0 {
                        out.push('`');
                    }
                    if i % 2 == 0 {
                        out += &inline_in_text(piece, dir, network);
                    } else {
                        out += piece;
                    }
                }
                continue;
            }
        }
        out += line;
    }
    out
}

/// Replaces the destination of each `![alt](destination)` in `text`.
fn inline_in_text(text: &str, dir: &Path, network: bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("![") {
        let Some(open) = rest[start..].find("](").map(|i| start + i + 2) else {
            break;
        };
        out += &rest[..open];
        rest = &rest[open..];
        // `<path with spaces>`, or up to a space (before a title) or `)`.
        let (dest, len) = match rest.strip_prefix('<').and_then(|r| r.split_once('>')) {
            Some((dest, _)) => (dest, dest.len() + 2),
            None => {
                let end = rest
                    .find(|c: char| c == ')' || c.is_whitespace())
                    .unwrap_or(rest.len());
                (&rest[..end], end)
            }
        };
        match image_data_url(dir, dest, network) {
            Some(url) => out += &url,
            None => out += &rest[..len],
        }
        rest = &rest[len..];
    }
    out + rest
}

/// An image as a data: URL: a local file, or with `network` an http(s) URL.
/// `None` (with a warning) for what cannot be read, and other URLs.
fn image_data_url(dir: &Path, dest: &str, network: bool) -> Option<String> {
    let remote = dest.starts_with("http://") || dest.starts_with("https://");
    if dest.is_empty() || (remote && !network) || (!remote && dest.contains(':')) {
        return None;
    }
    let read = if remote {
        download(dest).map_err(|e| e.to_string())
    } else {
        std::fs::read(dir.join(dest))
            .map(|data| (data, None))
            .map_err(|e| e.to_string())
    };
    let (data, content_type) = read
        .inspect_err(|e| eprintln!("rat: image {dest}: {e}"))
        .ok()?;
    let svg = dest.to_ascii_lowercase().ends_with(".svg")
        || content_type.as_deref() == Some("image/svg+xml");
    let mime = sniff(&data)
        .filter(|mime| mime.starts_with("image/"))
        .or(svg.then_some("image/svg+xml"))?;
    Some(format!("data:{mime};base64,{}", STANDARD.encode(&data)))
}

/// Downloads `url`, returning its body and content type.
fn download(url: &str) -> Result<(Vec<u8>, Option<String>), ureq::Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut response = agent.get(url).call()?;
    let content_type = response.body().mime_type().map(String::from);
    let data = response
        .body_mut()
        .with_config()
        .limit(MAX_PAYLOAD as u64)
        .read_to_vec()?;
    Ok((data, content_type))
}

/// Image or video MIME type from the file's magic bytes.
fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF8") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.len() >= 8 && &data[4..8] == b"ftyp" {
        Some("video/mp4")
    } else if data.starts_with(b"\x1a\x45\xdf\xa3") {
        // Matroska: webm and mkv alike, the player looks at the content.
        Some("video/webm")
    } else if data.starts_with(b"OggS") {
        Some("video/ogg")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_images_are_inlined_outside_code() {
        let dir = std::env::temp_dir().join(format!("rat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.png"), b"\x89PNG").unwrap();
        std::fs::write(dir.join("c.png"), b"\x89PNG").unwrap();
        let png = "data:image/png;base64,iVBORw==";
        let md = "x ![a](c.png \"title\") ![b](<a b.png>) ![c](https://e.org/i.png)\n\
                  `![d](c.png)`\n```\n![e](c.png)\n```\n![f](missing.png)\n";
        assert_eq!(
            inline_images(md, &dir, false),
            format!(
                "x ![a]({png} \"title\") ![b]({png}) ![c](https://e.org/i.png)\n\
                 `![d](c.png)`\n```\n![e](c.png)\n```\n![f](missing.png)\n"
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
