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
    cached_key(&key(url, edge))
}

fn cached_key(key: &str) -> Option<Image> {
    CACHE.with(|c| c.borrow().images.get(key).cloned())
}

/// The cached artwork for `url`, or an empty image, which the UI shows as a
/// placeholder until it loads.
pub fn cached_or_empty(url: Option<&str>, edge: u32) -> Image {
    url.and_then(|url| cached(url, edge)).unwrap_or_default()
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
    let key = key(url, edge);
    if let Some(image) = cached_key(&key) {
        return Some(image);
    }
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

/// The artwork's strongest colour, darkened enough for white text, as
/// Spotify tints its page headers.
fn accent_of(image: &image::DynamicImage) -> Color {
    let small = image
        .resize_exact(16, 16, image::imageops::FilterType::Triangle)
        .to_rgb8();
    // Pixels are pooled by hue, so artwork of several colours gives its
    // strongest one rather than their average, which tends to grey.
    const HUES: usize = 12;
    let mut pools = [[0.0f32; 5]; HUES];
    for p in small.pixels() {
        let [pr, pg, pb] = p.0.map(|c| c as f32 / 255.0);
        let (h, s, l) = rgb_to_hsl(pr, pg, pb);
        // Saturated mid-tones count; greys, black and white hardly do.
        let chroma = s * (1.0 - (2.0 * l - 1.0).abs());
        // Cubed, so a vivid area outweighs a larger dull one: a sky over
        // beige buildings is blue, not beige.
        let weight = chroma.powi(3);
        // Offset half a pool, so red, which sits at 0° and 360°, and the
        // other primaries fall in the middle of one rather than across two.
        let pool = &mut pools[((h / 360.0 * HUES as f32) + 0.5) as usize % HUES];
        for (sum, c) in pool
            .iter_mut()
            .zip([pr * weight, pg * weight, pb * weight, weight, chroma])
        {
            *sum += c;
        }
    }
    let [r, g, b, total, colour] = pools
        .into_iter()
        .max_by(|a, b| a[3].total_cmp(&b[3]))
        .unwrap_or_default();
    let (r, g, b) = if total > 0.0 {
        (r / total, g / total, b / total)
    } else {
        (0.0, 0.0, 0.0)
    };
    let (h, s, _) = rgb_to_hsl(r, g, b);
    // Muted artwork with a hue all the same, such as a bluish photo, stays
    // clear of greyish artwork's floor, so `vivid` can tell them apart.
    // Chroma, unlike saturation, stays low for near-black with a cast.
    let chroma = r.max(g).max(b) - r.min(g).min(b);
    let s = if chroma >= 0.05 && colour >= COLOURFUL_WEIGHT {
        s.clamp(MIN_ACCENT_SATURATION + 0.1, 0.7)
    } else {
        MIN_ACCENT_SATURATION
    };
    // Bright hues such as yellow are darkened further to keep white text
    // at 4.5:1.
    readable(from_hsl(h, s, 0.32), true)
}

/// The least saturation `accent_of` gives, which greyish artwork gets.
const MIN_ACCENT_SATURATION: f32 = 0.25;
/// How much colour, summed over `accent_of`'s 256 pixels, makes a hue
/// worth following: a few strong pixels, or a faint wash over a tenth.
const COLOURFUL_WEIGHT: f32 = 2.0;

/// A saturated take on an `accent`, for controls drawn in it rather than
/// backgrounds behind white text, or `None` for greyish artwork, whose hue
/// means nothing.
pub fn vivid(accent: Color) -> Option<Color> {
    let (r, g, b) = channels(accent);
    let (h, s, _) = rgb_to_hsl(r, g, b);
    // A little above the floor, as the colour went through 8-bit channels.
    (s > MIN_ACCENT_SATURATION + 0.05).then(|| from_hsl(h, s.max(0.5), 0.5))
}

/// `colour` made just light or dark enough to read: as text on the light
/// look's white, or, in the dark looks, as text on black and behind white
/// icons. WCAG asks 4.5:1 of text and 3:1 of icons.
pub fn readable(colour: Color, light: bool) -> Color {
    let (lowest, highest) = if light { (0.0, 0.18) } else { (0.175, 0.3) };
    let (r, g, b) = channels(colour);
    let lum = luminance(r, g, b);
    if (lowest..=highest).contains(&lum) {
        return colour;
    }
    // A little inside the range, which rounding to 8-bit channels could
    // otherwise step out of.
    let target = if lum > highest {
        highest - 0.01
    } else {
        lowest + 0.01
    };
    // Luminance rises with lightness, so halve the range towards the target.
    let (h, s, _) = rgb_to_hsl(r, g, b);
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = f32::midpoint(low, high);
        let (r, g, b) = hsl_to_rgb(h, s, mid);
        if luminance(r, g, b) < target {
            low = mid;
        } else {
            high = mid;
        }
    }
    from_hsl(h, s, f32::midpoint(low, high))
}

/// WCAG's relative luminance.
fn luminance(r: f32, g: f32, b: f32) -> f32 {
    let linear = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn channels(colour: Color) -> (f32, f32, f32) {
    let [r, g, b] = [colour.red(), colour.green(), colour.blue()].map(|c| f32::from(c) / 255.0);
    (r, g, b)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let (r, g, b) = hsl_to_rgb(h, s, l);
    let byte = |c: f32| (c * 255.0).round() as u8;
    Color::from_rgb_u8(byte(r), byte(g), byte(b))
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

        let bright = vivid(accent).expect("red artwork has a hue");
        assert!(bright.red() > accent.red(), "brighter");
        assert!(
            bright.red() > bright.green() && bright.red() > bright.blue(),
            "same hue"
        );

        let grey = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([128, 128, 128]),
        ));
        assert_eq!(vivid(accent_of(&grey)), None, "no hue to follow");
    }

    #[test]
    fn follows_muted_and_mixed_artwork() {
        let muted = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([90, 100, 130]),
        ));
        let blue = vivid(accent_of(&muted)).expect("a bluish photo has a hue");
        assert!(blue.blue() > blue.red() && blue.blue() > blue.green());

        // Red and cyan halves, which average to grey.
        let halves = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                image::Rgb([230, 20, 20])
            } else {
                image::Rgb([20, 230, 230])
            }
        }));
        let (r, g, b) = channels(vivid(accent_of(&halves)).expect("one of the two colours"));
        let (h, _, _) = rgb_to_hsl(r, g, b);
        assert!(
            !(15.0..165.0).contains(&h) && !(195.0..345.0).contains(&h),
            "red or cyan"
        );

        // A quarter of sky over beige buildings, the dull area the larger.
        let sky = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |_, y| {
            if y < 2 {
                image::Rgb([90, 150, 220])
            } else {
                image::Rgb([200, 170, 130])
            }
        }));
        let blue = vivid(accent_of(&sky)).expect("the sky's colour");
        assert!(blue.blue() > blue.red(), "the sky, not the beige");

        let near_black = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([40, 36, 32]),
        ));
        assert_eq!(vivid(accent_of(&near_black)), None, "a cast isn't a colour");

        let yellow = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([240, 220, 20]),
        ));
        let (r, g, b) = channels(accent_of(&yellow));
        assert!(luminance(r, g, b) <= 0.18, "white text still reads on it");
    }

    #[test]
    fn makes_accents_readable_in_each_look() {
        let lum = |c: Color| {
            let (r, g, b) = channels(c);
            luminance(r, g, b)
        };
        let green = Color::from_rgb_u8(0x1d, 0xb9, 0x54);
        assert!(lum(readable(green, true)) <= 0.18, "darkened on white");
        let red = Color::from_rgb_u8(0xff, 0x00, 0x33);
        assert_eq!(readable(red, false), red, "already fine on black");
        let blue = Color::from_rgb_u8(0, 0, 0xff);
        assert!(lum(readable(blue, false)) >= 0.175, "lightened on black");
        let yellow = Color::from_rgb_u8(0xee, 0xee, 0x22);
        let fixed = readable(yellow, false);
        assert!(lum(fixed) <= 0.3, "dark enough behind white icons");
        assert!(fixed.red() > fixed.blue(), "keeps its hue");
    }
}
