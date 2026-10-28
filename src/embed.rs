//! Text embeddings for meaning search.
//!
//! An embedder turns a piece of text into a vector; two texts that mean the same thing point
//! the same way, so a search or a delta can match a record a new message belongs to even when
//! the two share no words. The vectors come from Vertex AI under the server's own Google
//! identity, so no record or message text leaves Google Cloud. Each record and message vector
//! is cached as a file keyed by the content hash, under the data directory, so a rebuild
//! recomputes nothing. A search query's vector is computed afresh and never kept.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::api::client;
use crate::atomic::write_replace;
use crate::credentials;

/// Cosine at or above which two texts are taken to mean the same thing.
///
/// Embeddings of unrelated texts sit well below this; a paraphrase sits near 1. The value
/// keeps an unrelated record out of a result while admitting a true restatement.
pub const MEANING_THRESHOLD: f32 = 0.7;

/// A source of text embeddings.
pub trait Embed: Send + Sync {
    /// The model identifier stored beside each cached vector.
    fn model(&self) -> &str;

    /// Embeds every text, returning one vector per input in the same order.
    ///
    /// # Errors
    ///
    /// Returns an error when the embeddings cannot be computed.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embeds search queries, returning one vector per query in the same order.
    ///
    /// A query's vector is never kept: a caching embedder computes it afresh and writes
    /// nothing, so its cache holds only record and message vectors and no caller can grow it
    /// by searching. The default is [`Embed::embed`], which is right for an embedder that
    /// keeps nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the embeddings cannot be computed.
    fn embed_queries(&self, queries: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(queries)
    }
}

/// Whether meaning search is available for a tree, and the embedder when it is.
pub enum Embedding {
    /// Meaning search is not configured; `ctx` matches by text alone.
    Off,
    /// An embedder is configured and ready.
    Ready(Arc<dyn Embed>),
}

impl Embedding {
    /// The embedder, or `None` when meaning search is off.
    #[must_use]
    pub fn embedder(&self) -> Option<&dyn Embed> {
        match self {
            Self::Off => None,
            Self::Ready(embedder) => Some(embedder.as_ref()),
        }
    }
}

/// The cosine similarity of two vectors, or 0 when either has no length or they differ in
/// dimension. Vertex returns normalized vectors, so this is their dot product.
#[must_use]
pub fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return 0.0;
    }
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let norm_left = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let norm_right = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm_left == 0.0 || norm_right == 0.0 {
        return 0.0;
    }
    dot / (norm_left * norm_right)
}

/// Vertex AI's text-embedding model, called over its `:predict` endpoint.
pub struct Embedder {
    model: String,
    endpoint: String,
    dimensions: usize,
    batch: usize,
}

impl Embedder {
    /// Builds a Vertex embedder for `model`, reading the project and region from the
    /// environment the deployment sets.
    ///
    /// # Errors
    ///
    /// Returns an error when `GOOGLE_CLOUD_PROJECT` or `GOOGLE_CLOUD_LOCATION` is unset.
    pub fn vertex(model: &str) -> Result<Self> {
        let project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .map_err(|_| anyhow!("GOOGLE_CLOUD_PROJECT is required for meaning search"))?;
        let location = std::env::var("GOOGLE_CLOUD_LOCATION")
            .map_err(|_| anyhow!("GOOGLE_CLOUD_LOCATION is required for meaning search"))?;
        let base = std::env::var("CTX_VERTEX_API_BASE")
            .unwrap_or_else(|_| format!("https://{location}-aiplatform.googleapis.com"));
        let endpoint = format!(
            "{}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:predict",
            base.trim_end_matches('/')
        );
        Ok(Self {
            model: model.to_owned(),
            endpoint,
            dimensions: dimensions_for(model),
            batch: batch_for(model),
        })
    }
}

impl Embed for Embedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch.max(1)) {
            let token = credentials::metadata_token()
                .context("reading the Google identity for Vertex AI")?;
            let instances = chunk
                .iter()
                .map(|text| json!({ "content": text }))
                .collect::<Vec<_>>();
            let body = json!({
                "instances": instances,
                "parameters": { "outputDimensionality": self.dimensions },
            });
            let (status, _headers, value) =
                client::post_json(&self.endpoint, token.expose(), &body)
                    .context("calling Vertex AI")?;
            if !status.is_success() {
                bail!("Vertex AI answered {status}");
            }
            let predictions = value
                .get("predictions")
                .and_then(Value::as_array)
                .context("Vertex AI returned no predictions")?;
            if predictions.len() != chunk.len() {
                bail!(
                    "Vertex AI returned {} vectors for {} inputs",
                    predictions.len(),
                    chunk.len()
                );
            }
            for prediction in predictions {
                vectors.push(parse_vector(prediction)?);
            }
        }
        Ok(vectors)
    }
}

/// The embedding of one Vertex prediction.
fn parse_vector(prediction: &Value) -> Result<Vec<f32>> {
    prediction
        .pointer("/embeddings/values")
        .and_then(Value::as_array)
        .context("a Vertex prediction carried no embedding values")?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|number| {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "an embedding is kept as f32, as its cache file is"
                    )]
                    let narrowed = number as f32;
                    narrowed
                })
                .context("a Vertex embedding value was not a number")
        })
        .collect()
}

/// The output dimension `ctx` asks Vertex for, by model.
fn dimensions_for(model: &str) -> usize {
    if model.starts_with("gemini-embedding") {
        3072
    } else {
        768
    }
}

/// How many inputs one `:predict` request may carry, by model. `gemini-embedding` models
/// accept a single input; the others accept several.
fn batch_for(model: &str) -> usize {
    if model.starts_with("gemini-embedding") {
        1
    } else {
        5
    }
}

/// An embedder that keeps each record and message vector as a file keyed by the content hash,
/// so the same text is embedded once across a rebuild, a search and a delta. A query is never
/// kept (see [`Embed::embed_queries`]).
pub struct CachedEmbedder {
    inner: Arc<dyn Embed>,
    dir: PathBuf,
}

impl CachedEmbedder {
    /// Caches `inner`'s vectors under `dir`, which is `<data_dir>/embeddings`.
    #[must_use]
    pub fn new(inner: Arc<dyn Embed>, dir: PathBuf) -> Self {
        Self { inner, dir }
    }

    fn path(&self, text: &str) -> PathBuf {
        let hash = Sha256::digest(text.as_bytes());
        self.dir.join(format!("{hash:x}.json"))
    }

    fn cached(&self, text: &str) -> Option<Vec<f32>> {
        let bytes = std::fs::read(self.path(text)).ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        (entry.model == self.inner.model()).then_some(entry.vector)
    }

    fn store(&self, text: &str, vector: &[f32]) -> Result<()> {
        let entry = CacheEntry {
            model: self.inner.model().to_owned(),
            dims: vector.len(),
            vector: vector.to_vec(),
        };
        let bytes = serde_json::to_vec(&entry).context("serializing an embedding")?;
        write_replace(&self.path(text), &bytes)
    }
}

impl Embed for CachedEmbedder {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut vectors: Vec<Option<Vec<f32>>> =
            texts.iter().map(|text| self.cached(text)).collect();
        let misses: Vec<(usize, &str)> = texts
            .iter()
            .zip(vectors.iter())
            .enumerate()
            .filter_map(|(position, (text, cached))| cached.is_none().then_some((position, *text)))
            .collect();
        if !misses.is_empty() {
            let miss_texts = misses.iter().map(|(_, text)| *text).collect::<Vec<_>>();
            let fresh = self.inner.embed(&miss_texts)?;
            if fresh.len() != misses.len() {
                bail!(
                    "the embedder returned {} vectors for {} inputs",
                    fresh.len(),
                    misses.len()
                );
            }
            for ((position, text), vector) in misses.into_iter().zip(fresh) {
                self.store(text, &vector)?;
                if let Some(slot) = vectors.get_mut(position) {
                    *slot = Some(vector);
                }
            }
        }
        vectors
            .into_iter()
            .enumerate()
            .map(|(position, vector)| {
                vector
                    .ok_or_else(|| anyhow!("embedding {position} was neither cached nor computed"))
            })
            .collect()
    }

    fn embed_queries(&self, queries: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed_queries(queries)
    }
}

/// One cached vector on disk.
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    model: String,
    dims: usize,
    vector: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::{cosine, CachedEmbedder, Embed};

    struct Counting {
        model: String,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl Counting {
        fn new(model: &str) -> Self {
            Self {
                model: model.to_owned(),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Embed for Counting {
        fn model(&self) -> &str {
            &self.model
        }

        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(texts.len(), Ordering::Relaxed);
            if let Ok(mut seen) = self.seen.lock() {
                seen.extend(texts.iter().map(|text| (*text).to_owned()));
            }
            Ok(texts
                .iter()
                .map(|text| vec![if text.is_empty() { 0.0 } else { 1.0 }, 2.0])
                .collect())
        }
    }

    #[test]
    fn cosine_is_one_for_the_same_direction_and_zero_across_dimensions() {
        assert!((cosine(&[1.0, 2.0], &[2.0, 4.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn a_cached_vector_is_computed_once() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("ctx-embed-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let inner = std::sync::Arc::new(Counting::new("fake-1"));
        let cached = CachedEmbedder::new(inner.clone(), dir.clone());

        let first = cached.embed(&["alpha", "beta"])?;
        let second = cached.embed(&["alpha", "beta"])?;
        std::fs::remove_dir_all(&dir)?;

        assert_eq!(first, second);
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "two misses, then two hits"
        );
        Ok(())
    }

    #[test]
    fn a_query_is_embedded_afresh_and_never_kept() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("ctx-embed-query-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let inner = std::sync::Arc::new(Counting::new("fake-1"));
        let cached = CachedEmbedder::new(inner.clone(), dir.clone());

        let first = cached.embed_queries(&["alpha"])?;
        let second = cached.embed_queries(&["alpha"])?;
        let kept = std::fs::read_dir(&dir)?.count();
        std::fs::remove_dir_all(&dir)?;

        assert_eq!(first, second);
        assert_eq!(kept, 0, "a query's vector was written to the cache");
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "each query is computed afresh"
        );
        Ok(())
    }

    #[test]
    fn a_changed_model_recomputes() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("ctx-embed-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let one = std::sync::Arc::new(Counting::new("fake-1"));
        CachedEmbedder::new(one.clone(), dir.clone()).embed(&["alpha"])?;
        let two = std::sync::Arc::new(Counting::new("fake-2"));
        CachedEmbedder::new(two.clone(), dir.clone()).embed(&["alpha"])?;
        std::fs::remove_dir_all(&dir)?;

        assert_eq!(one.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            two.calls.load(Ordering::Relaxed),
            1,
            "a new model ignores the old cache"
        );
        Ok(())
    }
}
