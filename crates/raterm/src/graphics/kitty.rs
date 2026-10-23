//! kitty's graphics protocol: `APC G <keys> ; <base64> ST`, in chunks.

use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, general_purpose};

use super::{Data, MAX_IMAGE, Pixels};

/// Chunks are padded or not, as programs send them.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The keys of a command, with kitty's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// `a`: transmit `t`, transmit and put `T`, put `p`, delete `d`, query `q`.
    pub action: u8,
    /// `t`: direct `d`, the only one raterm takes; file, temporary file and
    /// shared memory are refused.
    pub medium: u8,
    /// `f`: 24 (RGB), 32 (RGBA) or 100 (PNG).
    pub format: u32,
    /// `o`: `z` for zlib.
    pub compression: u8,
    /// `i`, `I` and `p`: ids of the image, its number, the placement.
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    /// `q`: 1 leaves out OK, 2 errors too.
    pub quiet: u32,
    /// `s` and `v`: the size of RGB and RGBA data.
    pub width: usize,
    pub height: usize,
    /// `m=1`: more chunks follow.
    pub more: bool,
    /// `x`, `y`, `w`, `h`: the part of the image shown, all of it by default.
    pub source: (usize, usize, usize, usize),
    /// `X`, `Y`: where it starts in the first cell, in pixels.
    pub offset: (usize, usize),
    /// `c`, `r`: the cells it is shown over, scaled; from its size if 0.
    pub cols: usize,
    pub rows: usize,
    /// `C=1`: the cursor stays where it is.
    pub stay: bool,
    /// `z`: negative under the text.
    pub z: i64,
    /// `d`: what to delete.
    pub delete: u8,
    /// `U=1`: a virtual placement, which unicode placeholders show.
    pub placeholders: bool,
}

impl Default for Command {
    fn default() -> Self {
        Command {
            action: b't',
            medium: b'd',
            format: 32,
            compression: 0,
            id: 0,
            number: 0,
            placement: 0,
            quiet: 0,
            width: 0,
            height: 0,
            more: false,
            source: (0, 0, 0, 0),
            offset: (0, 0),
            cols: 0,
            rows: 0,
            stay: false,
            z: 0,
            delete: b'a',
            placeholders: false,
        }
    }
}

impl Command {
    /// Parses the keys of a command (`a=T,f=100,i=1`); unknown ones are left
    /// out.
    pub fn parse(keys: &[u8]) -> Command {
        let mut cmd = Command::default();
        for pair in keys.split(|&b| b == b',') {
            let [key, b'=', value @ ..] = pair else {
                continue;
            };
            let letter = value.first().copied().unwrap_or(0);
            let n = std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let u = n.clamp(0, u32::MAX as i64) as u32;
            match key {
                b'a' => cmd.action = letter,
                b't' => cmd.medium = letter,
                b'o' => cmd.compression = letter,
                b'd' => cmd.delete = letter,
                b'f' => cmd.format = u,
                b'i' => cmd.id = u,
                b'I' => cmd.number = u,
                b'p' => cmd.placement = u,
                b'q' => cmd.quiet = u,
                b's' => cmd.width = u as usize,
                b'v' => cmd.height = u as usize,
                b'm' => cmd.more = u == 1,
                b'x' => cmd.source.0 = u as usize,
                b'y' => cmd.source.1 = u as usize,
                b'w' => cmd.source.2 = u as usize,
                b'h' => cmd.source.3 = u as usize,
                b'X' => cmd.offset.0 = u as usize,
                b'Y' => cmd.offset.1 = u as usize,
                b'c' => cmd.cols = u as usize,
                b'r' => cmd.rows = u as usize,
                b'C' => cmd.stay = u == 1,
                b'z' => cmd.z = n,
                b'U' => cmd.placeholders = u == 1,
                _ => {}
            }
        }
        cmd
    }

    /// The answer to the program, if it asked for one: commands without an
    /// id nor a number get none. Built from numbers and `error` only, a
    /// fixed text, never from what the program sent.
    pub fn reply(&self, error: Option<&str>) -> Option<String> {
        let quiet = match error {
            None => self.quiet >= 1,
            Some(_) => self.quiet >= 2,
        };
        if quiet || (self.id == 0 && self.number == 0) {
            return None;
        }
        let mut keys = format!("i={}", self.id);
        if self.number != 0 {
            keys += &format!(",I={}", self.number);
        }
        if self.placement != 0 {
            keys += &format!(",p={}", self.placement);
        }
        Some(format!("\x1b_G{keys};{}\x1b\\", error.unwrap_or("OK")))
    }
}

/// Joins the chunks of a transmission.
#[derive(Default)]
pub struct Chunks {
    pending: Option<(Command, Vec<u8>)>,
}

impl Chunks {
    /// Takes a command, `keys;payload`: the whole one and its payload (still
    /// in base64) once its last chunk came.
    pub fn feed(&mut self, data: &[u8]) -> Option<(Command, Vec<u8>)> {
        let (keys, payload) = match data.iter().position(|&b| b == b';') {
            Some(at) => (&data[..at], &data[at + 1..]),
            None => (data, &[][..]),
        };
        let chunk = Command::parse(keys);
        let (cmd, mut all) = match self.pending.take() {
            // The next chunks carry only `m`, and maybe `q`.
            Some((cmd, all)) => (cmd, all),
            None => (chunk.clone(), Vec::new()),
        };
        all.extend_from_slice(payload);
        if all.len() > MAX_IMAGE / 3 * 4 + 4 {
            eprintln!("raterm: kitty image over {MAX_IMAGE} bytes, ignored");
            return None;
        }
        if chunk.more {
            self.pending = Some((cmd, all));
            return None;
        }
        Some((cmd, all))
    }
}

/// The data of a transmission, or the error to answer.
pub fn decode(cmd: &Command, payload: &[u8]) -> Result<Data, &'static str> {
    if cmd.medium != b'd' {
        return Err("EINVAL:only direct transmission is supported");
    }
    let bytes = BASE64
        .decode(payload)
        .or_else(|_| general_purpose::STANDARD.decode(payload.trim_ascii()))
        .map_err(|_| "EINVAL:invalid base64 data")?;
    let bytes = match cmd.compression {
        0 => bytes,
        b'z' => inflate(&bytes)?,
        _ => return Err("EINVAL:unknown compression"),
    };
    let (width, height) = match cmd.format {
        100 => png_size(&bytes).ok_or("EINVAL:invalid PNG data")?,
        24 | 32 => (cmd.width, cmd.height),
        _ => return Err("EINVAL:unknown format"),
    };
    if width == 0 || height == 0 {
        return Err("EINVAL:zero width or height");
    }
    if width.saturating_mul(height).saturating_mul(4) > MAX_IMAGE {
        return Err("EFBIG:image too large");
    }
    let pixels = match cmd.format {
        100 => Pixels::Png(bytes),
        format => {
            let depth = (format / 8) as usize;
            if bytes.len() < width * height * depth {
                return Err("ENODATA:insufficient image data");
            }
            Pixels::Argb(argb(&bytes[..width * height * depth], depth))
        }
    };
    Ok(Data {
        width,
        height,
        pixels,
    })
}

/// Inflates zlib data, up to `MAX_IMAGE` bytes.
fn inflate(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .take(MAX_IMAGE as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| "EINVAL:invalid zlib data")?;
    if out.len() > MAX_IMAGE {
        return Err("EFBIG:image too large");
    }
    Ok(out)
}

/// RGB or RGBA bytes as premultiplied ARGB.
fn argb(bytes: &[u8], depth: usize) -> Vec<u32> {
    bytes
        .chunks(depth)
        .map(|p| {
            let a = if depth == 4 { p[3] as u32 } else { 255 };
            let c = |v: u8| (v as u32 * a + 127) / 255;
            a << 24 | c(p[0]) << 16 | c(p[1]) << 8 | c(p[2])
        })
        .collect()
}

/// The size of a PNG, from its `IHDR` chunk.
fn png_size(bytes: &[u8]) -> Option<(usize, usize)> {
    let header = bytes.strip_prefix(PNG)?;
    if header.get(4..8)? != b"IHDR" {
        return None;
    }
    let be = |at: usize| Some(u32::from_be_bytes(header.get(at..at + 4)?.try_into().ok()?));
    Some((be(8)? as usize, be(12)? as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_with_defaults() {
        let cmd = Command::parse(b"a=T,f=24,s=2,v=3,i=7,c=4,z=-5,C=1,U=1,bogus,k=9");
        assert_eq!((cmd.action, cmd.medium, cmd.format), (b'T', b'd', 24));
        assert_eq!((cmd.width, cmd.height, cmd.id, cmd.cols), (2, 3, 7, 4));
        assert_eq!((cmd.z, cmd.stay, cmd.placeholders), (-5, true, true));
        assert_eq!(Command::parse(b"").delete, b'a');
    }

    #[test]
    fn joins_chunks() {
        let mut chunks = Chunks::default();
        assert_eq!(chunks.feed(b"a=T,f=24,s=1,v=1,i=3,m=1;/w"), None);
        assert_eq!(chunks.feed(b"m=1;AA"), None);
        let (cmd, payload) = chunks.feed(b"m=0;").unwrap();
        assert_eq!((cmd.action, cmd.id), (b'T', 3));
        let data = decode(&cmd, &payload).unwrap();
        assert!(matches!(data.pixels, Pixels::Argb(ref p) if p == &[0xffff_0000]));
    }

    #[test]
    fn decodes_rgba_premultiplied_and_png_sizes() {
        let cmd = Command::parse(b"f=32,s=1,v=1");
        let data = decode(&cmd, b"/wAAgA==").unwrap();
        assert!(matches!(data.pixels, Pixels::Argb(ref p) if p == &[0x8080_0000]));
        // A PNG of 1 × 1 pixel.
        let png = b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let data = decode(&Command::parse(b"f=100"), png).unwrap();
        assert_eq!((data.width, data.height), (1, 1));
    }

    #[test]
    fn inflates_zlib_up_to_the_limit() {
        use std::io::Write;
        let zlib = |bytes: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            z.write_all(bytes).unwrap();
            general_purpose::STANDARD.encode(z.finish().unwrap())
        };
        let data = decode(
            &Command::parse(b"o=z,f=24,s=1,v=1"),
            zlib(&[255, 0, 0]).as_bytes(),
        );
        assert!(matches!(data.unwrap().pixels, Pixels::Argb(ref p) if p == &[0xffff_0000]));
        // A bomb: 64 MiB of zeros and a byte more, in a few KiB.
        let bomb = zlib(&vec![0; MAX_IMAGE + 1]);
        assert!(bomb.len() < 1 << 20);
        let err = decode(&Command::parse(b"o=z,f=32,s=1,v=1"), bomb.as_bytes()).err();
        assert_eq!(err, Some("EFBIG:image too large"));
    }

    #[test]
    fn refuses_what_it_cannot_take() {
        let err = |keys: &[u8], payload: &[u8]| decode(&Command::parse(keys), payload).err();
        assert_eq!(
            err(b"t=f,f=24,s=1,v=1", b"L3RtcC94"),
            Some("EINVAL:only direct transmission is supported")
        );
        assert_eq!(
            err(b"o=z,f=24,s=1,v=1", b"AAAA"),
            Some("EINVAL:invalid zlib data")
        );
        assert_eq!(
            err(b"o=x,f=24,s=1,v=1", b"AAAA"),
            Some("EINVAL:unknown compression")
        );
        assert_eq!(
            err(b"f=24,s=3,v=2", b"AAAA"),
            Some("ENODATA:insufficient image data")
        );
        assert_eq!(
            err(b"f=24,s=99999,v=99999", b"AAAA"),
            Some("EFBIG:image too large")
        );
        assert_eq!(err(b"f=100", b"AAAA"), Some("EINVAL:invalid PNG data"));
    }

    #[test]
    fn replies_only_when_asked() {
        let reply = |keys: &[u8], error| Command::parse(keys).reply(error);
        assert_eq!(reply(b"i=31", None).as_deref(), Some("\x1b_Gi=31;OK\x1b\\"));
        assert_eq!(reply(b"a=T", None), None);
        assert_eq!(reply(b"i=1,q=1", None), None);
        assert_eq!(
            reply(b"i=1,I=7,p=2,q=1", Some("ENOENT:no such image")).as_deref(),
            Some("\x1b_Gi=1,I=7,p=2;ENOENT:no such image\x1b\\")
        );
        assert_eq!(reply(b"i=1,q=2", Some("ENOENT:no such image")), None);
    }
}
