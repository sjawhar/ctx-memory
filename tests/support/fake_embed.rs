//! A deterministic embedder for tests, so no test calls Vertex AI.
//!
//! One axis per group of words that mean the same thing: two texts that say the same thing in
//! different words land on the same axes and so point the same way, which is what a meaning
//! match has to find. An extra axis carries a small constant, so an empty text still has a
//! direction and cosine is defined for it.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use ctx_memory::embed::Embed;

/// Words that mean the same thing, one group per axis.
const CONCEPTS: [&[&str]; 5] = [
    &["vendor", "supplier", "acme"],
    &["contract", "agreement", "deal"],
    &["renewal", "renew", "extend", "extends"],
    &["march", "spring", "q1"],
    &["hiring", "recruiting", "candidate", "interview"],
];

/// The constant axis, so a text that names no concept is still a unit vector.
const FLOOR: f32 = 0.05;

/// The vector this fake gives `text`, which a fake Vertex answer also uses.
#[must_use]
pub fn vector(text: &str) -> Vec<f32> {
    let mut values = vec![0.0_f32; CONCEPTS.len() + 1];
    for word in text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let word = word.to_lowercase();
        for (axis, group) in CONCEPTS.iter().enumerate() {
            if group.contains(&word.as_str()) {
                if let Some(value) = values.get_mut(axis) {
                    *value += 1.0;
                }
            }
        }
    }
    if let Some(last) = values.last_mut() {
        *last = FLOOR;
    }
    let norm = values
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt()
        .max(f32::MIN_POSITIVE);
    for value in &mut values {
        *value /= norm;
    }
    values
}

/// A fake embedder that counts what it was asked to embed.
pub struct ConceptEmbedder {
    model: String,
    calls: AtomicUsize,
    texts: Mutex<Vec<String>>,
}

impl ConceptEmbedder {
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            calls: AtomicUsize::new(0),
            texts: Mutex::new(Vec::new()),
        }
    }

    /// How many texts this embedder was asked to embed.
    #[must_use]
    pub fn embedded(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// Every text this embedder was asked to embed, in order.
    #[must_use]
    pub fn texts(&self) -> Vec<String> {
        self.texts
            .lock()
            .map(|texts| texts.clone())
            .unwrap_or_default()
    }
}

impl Embed for ConceptEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::Relaxed);
        if let Ok(mut seen) = self.texts.lock() {
            seen.extend(texts.iter().map(|text| (*text).to_owned()));
        }
        Ok(texts.iter().map(|text| vector(text)).collect())
    }
}
