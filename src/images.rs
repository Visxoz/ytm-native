//! Loads artwork off the UI thread and caches decoded images on it.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    sync::{Arc, OnceLock},
    time::Duration,
};

use slint::{Color, Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, oneshot},
};

// Logical sizes artwork is drawn at.
pub const ROW: u32 = 48;
pub const PLAYER: u32 = 64;
pub const TOP: u32 = 96;
pub const CARD: u32 = 176;
pub const HEADER: u32 = 232;
pub const BANNER: u32 = 360;
pub const AVATAR: u32 = 26;

/// Decoded artwork kept for reuse, in bytes. Images still on screen stay
/// alive regardless, as Slint holds its own reference.
const CACHE_BUDGET: usize = 32 * 1024 * 1024;
/// Thumbnails are a few KB each and share HTTP/2 connections, so a page's
/// worth is fetched widely in parallel.
const PARALLEL_FETCHES: usize = 16;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    /// Images being fetched, with whoever else is waiting for each.
    static PENDING: RefCell<HashMap<String, Vec<oneshot::Sender<Option<Image>>>>> =
        RefCell::new(HashMap::new());
    /// Window scale factor, so artwork decodes at the size it is drawn.
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

pub fn set_scale(scale: f32) {
    SCALE.with(|s| s.set(scale.clamp(1.0, 4.0)));
}

#[derive(Default)]
struct Cache {
    images: HashMap<String, Image>,
    accents: HashMap<String, Color>,
    order: VecDeque<String>,
    bytes: usize,
}

fn byte_size(image: &Image) -> usize {
    let size = image.size();
    size.width as usize * size.height as usize * 4
}

impl Cache {
    fn insert(&mut self, key: String, image: Image) {
        self.bytes += byte_size(&image);
        if let Some(old) = self.images.insert(key.clone(), image) {
            self.bytes -= byte_size(&old);
        } else {
            self.order.push_back(key);
        }
        while self.bytes > CACHE_BUDGET {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.images.remove(&oldest) {
                self.bytes -= byte_size(&old);
            }
            self.accents.remove(&oldest);
        }
    }
}

fn permits() -> &'static Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(Semaphore::new(PARALLEL_FETCHES)))
}

/// Returns a cached image without fetching. `edge` is in logical pixels.
pub fn cached(url: &str, edge: u32) -> Option<Image> {
    CACHE.with(|c| c.borrow().images.get(&key(url, edge)).cloned())
}

/// A colour from the artwork for backgrounds behind it, once loaded.
pub fn accent(url: &str, edge: u32) -> Option<Color> {
    CACHE.with(|c| c.borrow().accents.get(&key(url, edge)).copied())
}

fn key(url: &str, edge: u32) -> String {
    format!("{}:{url}", physical(edge))
}

fn physical(edge: u32) -> u32 {
    (edge as f32 * SCALE.with(Cell::get)).round() as u32
}

/// Fetches `url` and scales it to `edge` logical pixels. Call on the UI thread.
/// Requests for an image already on its way wait for that one.
pub async fn load(rt: &Handle, url: &str, edge: u32) -> Option<Image> {
    if let Some(image) = cached(url, edge) {
        return Some(image);
    }
    let key = key(url, edge);
    let waiting = PENDING.with(|p| {
        let mut pending = p.borrow_mut();
        match pending.get_mut(&key) {
            Some(waiters) => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                Some(rx)
            }
            None => {
                pending.insert(key.clone(), Vec::new());
                None
            }
        }
    });
    if let Some(rx) = waiting {
        return rx.await.ok().flatten();
    }

    // Only artwork with something drawn behind it needs a colour picked.
    let with_accent = matches!(edge, PLAYER | HEADER | BANNER);
    let image = fetch(rt, url.to_owned(), edge, with_accent)
        .await
        .map(|(buffer, accent)| {
            let image = Image::from_rgba8(buffer);
            CACHE.with(|c| {
                let mut cache = c.borrow_mut();
                if let Some(accent) = accent {
                    cache.accents.insert(key.clone(), accent);
                }
                cache.insert(key.clone(), image.clone());
            });
            image
        });
    let waiters = PENDING.with(|p| p.borrow_mut().remove(&key));
    for tx in waiters.into_iter().flatten() {
        let _ = tx.send(image.clone());
    }
    image
}

/// Fetches `url` and decodes it at `edge` logical pixels.
async fn fetch(
    rt: &Handle,
    url: String,
    edge: u32,
    with_accent: bool,
) -> Option<(SharedPixelBuffer<Rgba8Pixel>, Option<Color>)> {
    if !crate::http::allowed(&url) {
        log::debug!("skipped artwork from a host YouTube does not use");
        return None;
    }
    let pixels = physical(edge);
    // googleusercontent renders any size, so ask for the pixels drawn rather
    // than download a larger rendition and scale it down. Banners keep the
    // wide rendition they came with, as a square size would crop them.
    let url = if edge == BANNER {
        url
    } else {
        crate::api::resize_thumb(&url, pixels)
    };
    rt.spawn(async move {
        let _permit = permits().acquire().await.ok()?;
        let bytes = crate::http::client()
            .get(&url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .ok()?
            .bytes()
            .await
            .ok()?;
        tokio::task::spawn_blocking(move || decode(&bytes, pixels, with_accent))
            .await
            .ok()?
    })
    .await
    .ok()?
}

fn decode(
    bytes: &[u8],
    edge: u32,
    with_accent: bool,
) -> Option<(SharedPixelBuffer<Rgba8Pixel>, Option<Color>)> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| log::debug!("undecodable artwork: {e}"))
        .ok()?;
    // Scale so the shorter side is `edge`; the UI crops to fill.
    let (w, h) = (image.width(), image.height());
    let scale = edge as f32 / w.min(h).max(1) as f32;
    let image = if scale < 1.0 {
        image.resize(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        image
    };
    let accent = with_accent.then(|| accent_of(&image));
    // Consumes the image, so one already in RGBA is not copied first.
    let rgba = image.into_rgba8();
    let buffer = SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    Some((buffer, accent))
}

/// The artwork's most colourful average, darkened enough for white text,
/// as Spotify tints its page headers.
fn accent_of(image: &image::DynamicImage) -> Color {
    let small = image
        .resize_exact(16, 16, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let (mut r, mut g, mut b, mut total) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for p in small.pixels() {
        let [pr, pg, pb] = p.0.map(|c| c as f32 / 255.0);
        let (_, s, l) = rgb_to_hsl(pr, pg, pb);
        // Favour saturated mid-tones over greys, black and white.
        let weight = 0.05 + s * (1.0 - (2.0 * l - 1.0).abs());
        r += pr * weight;
        g += pg * weight;
        b += pb * weight;
        total += weight;
    }
    let (h, s, _) = rgb_to_hsl(r / total, g / total, b / total);
    let (r, g, b) = hsl_to_rgb(h, s.clamp(0.25, 0.7), 0.32);
    Color::from_rgb_u8((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = f32::midpoint(max, min);
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_hsl() {
        for (r, g, b) in [
            (1.0, 0.0, 0.0),
            (0.2, 0.6, 0.4),
            (0.5, 0.5, 0.5),
            (0.1, 0.2, 0.9),
        ] {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let (r2, g2, b2) = hsl_to_rgb(h, s, l);
            assert!((r - r2).abs() < 1e-4 && (g - g2).abs() < 1e-4 && (b - b2).abs() < 1e-4);
        }
    }

    #[test]
    fn accent_keeps_the_hue_and_darkens() {
        let red = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([230, 20, 20]),
        ));
        let accent = accent_of(&red);
        assert!(accent.red() > accent.green() && accent.red() > accent.blue());
        assert!(accent.red() < 180, "dark enough for white text");
    }
}
