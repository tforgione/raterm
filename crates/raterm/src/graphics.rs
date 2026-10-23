//! Images of the graphics protocols, shown through the cells they cover (see
//! `Spec-Graphics`): each cell is marked with its slice of an image.

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

pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Premultiplied ARGB, a `u32` per pixel, as Cairo takes them.
    pub pixels: Vec<u32>,
    /// The size of a cell when it was placed, by which it is cut in slices.
    pub cell: (f64, f64),
    /// Placed on the alternate screen, which drops it on exit.
    pub alt: bool,
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

#[derive(Default)]
pub struct Images {
    images: HashMap<u32, Arc<Image>>,
    /// Oldest first.
    order: VecDeque<u32>,
    bytes: usize,
    next: u32,
}

impl Images {
    /// Keeps `image`, dropping the oldest ones past the quota; its number.
    pub fn add(&mut self, image: Image) -> u32 {
        let size = image.pixels.len() * 4;
        while self.bytes + size > QUOTA
            && let Some(old) = self.order.pop_front()
        {
            self.remove(old);
        }
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.bytes += size;
        self.images.insert(id, Arc::new(image));
        self.order.push_back(id);
        id
    }

    pub fn get(&self, id: u32) -> Option<&Arc<Image>> {
        self.images.get(&id)
    }

    pub fn retain(&mut self, keep: impl Fn(u32, &Image) -> bool) {
        let gone: Vec<u32> = (self.images.iter())
            .filter(|(id, image)| !keep(**id, image))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.remove(id);
            self.order.retain(|&i| i != id);
        }
    }

    fn remove(&mut self, id: u32) {
        if let Some(image) = self.images.remove(&id) {
            self.bytes -= image.pixels.len() * 4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: usize) -> Image {
        Image {
            width: pixels,
            height: 1,
            pixels: vec![0; pixels],
            cell: (1.0, 1.0),
            alt: false,
        }
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
        let ids: Vec<u32> = (0..5).map(|_| images.add(image(quarter))).collect();
        assert!(images.get(ids[0]).is_none());
        assert!(ids[1..].iter().all(|&id| images.get(id).is_some()));
        images.retain(|id, _| id != ids[2]);
        assert!(images.get(ids[2]).is_none());
        assert_eq!(images.bytes, 3 * quarter * 4);
    }
}
