//! Images of the graphics protocols, shown through the cells they cover (see
//! `Spec-Graphics`): each cell is marked with its slice of an image.

mod diacritics;
pub mod kitty;
pub mod sixel;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use alacritty_terminal::term::cell::Hyperlink;
use serde::Serialize;

/// Decoded pixels kept in all; past it, the oldest images are dropped.
pub const QUOTA: usize = 320 << 20;
/// Decoded pixels, or data, of one image at most.
pub const MAX_IMAGE: usize = 64 << 20;
/// The scheme of the hyperlinks that mark cells: raterm never shows them.
const MARK: &str = "raterm-image:";

/// The pixels of an image, shared by its placements.
pub struct Data {
    pub width: usize,
    pub height: usize,
    pub pixels: Pixels,
}

pub enum Pixels {
    /// Premultiplied ARGB, a `u32` per pixel, as Cairo takes them.
    Argb(Vec<u32>),
    /// PNG, which the view decodes: the core only reads its size.
    Png(Vec<u8>),
}

impl Data {
    /// Its decoded size, in bytes.
    pub fn size(&self) -> usize {
        self.width * self.height * 4
    }
}

/// An image placed on the grid, shown by the cells it marked.
pub struct Image {
    pub data: Arc<Data>,
    /// The part of the data shown, in its pixels: x, y, width, height.
    pub source: (usize, usize, usize, usize),
    /// The size of a cell in the data's pixels, by which it is cut in
    /// slices: the terminal's when it was placed, or the source spread over
    /// the cells a program asked for.
    pub cell: (f64, f64),
    /// Where the source starts in the first cell, in the data's pixels.
    pub offset: (f64, f64),
    /// Drawn under the text (kitty's negative `z`).
    pub under: bool,
    /// Placed on the alternate screen, which drops it on exit.
    pub alt: bool,
    /// kitty's image and placement ids.
    pub kitty: Option<(u32, u32)>,
}

impl Image {
    /// All of `data`, a cell of the terminal's size at a time.
    pub fn whole(data: Arc<Data>, cell: (f64, f64), alt: bool) -> Image {
        Image {
            source: (0, 0, data.width, data.height),
            data,
            cell,
            offset: (0.0, 0.0),
            under: false,
            alt,
            kitty: None,
        }
    }
}

/// The part of an image a cell shows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Slice {
    pub image: u32,
    pub row: u16,
    pub col: u16,
}

impl Slice {
    pub fn mark(self) -> Hyperlink {
        let uri = format!("{MARK}{}/{}/{}", self.image, self.row, self.col);
        Hyperlink::new(Some(""), uri)
    }

    /// The slice a cell's hyperlink marks, if it is a mark.
    pub fn of(link: &Hyperlink) -> Option<Slice> {
        let mut parts = link.uri().strip_prefix(MARK)?.split('/');
        let mut next = || parts.next()?.parse().ok();
        Some(Slice {
            image: next()?,
            row: next()? as u16,
            col: next()? as u16,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Placed(u32),
    Stored(u32),
}

#[derive(Default)]
pub struct Images {
    /// Placed images by number, with the bytes they count in the quota.
    placed: HashMap<u32, (Arc<Image>, usize)>,
    /// The images kitty's protocol transmitted, by id.
    stored: HashMap<u32, Arc<Data>>,
    /// kitty's image numbers (`I`), to the ids raterm picked for them.
    numbers: HashMap<u32, u32>,
    /// kitty's virtual placements, by image and placement id, which unicode
    /// placeholders show.
    virtuals: HashMap<(u32, u32), u32>,
    /// Oldest first.
    order: VecDeque<Key>,
    bytes: usize,
    next: u32,
    /// Placed images were removed this many times: the view draws its rows
    /// again, as their cells may show them still.
    pub removed: u64,
}

impl Images {
    /// Keeps a placed image, counting `bytes` in the quota; its number.
    pub fn place(&mut self, image: Image, bytes: usize) -> u32 {
        self.charge(bytes);
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.placed.insert(id, (Arc::new(image), bytes));
        self.order.push_back(Key::Placed(id));
        id
    }

    /// Keeps a virtual placement of kitty's image `id`, replacing the one
    /// with the same `placement` id; its number.
    pub fn place_virtual(&mut self, image: Image, id: u32, placement: u32) -> u32 {
        if let Some(old) = self.virtuals.get(&(id, placement)).copied() {
            self.retain(|n, _| n != old);
        }
        let n = self.place(image, 0);
        self.virtuals.insert((id, placement), n);
        n
    }

    /// The virtual placement placeholders of image `id` show: placement
    /// `placement`, or with 0 any of them (the first one).
    pub fn virtual_of(&self, id: u32, placement: u32) -> Option<u32> {
        match placement {
            0 => (self.virtuals.iter())
                .filter(|((i, _), _)| *i == id)
                .min_by_key(|((_, p), _)| *p)
                .map(|(_, &n)| n),
            p => self.virtuals.get(&(id, p)).copied(),
        }
    }

    pub fn get(&self, id: u32) -> Option<&Arc<Image>> {
        self.placed.get(&id).map(|(image, _)| image)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &Arc<Image>)> {
        self.placed.iter().map(|(&id, (image, _))| (id, image))
    }

    pub fn retain(&mut self, keep: impl Fn(u32, &Image) -> bool) {
        let gone: Vec<u32> = (self.placed.iter())
            .filter(|(id, (image, _))| !keep(**id, image))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.remove(Key::Placed(id));
            self.order.retain(|&k| k != Key::Placed(id));
        }
    }

    /// Keeps the data kitty's protocol transmitted under `id`, replacing
    /// what it held.
    pub fn store(&mut self, id: u32, data: Arc<Data>) {
        self.unstore(id);
        self.charge(data.size());
        self.stored.insert(id, data);
        self.order.push_back(Key::Stored(id));
    }

    pub fn stored(&self, id: u32) -> Option<&Arc<Data>> {
        self.stored.get(&id)
    }

    pub fn unstore(&mut self, id: u32) {
        self.remove(Key::Stored(id));
        self.order.retain(|&k| k != Key::Stored(id));
    }

    /// The id of kitty's image `number`, a new one unless `picked`.
    pub fn number(&mut self, number: u32, picked: Option<u32>) -> u32 {
        let id = picked.unwrap_or_else(|| {
            // Out of the small ids programs pick themselves.
            (u32::MAX / 2..)
                .find(|id| !self.stored.contains_key(id))
                .unwrap()
        });
        self.numbers.insert(number, id);
        id
    }

    pub fn numbered(&self, number: u32) -> Option<u32> {
        self.numbers.get(&number).copied()
    }

    /// Drops the oldest images until `bytes` more fit in the quota.
    fn charge(&mut self, bytes: usize) {
        while self.bytes + bytes > QUOTA
            && let Some(old) = self.order.pop_front()
        {
            self.remove(old);
        }
        self.bytes += bytes;
    }

    fn remove(&mut self, key: Key) {
        match key {
            Key::Placed(id) => {
                if let Some((_, bytes)) = self.placed.remove(&id) {
                    self.bytes -= bytes;
                    self.removed += 1;
                    self.virtuals.retain(|_, &mut n| n != id);
                }
            }
            Key::Stored(id) => {
                if let Some(data) = self.stored.remove(&id) {
                    self.bytes -= data.size();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(pixels: usize) -> Arc<Data> {
        Arc::new(Data {
            width: pixels,
            height: 1,
            pixels: Pixels::Argb(vec![0; pixels]),
        })
    }

    #[test]
    fn marks_round_trip() {
        let slice = Slice {
            image: 7,
            row: 2,
            col: 30,
        };
        assert_eq!(Slice::of(&slice.mark()), Some(slice));
        assert_eq!(
            Slice::of(&Hyperlink::new(None::<&str>, "https://x".into())),
            None
        );
    }

    #[test]
    fn quota_drops_the_oldest() {
        let mut images = Images::default();
        let quarter = QUOTA / 4 / 4;
        let place = |images: &mut Images| {
            let image = Image::whole(data(quarter), (1.0, 1.0), false);
            images.place(image, quarter * 4)
        };
        let first = place(&mut images);
        images.store(9, data(quarter));
        let ids: Vec<u32> = (0..2).map(|_| place(&mut images)).collect();
        assert!(images.get(first).is_some());
        // The fifth quarter drops the oldest, placed or stored.
        place(&mut images);
        assert!(images.get(first).is_none());
        assert!(images.stored(9).is_some());
        images.retain(|id, _| id != ids[0]);
        assert!(images.get(ids[0]).is_none());
        assert_eq!(images.bytes, 3 * quarter * 4);
        assert_eq!(images.removed, 2);
    }
}
