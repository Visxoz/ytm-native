//! Loads artwork off the UI thread and caches decoded images on it.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::{Arc, OnceLock},
    time::Duration,
};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::{runtime::Handle, sync::Semaphore};

/// Decoded artwork kept for reuse, in bytes. Images still on screen stay
/// alive regardless, as Slint holds its own reference.
const CACHE_BUDGET: usize = 32 * 1024 * 1024;
const PARALLEL_FETCHES: usize = 8;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

#[derive(Default)]
struct Cache {
    images: HashMap<String, Image>,
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
        }
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("HTTP client builds")
    })
}

fn permits() -> &'static Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(Semaphore::new(PARALLEL_FETCHES)))
}

/// Returns a cached image without fetching.
pub fn cached(url: &str, edge: u32) -> Option<Image> {
    CACHE.with(|c| c.borrow().images.get(&key(url, edge)).cloned())
}

fn key(url: &str, edge: u32) -> String {
    format!("{edge}:{url}")
}

/// Fetches `url` and scales it to fit `edge` pixels. Call on the UI thread.
pub async fn load(rt: &Handle, url: &str, edge: u32) -> Option<Image> {
    if let Some(image) = cached(url, edge) {
        return Some(image);
    }
    let owned = url.to_owned();
    let buffer = rt
        .spawn(async move {
            let _permit = permits().acquire().await.ok()?;
            let bytes = http()
                .get(&owned)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .ok()?
                .bytes()
                .await
                .ok()?;
            tokio::task::spawn_blocking(move || decode(&bytes, edge))
                .await
                .ok()?
        })
        .await
        .ok()??;
    let image = Image::from_rgba8(buffer);
    CACHE.with(|c| c.borrow_mut().insert(key(url, edge), image.clone()));
    Some(image)
}

fn decode(bytes: &[u8], edge: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
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
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}
