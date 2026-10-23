//! Fakes and helpers for the knowledge tool's tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::embed::{EmbedError, Embedder, ModelInfo};
use super::store::{BucketConfig, Store};
use crate::tools::BoxFuture;

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-knowledge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(super) fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An index store over buckwal's in-memory bucket, encrypted with a throwaway key.
pub(super) fn memory_store(folder: &TempFolder) -> Arc<Store> {
    let key = folder.0.join("key");
    std::fs::write(&key, "ab".repeat(32)).unwrap();
    let bucket = BucketConfig {
        endpoint: "http://127.0.0.1:1".into(),
        bucket: "test".into(),
        prefix: "harold-knowledge/test".into(),
        access_key: "ak".into(),
        secret_key: "sk".into(),
        encryption_key_file: key,
    };
    let store = Arc::new(buckwal::store::MemStore::new());
    Arc::new(Store::open_with_store(&folder.0.join("db"), &bucket, store).unwrap())
}

pub(super) const DIMENSIONS: usize = 32;

/// Embeds a text as a normalised bag of its words, hashed into `DIMENSIONS` slots, so texts
/// that share words are close. Counts the texts it was asked to embed.
pub(super) struct FakeEmbedder {
    pub embedded: Mutex<Vec<String>>,
    pub fail: Mutex<Option<EmbedError>>,
    pub model: Mutex<String>,
}

impl FakeEmbedder {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            embedded: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            model: Mutex::new("fake-model".into()),
        })
    }

    pub(super) fn embedded_count(&self) -> usize {
        self.embedded.lock().unwrap().len()
    }

    pub(super) fn vector(text: &str) -> Vec<f32> {
        let mut v = vec![0.0_f32; DIMENSIONS];
        for word in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let slot = word
                .bytes()
                .fold(7_usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize))
                % DIMENSIONS;
            v[slot] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        } else {
            v[0] = 1.0;
        }
        v
    }
}

impl Embedder for FakeEmbedder {
    fn ready(&self) -> BoxFuture<'_, Result<ModelInfo, EmbedError>> {
        Box::pin(async move {
            if let Some(error) = *self.fail.lock().unwrap() {
                return Err(error);
            }
            Ok(ModelInfo {
                runtime: "fake".into(),
                model: self.model.lock().unwrap().clone(),
                dimensions: DIMENSIONS,
                prompts: "fake".into(),
            })
        })
    }

    fn embed(&self, texts: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, EmbedError>> {
        Box::pin(async move {
            if let Some(error) = *self.fail.lock().unwrap() {
                return Err(error);
            }
            self.embedded.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts.iter().map(|t| Self::vector(t)).collect())
        })
    }

    fn state(&self) -> &'static str {
        "ready"
    }
}
