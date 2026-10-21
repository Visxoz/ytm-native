//! Keeps sign-in data encrypted at rest with Windows' DPAPI, so other
//! Windows users and copies of the files on other machines cannot read it.

use std::{
    io,
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

/// Mixed into the key, so other programs calling DPAPI for this user
/// without it cannot decrypt the files.
const ENTROPY: &[u8] = b"ytm-native sign-in";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    }
}

fn crypt(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: the input blobs point at live slices, which DPAPI only reads,
    // and `output` receives a buffer DPAPI allocates.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }
    // SAFETY: DPAPI returned `cbData` bytes at `pbData`, which are copied
    // out and then freed once with `LocalFree`, as it requires.
    unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        Some(bytes)
    }
}

/// Reads and decrypts a file `write` made.
pub fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(crypt(&bytes, false)?).ok()
}

/// Encrypts `text` into `path`. The old file stays whole should this fail.
pub fn write(path: &Path, text: &str) -> io::Result<()> {
    let data = crypt(text.as_bytes(), true)
        .ok_or_else(|| io::Error::other("Windows could not encrypt the data"))?;
    let partial = path.with_extension("tmp");
    std::fs::write(&partial, data)?;
    std::fs::rename(partial, path)
}

/// Reads `path`, or failing that the plain-text `legacy` file an older
/// version wrote, which is then encrypted and removed.
pub fn read_migrating(path: &Path, legacy: &Path) -> Option<String> {
    if let Some(text) = read(path) {
        return Some(text);
    }
    let text = std::fs::read_to_string(legacy).ok()?;
    match write(path, &text) {
        Ok(()) => {
            let _ = std::fs::remove_file(legacy);
        }
        Err(e) => log::warn!("could not encrypt {}: {e}", legacy.display()),
    }
    Some(text)
}

/// rustypipe's cache, which holds the sign-in cookie.
pub struct EncryptedStorage {
    path: PathBuf,
    legacy: PathBuf,
}

impl EncryptedStorage {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("rustypipe_cache.bin"),
            legacy: dir.join("rustypipe_cache.json"),
        }
    }
}

impl rustypipe::cache::CacheStorage for EncryptedStorage {
    fn write(&self, data: &str) {
        if let Err(e) = write(&self.path, data) {
            log::warn!("could not save rustypipe's cache: {e}");
        }
    }

    fn read(&self) -> Option<String> {
        read_migrating(&self.path, &self.legacy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_migrates() {
        let dir = std::env::temp_dir().join(format!("ytm-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, legacy) = (dir.join("s.bin"), dir.join("s.txt"));

        std::fs::write(&legacy, "SID=a; HSID=b").unwrap();
        assert_eq!(
            read_migrating(&path, &legacy).as_deref(),
            Some("SID=a; HSID=b")
        );
        assert!(!legacy.exists(), "the plain-text file is removed");
        let stored = std::fs::read(&path).unwrap();
        assert!(
            !stored.windows(5).any(|w| w == b"SID=a"),
            "stored encrypted"
        );
        assert_eq!(read(&path).as_deref(), Some("SID=a; HSID=b"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
