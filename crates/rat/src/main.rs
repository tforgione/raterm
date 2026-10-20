//! Shows a file as a rich block in raterm, or as plain text anywhere else.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
