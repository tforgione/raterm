//! Local files of video and audio blocks, streamed to the view over HTTP.
//!
//! WebKit's media player only reads http(s), data: and blob: URLs, neither
//! raterm:// nor file://. Files are served on 127.0.0.1, under a random token,
//! and only those announced by a block, by index: no other process can
//! use the server, and no block can make it read an arbitrary path.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

/// Files announced by video and audio blocks.
#[derive(Clone, Default)]
pub struct Media(Arc<Mutex<Vec<(PathBuf, String)>>>);

impl Media {
    /// Registers a file and its MIME type, returning its URL relative to the
    /// server's base URL, prefixed with `media/`.
    pub fn add(&self, path: PathBuf, mime: String) -> String {
        let mut files = self.0.lock().unwrap();
        files.push((path, mime));
        format!("media/{}", files.len() - 1)
    }

    pub fn get(&self, id: usize) -> Option<(PathBuf, String)> {
        self.0.lock().unwrap().get(id).cloned()
    }
}

pub struct Server {
    /// `http://127.0.0.1:port`, for the content security policy.
    pub origin: String,
    /// URL that file `n` is served under when followed by `n`.
    pub base: String,
}

/// Starts serving `media` in the background.
pub fn serve(media: Media) -> io::Result<Server> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut token = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut token)?;
    let token: String = token.iter().map(|b| format!("{b:02x}")).collect();
    let origin = format!("http://{}", listener.local_addr()?);
    let prefix = format!("/{token}/");
    let base = format!("{origin}{prefix}");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (media, prefix) = (media.clone(), prefix.clone());
            // Errors are the player closing the connection to seek elsewhere.
            thread::spawn(move || respond(stream, &media, &prefix));
        }
    });
    Ok(Server { origin, base })
}

fn respond(mut stream: TcpStream, media: &Media, prefix: &str) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("range")
        {
            range = parse_range(value.trim());
        }
    }
    let mut words = request.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let found = target
        .strip_prefix(prefix)
        .and_then(|id| id.parse().ok())
        .and_then(|id| media.get(id))
        .filter(|_| method == "GET" || method == "HEAD");
    let Some((path, mime)) = found else {
        return stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    };
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let (status, start, end) = match range {
        Some((start, _)) if start >= total => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            return stream.write_all(head.as_bytes());
        }
        Some((start, end)) => (
            "206 Partial Content",
            start,
            end.unwrap_or(total - 1).min(total - 1),
        ),
        None => ("200 OK", 0, total.saturating_sub(1)),
    };
    let len = if total == 0 { 0 } else { end - start + 1 };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\n\
         Accept-Ranges: bytes\r\nConnection: close\r\n"
    );
    if range.is_some() {
        head += &format!("Content-Range: bytes {start}-{end}/{total}\r\n");
    }
    head += "\r\n";
    stream.write_all(head.as_bytes())?;
    if method == "GET" {
        file.seek(SeekFrom::Start(start))?;
        io::copy(&mut file.take(len), &mut stream)?;
    }
    Ok(())
}

/// `bytes=start-[end]`; suffix ranges (`bytes=-n`) are not used by players.
fn parse_range(header: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = header.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = (start.parse().ok()?, end.parse().ok());
    end.is_none_or(|end| end >= start).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str, range: Option<&str>) -> String {
        let (host, path) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap();
        let mut stream = TcpStream::connect(host).unwrap();
        let range = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
        write!(
            stream,
            "GET /{path} HTTP/1.1\r\nHost: {host}\r\n{range}\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn serves_registered_files_by_range() {
        let path = std::env::temp_dir().join(format!("raterm-media-{}", std::process::id()));
        std::fs::write(&path, "0123456789").unwrap();
        let media = Media::default();
        let server = serve(media.clone()).unwrap();
        let url = format!(
            "{}{}",
            server.base,
            &media.add(path.clone(), "video/mp4".into())["media/".len()..]
        );

        let part = get(&url, Some("bytes=2-4"));
        assert!(part.starts_with("HTTP/1.1 206"), "{part}");
        assert!(part.contains("Content-Range: bytes 2-4/10\r\n"));
        assert!(part.ends_with("\r\n\r\n234"));
        assert!(get(&url, Some("bytes=7-")).ends_with("789"));
        assert!(get(&url, None).ends_with("0123456789"));
        assert!(get(&url, Some("bytes=10-")).starts_with("HTTP/1.1 416"));

        // Unknown index or wrong token.
        assert!(get(&format!("{}1", server.base), None).starts_with("HTTP/1.1 404"));
        let forged = format!("{}/0", server.origin);
        assert!(get(&forged, None).starts_with("HTTP/1.1 404"));
        std::fs::remove_file(path).unwrap();
    }
}
