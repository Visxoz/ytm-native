//! The HTTP client shared by the app's own requests, so requests to the same
//! host reuse one connection.

use std::{sync::OnceLock, time::Duration};

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("HTTP client builds")
    })
}
