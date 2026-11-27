//! Album art: fetched once and kept on disk for Now Playing cover art and
//! playlist cover verification.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};

use crate::http::Http;

const MAX_ART_BYTES: usize = 8 * 1024 * 1024;
/// The disk cache is pruned back to `ART_CACHE_KEEP` once it holds more
/// than this, oldest use first.
const ART_CACHE_LIMIT: u64 = 256 * 1024 * 1024;
const ART_CACHE_KEEP: u64 = 192 * 1024 * 1024;

fn is_http(uri: &str) -> bool {
    uri.starts_with("https://") || uri.starts_with("http://")
}

enum Entry {
    Pending,
    Ready { bytes: Option<Arc<[u8]>> },
    Failed,
}

struct Inner {
    entries: Mutex<HashMap<String, Entry>>,
    http: Http,
    runtime: tokio::runtime::Handle,
    cache_dir: PathBuf,
}

#[derive(Clone)]
pub struct ArtLoader {
    inner: Arc<Inner>,
}

impl ArtLoader {
    pub fn new(http: impl Into<Http>, runtime: tokio::runtime::Handle, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        let dir = cache_dir.clone();
        runtime.spawn_blocking(
            move || match prune_cache(&dir, ART_CACHE_LIMIT, ART_CACHE_KEEP) {
                Ok(0) => {}
                Ok(freed) => log::info!("artwork cache pruned by {freed} bytes"),
                Err(error) => log::warn!("unable to prune the artwork cache: {error}"),
            },
        );
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                http: http.into(),
                runtime,
                cache_dir,
            }),
        }
    }

    /// Bytes for `url`, from memory, disk, or the network.
    pub async fn fetch(&self, url: &str) -> Result<Arc<[u8]>, String> {
        self.inner.fetch(url).await
    }

    /// Whether artwork has finished loading from disk or the network.
    pub fn is_ready(&self, url: &str) -> bool {
        matches!(
            self.inner
                .entries
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(url),
            Some(Entry::Ready { .. })
        )
    }

    /// The disk-cache file holding `url`'s artwork, once it has been fetched.
    pub fn cached_file(&self, url: &str) -> Option<PathBuf> {
        let path = self.inner.cache_path(url);
        std::fs::metadata(&path)
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
            .then_some(path)
    }

    /// Starts downloading `url` to the disk cache and wakes `waker` once the
    /// file is written, for macOS Now Playing cover art.
    pub fn prefetch_with_waker(&self, waker: &crate::backend::Waker, url: &str) -> bool {
        if !is_http(url) {
            return false;
        }
        let mut entries = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        if entries.contains_key(url) {
            return false;
        }
        entries.insert(url.to_string(), Entry::Pending);
        drop(entries);
        self.inner.start_headless(waker.clone(), url.to_string());
        true
    }
}

/// Deletes the least recently used files in `dir` once they add up to more
/// than `limit` bytes, until they fit in `keep`. Returns the bytes freed.
fn prune_cache(dir: &std::path::Path, limit: u64, keep: u64) -> std::io::Result<u64> {
    let mut files = Vec::new();
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            total += meta.len();
            let used = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push((used, meta.len(), entry.path()));
        }
    }
    if total <= limit {
        return Ok(0);
    }
    files.sort_by_key(|(used, ..)| *used);
    let mut freed = 0;
    for (_, len, path) in files {
        if total - freed <= keep {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            freed += len;
        }
    }
    Ok(freed)
}

impl Inner {
    fn cache_path(&self, url: &str) -> PathBuf {
        let digest = Sha1::digest(url.as_bytes());
        let mut name = String::with_capacity(40);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(name, "{byte:02x}");
        }
        self.cache_dir.join(name)
    }

    async fn fetch(self: &Arc<Self>, url: &str) -> Result<Arc<[u8]>, String> {
        if let Some(Entry::Ready {
            bytes: Some(bytes), ..
        }) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(url)
        {
            return Ok(Arc::clone(bytes));
        }
        let path = self.cache_path(url);
        let cached = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                let bytes = std::fs::read(&path).ok()?;
                // Marks the file as used, so pruning keeps it longest.
                if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                    let _ = file.set_modified(std::time::SystemTime::now());
                }
                Some(bytes)
            }
        })
        .await
        .ok()
        .flatten();
        let bytes: Arc<[u8]> = match cached {
            Some(bytes) if !bytes.is_empty() => Arc::from(bytes),
            _ => {
                let mut response = self
                    .http
                    .client()?
                    .get(url)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("artwork request failed: {}", response.status()));
                }
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_ART_BYTES as u64)
                {
                    return Err("artwork is too large".to_string());
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                    if bytes.len() + chunk.len() > MAX_ART_BYTES {
                        return Err("artwork is too large".to_string());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let bytes: Arc<[u8]> = Arc::from(bytes.as_ref());
                let write_path = path.clone();
                let payload = Arc::clone(&bytes);
                self.runtime.spawn_blocking(move || {
                    let temporary = write_path.with_extension("part");
                    if std::fs::write(&temporary, &payload).is_ok() {
                        let _ = std::fs::rename(&temporary, &write_path);
                    }
                });
                bytes
            }
        };
        Ok(bytes)
    }

    fn start_headless(self: &Arc<Self>, waker: crate::backend::Waker, url: String) {
        let loader = Arc::clone(self);
        self.runtime.spawn(async move {
            let result = loader.fetch(&url).await;
            let entry = match result {
                Ok(_bytes) => Entry::Ready { bytes: None },
                Err(_) => Entry::Failed,
            };
            loader
                .entries
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(url, entry);
            waker.wake();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn serve_artwork(status: &str, bytes: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        serve_artwork_sized(status, bytes, true).await
    }

    async fn serve_artwork_sized(
        status: &str,
        bytes: Vec<u8>,
        declare_length: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/cover", listener.local_addr().unwrap());
        let length = if declare_length {
            format!("Content-Length: {}\r\n", bytes.len())
        } else {
            String::new()
        };
        let header = format!("HTTP/1.1 {status}\r\n{length}Connection: close\r\n\r\n");
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                    assert!(request.len() < 8192);
                }
                socket.write_all(header.as_bytes()).await.unwrap();
                let _ = socket.write_all(&bytes).await;
            })
            .await
            .expect("the owned artwork request completes");
        });
        (url, server)
    }

    fn artwork_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn artwork_test_loader(runtime: &tokio::runtime::Runtime, dir: PathBuf) -> ArtLoader {
        ArtLoader::new(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            runtime.handle().clone(),
            dir,
        )
    }

    async fn wait_for_artwork_file(path: &std::path::Path, expected: &[u8]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if tokio::fs::read(path)
                    .await
                    .is_ok_and(|bytes| bytes == expected)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the background writer completes while its runtime is alive");
    }

    #[test]
    fn pruning_drops_the_least_recently_used_covers_first() {
        let dir = std::env::temp_dir().join(format!("swiftspot-art-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let day = Duration::from_secs(86_400);
        let now = std::time::SystemTime::now();
        for (name, age) in [("old", 3), ("middle", 2), ("new", 1)] {
            let path = dir.join(name);
            std::fs::write(&path, [0u8; 100]).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - day * age).unwrap();
        }
        assert_eq!(prune_cache(&dir, 300, 100).unwrap(), 0, "under the limit");
        assert_eq!(prune_cache(&dir, 250, 150).unwrap(), 200);
        assert!(!dir.join("old").exists());
        assert!(!dir.join("middle").exists());
        assert!(dir.join("new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downloaded_artwork_survives_caller_drop_and_reloads_without_network() {
        let dir =
            std::env::temp_dir().join(format!("swiftspot-art-roundtrip-{}", std::process::id()));
        let runtime = artwork_test_runtime();
        let loader = artwork_test_loader(&runtime, dir.clone());
        let expected: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
        let url = runtime.block_on(async {
            let (url, server) = serve_artwork("200 OK", expected.clone()).await;
            let bytes = loader.fetch(&url).await.expect("downloaded artwork");
            assert_eq!(&*bytes, expected.as_slice());
            drop(bytes);
            server.await.unwrap();
            wait_for_artwork_file(&loader.inner.cache_path(&url), &expected).await;
            url
        });
        let path = loader.inner.cache_path(&url);
        drop(loader);
        runtime.shutdown_timeout(Duration::from_secs(10));
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert!(!path.with_extension("part").exists());

        let runtime = artwork_test_runtime();
        let loader = artwork_test_loader(&runtime, dir.clone());
        assert_eq!(&*runtime.block_on(loader.fetch(&url)).unwrap(), expected);
        assert_eq!(loader.cached_file(&url), Some(path));
        drop(loader);
        runtime.shutdown_timeout(Duration::from_secs(10));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn artwork_cache_write_failure_keeps_download_usable() {
        let dir = std::env::temp_dir().join(format!(
            "swiftspot-art-write-failure-{}",
            std::process::id()
        ));
        let runtime = artwork_test_runtime();
        let loader = artwork_test_loader(&runtime, dir.clone());
        let (bytes, path) = runtime.block_on(async {
            let (url, server) = serve_artwork("200 OK", b"complete artwork".to_vec()).await;
            let path = loader.inner.cache_path(&url);
            std::fs::create_dir(&path).unwrap();
            let bytes = loader
                .fetch(&url)
                .await
                .expect("display does not depend on caching");
            server.await.unwrap();
            wait_for_artwork_file(&path.with_extension("part"), b"complete artwork").await;
            (bytes, path)
        });
        drop(loader);
        runtime.shutdown_timeout(Duration::from_secs(10));
        assert_eq!(&*bytes, b"complete artwork");
        assert!(path.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_artwork_responses_do_not_create_cache_files() {
        let dir =
            std::env::temp_dir().join(format!("swiftspot-art-rejected-{}", std::process::id()));
        let runtime = artwork_test_runtime();
        let loader = artwork_test_loader(&runtime, dir.clone());
        runtime.block_on(async {
            for (status, body, declare_length, message) in [
                (
                    "404 Not Found",
                    Vec::new(),
                    true,
                    "artwork request failed: 404",
                ),
                (
                    "200 OK",
                    vec![1; MAX_ART_BYTES + 1],
                    true,
                    "artwork is too large",
                ),
                (
                    "200 OK",
                    vec![1; MAX_ART_BYTES + 1],
                    false,
                    "artwork is too large",
                ),
            ] {
                let (url, server) = serve_artwork_sized(status, body, declare_length).await;
                let error = loader.fetch(&url).await.unwrap_err();
                assert!(error.starts_with(message), "{error}");
                server.await.unwrap();
            }
        });
        drop(loader);
        runtime.shutdown_timeout(Duration::from_secs(10));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
