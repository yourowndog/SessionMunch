//! Local cross-encoder reranker implementation (card F1,
//! `cross-encoder/ms-marco-MiniLM-L-6-v2`).
//!
//! Wraps a local candle BERT model behind the [`CrossEncoder`] trait from
//! `sessionmunch_core::sections` (`CrossEncoderTrait` here). The model is a
//! `BertForSequenceClassification`: a BERT encoder over the concatenated
//! `query [SEP] passage` input, pooled through the BERT pooler, then a
//! single linear classification head producing one relevance logit.
//! The repo's `sbert_ce_default_activation_function` is `Identity`, so the
//! raw logit is what the core trait reports (same value the F1 Python
//! benchmark recorded).
//!
//! Follows the [`LocalEmbedder`] precedent in [`local`] for model
//! fetching, pinned-checksum verification, and lazy loading.
//!
//! Two trait surfaces, both fail-open per the core invariants:
//!
//! - [`CrossEncoderTrait`] (sync): returns `None` on any scoring failure,
//!   never a panic — the caller keeps the pre-rerank (RRF) order.
//! - [`Reranker`] (async, what the MCP server consumes): scores every
//!   candidate, sigmoid-maps the logit into `[0, 1]`, and errors the whole
//!   call on inference failure so the server's rerank wrapper preserves the
//!   pre-rerank order (the wrapper, not this type, owns timeout and
//!   concurrency bounds).
//!
//! Off by default: nothing constructs this type yet (the MCP retrieval
//! wiring is card G1, and `SESSIONMUNCH_RERANKER` continues to accept only
//! `llm` until Sam approves enabling the cross-encoder live). `load` turns
//! missing/tampered model files into an error the caller can ignore by
//! simply never constructing the reranker.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{Device, Tensor};
use candle_nn::{Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::error::{LlmError, LlmResult};
use crate::reranker::{RerankCandidate, RerankScore, Reranker};
use sessionmunch_core::CrossEncoder as CrossEncoderTrait;
use sessionmunch_core::CrossEncoderScore;

/// Model identity as stored on disk and used in diagnostics.
pub const CROSS_ENCODER_MODEL: &str = "cross-encoder/ms-marco-MiniLM-L-6-v2";
/// Output dimensionality of the classification head (a single score).
pub const CROSS_ENCODER_DIM: u32 = 1;
/// Token cap per input (the model's positional limit is 512).
const MAX_TOKENS: usize = 512;
/// HuggingFace model base URL.
const MODEL_BASE_URL: &str =
    "https://huggingface.co/cross-encoder/ms-marco-MiniLM-L-6-v2/resolve/main";

/// The three files that make up the model, with sha256s pinned on
/// 2026-09-23 against the upstream repo. A drifted upstream file fails
/// loudly instead of silently changing every score.
pub const MODEL_FILES: [(&str, &str); 3] = [
    (
        "model.safetensors",
        "821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae",
    ),
    (
        "tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
    (
        "config.json",
        "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc",
    ),
];

/// Directory the model lives in under the data dir's `models/` root.
#[must_use]
pub fn model_dir(models_root: &Path) -> PathBuf {
    models_root.join(CROSS_ENCODER_MODEL)
}

/// Whether every model file is present (checksums are verified at load,
/// not here — presence is the cheap serve-startup probe).
#[must_use]
pub fn model_present(models_root: &Path) -> bool {
    let dir = model_dir(models_root);
    MODEL_FILES.iter().all(|(name, _)| dir.join(name).exists())
}

/// Download the model files with pinned checksums (atomic: tmp +
/// verify + rename). Skips files already present and valid; a present
/// file with a wrong checksum is replaced.
///
/// # Errors
/// Network failures, checksum mismatches, and IO errors — the caller
/// (serve startup / reranker construction) surfaces them with the
/// offline instructions, and retrieval simply continues without
/// reranking (fail-open).
pub async fn fetch_model(models_root: &Path) -> LlmResult<()> {
    let dir = model_dir(models_root);
    std::fs::create_dir_all(&dir).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    for (name, pinned) in MODEL_FILES {
        let dest = dir.join(name);
        if let Ok(existing) = std::fs::read(&dest)
            && hex(&Sha256::digest(&existing)) == pinned
        {
            continue;
        }
        tracing::info!(file = name, "fetching cross-encoder model file");
        let bytes = client
            .get(format!("{MODEL_BASE_URL}/{name}"))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let digest = hex(&Sha256::digest(&bytes));
        if digest != pinned {
            return Err(LlmError::UnexpectedShape(format!(
                "downloaded {name} sha256 {digest} does not match pin {pinned}; \
                 refusing to install an unverified model"
            )));
        }
        let tmp = dest.with_extension("part");
        std::fs::write(&tmp, &bytes).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        std::fs::rename(&tmp, &dest).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Map a cross-encoder logit to the `[0, 1]` relevance scale the
/// [`Reranker`] trait promises. The upstream config declares the
/// *Identity* activation (`sbert_ce_default_activation_function`), so the
/// logistic function is applied here rather than at load time.
fn sigmoid(logit: f32) -> f32 {
    1.0 / (1.0 + (-logit).exp())
}

/// The classification head on top of the BERT encoder, mirroring
/// `BertForSequenceClassification`: BERT pooler (Linear + tanh) over the
/// CLS token, then a single Linear to one logit. The upstream weights
/// (`bert.pooler.dense.*`, `classifier.*`) have no tanh/out_proj stack —
/// the two-layer head some cross-encoder exports use does not exist here.
struct CrossEncoderHead {
    pooler: candle_nn::Linear,
    classifier: candle_nn::Linear,
}

impl CrossEncoderHead {
    fn load(vb: &VarBuilder, hidden_size: usize) -> LlmResult<Self> {
        let pooler = candle_nn::linear(hidden_size, hidden_size, vb.pp("bert.pooler.dense"))
            .map_err(|e| LlmError::UnexpectedShape(format!("loading pooler: {e}")))?;
        let classifier = candle_nn::linear(hidden_size, 1, vb.pp("classifier"))
            .map_err(|e| LlmError::UnexpectedShape(format!("loading classifier: {e}")))?;
        Ok(Self { pooler, classifier })
    }

    fn forward(&self, cls: &Tensor) -> LlmResult<Tensor> {
        let pooled = self
            .pooler
            .forward(cls)
            .and_then(|t| t.tanh())
            .map_err(|e| LlmError::UnexpectedShape(format!("pooler forward: {e}")))?;
        self.classifier
            .forward(&pooled)
            .map_err(|e| LlmError::UnexpectedShape(format!("classifier forward: {e}")))
    }
}

/// In-process cross-encoder reranker, [`LocalEmbedder`]-style: cheap to
/// clone-share via `Arc`.
///
/// Both trait surfaces are singleflight on CPU: each pair costs one
/// ~110 ms forward pass (F1 benchmark), so callers must keep the reranker
/// off the async runtime's executor threads and bound concurrency
/// themselves. The [`Reranker`] impl moves its batch onto
/// `spawn_blocking`; the F1-recorded `RERANK_MAX_IN_FLIGHT` /
/// `RERANK_TIMEOUT` wrapper in the MCP server owns the bounds (wiring
/// card G1) — this card only provides the type.
pub struct CrossEncoderReranker {
    inner: Arc<Inner>,
}

struct Inner {
    model: BertModel,
    head: CrossEncoderHead,
    tokenizer: Tokenizer,
    device: Device,
}

impl CrossEncoderReranker {
    /// Load the model from `<models_root>/cross-encoder/ms-marco-MiniLM-L-6-v2/`,
    /// verifying every file against its pinned sha256 first — a tampered
    /// or torn model must not silently produce bogus scores.
    ///
    /// # Errors
    /// Missing files (with the fetch/offline instructions), checksum
    /// mismatches, and model-load failures. All are fail-open at the
    /// callsite: retrieval proceeds with the RRF-fused order.
    pub fn load(models_root: &Path) -> LlmResult<Self> {
        let dir = model_dir(models_root);
        for (name, pinned) in MODEL_FILES {
            let path = dir.join(name);
            let bytes = std::fs::read(&path).map_err(|_| {
                LlmError::NotConfigured(format!(
                    "cross-encoder model file missing: {}. Start the server once \
                     with network access to fetch it, or download \
                     {MODEL_BASE_URL}/{name} manually into {}",
                    path.display(),
                    dir.display(),
                ))
            })?;
            let digest = hex(&Sha256::digest(&bytes));
            if digest != pinned {
                return Err(LlmError::NotConfigured(format!(
                    "cross-encoder model file {} sha256 {digest} does not match \
                     the pinned {pinned}; delete it and re-fetch",
                    path.display(),
                )));
            }
        }

        let device = Device::Cpu;
        let config: BertConfig = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?,
        )
        .map_err(|e| LlmError::UnexpectedShape(format!("parsing model config: {e}")))?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| LlmError::UnexpectedShape(format!("loading tokenizer: {e}")))?;
        // Single-sequence scoring per call (no batching), so no padding —
        // the shipped tokenizer.json enables fixed-length padding, and
        // feeding [PAD] tokens through with an all-ones attention mask
        // pollutes scores.
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| LlmError::UnexpectedShape(format!("configuring truncation: {e}")))?;
        // Buffered (safe) loader: ~91 MB read once into memory.
        let weights = std::fs::read(dir.join("model.safetensors"))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DTYPE, &device)
            .map_err(|e| LlmError::UnexpectedShape(format!("loading model weights: {e}")))?;
        let model = BertModel::load(vb.clone(), &config)
            .map_err(|e| LlmError::UnexpectedShape(format!("building BERT model: {e}")))?;
        let head = CrossEncoderHead::load(&vb, config.hidden_size)?;
        Ok(Self {
            inner: Arc::new(Inner {
                model,
                head,
                tokenizer,
                device,
            }),
        })
    }

    fn score_blocking(inner: &Inner, passage: &str, query: &str) -> LlmResult<f32> {
        // Tokenize as a (query, passage) PAIR so the tokenizer emits the
        // same segmentation as the Python cross-encoder: [CLS] query [SEP]
        // passage [SEP] with segment ids 0/1, instead of a single all-zero
        // segment. The upstream model was fine-tuned on that input shape.
        let encoding = inner
            .tokenizer
            .encode((query.to_string(), passage.to_string()), true)
            .map_err(|e| LlmError::UnexpectedShape(format!("tokenizing: {e}")))?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            return Err(LlmError::UnexpectedShape("empty tokenization".into()));
        }
        let type_ids = encoding.get_type_ids();
        let mask_vals = encoding.get_attention_mask();
        let input_ids = Tensor::new(ids, &inner.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let token_type_ids = Tensor::new(type_ids, &inner.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let attention_mask = Tensor::new(mask_vals, &inner.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let sequence_output = inner
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| LlmError::UnexpectedShape(format!("forward pass: {e}")))?;
        // CLS token (index 0) → BERT pooler → classifier → one logit.
        let cls_token = sequence_output
            .narrow(1, 0, 1)
            .and_then(|t| t.squeeze(1))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let logits = inner.head.forward(&cls_token)?;
        let score = logits
            .get(0)
            .and_then(|t| t.get(0))
            .and_then(|t| t.to_scalar::<f32>())
            .map_err(|e| LlmError::UnexpectedShape(format!("extracting score: {e}")))?;
        if !score.is_finite() {
            return Err(LlmError::UnexpectedShape(format!(
                "non-finite cross-encoder score {score}"
            )));
        }
        Ok(score)
    }
}

impl CrossEncoderTrait for CrossEncoderReranker {
    fn score(&self, passage: &str, query: &str) -> Option<CrossEncoderScore> {
        // Fail-open: any inference error degrades to `None`, and the
        // caller's pre-rerank (RRF) order is preserved untouched.
        match Self::score_blocking(&self.inner, passage, query) {
            Ok(score) => Some(CrossEncoderScore(score)),
            Err(e) => {
                tracing::warn!(error = %e, "cross-encoder scoring failed; keeping RRF order");
                None
            }
        }
    }

    fn score_batch(&self, passages: &[&str], query: &str) -> Vec<Option<CrossEncoderScore>> {
        passages.iter().map(|&p| self.score(p, query)).collect()
    }
}

#[async_trait]
impl Reranker for CrossEncoderReranker {
    fn name(&self) -> &'static str {
        "cross-encoder"
    }

    fn model(&self) -> &str {
        CROSS_ENCODER_MODEL
    }

    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
    ) -> LlmResult<Vec<RerankScore>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let inner = Arc::clone(&self.inner);
        let query = query.to_string();
        let pairs: Vec<(String, String)> = candidates
            .iter()
            .map(|c| (c.id.clone(), c.snippet.clone()))
            .collect();
        // CPU-heavy batch: move off the async executor, then sigmoid-map
        // each logit onto the `[0, 1]` scale. A single failed candidate
        // errors the whole call — the server's rerank wrapper catches the
        // error and preserves the pre-rerank RRF order intact.
        tokio::task::spawn_blocking(move || -> LlmResult<Vec<RerankScore>> {
            let mut scores = Vec::with_capacity(pairs.len());
            for (id, snippet) in pairs {
                let logit = Self::score_blocking(&inner, &snippet, &query)?;
                scores.push(RerankScore {
                    id,
                    relevance: sigmoid(logit),
                });
            }
            Ok(scores)
        })
        .await
        .map_err(|e| LlmError::UnexpectedShape(format!("cross-encoder rerank task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_presence_requires_all_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!model_present(tmp.path()));
        let dir = model_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        for (name, _) in &MODEL_FILES[..2] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert!(!model_present(tmp.path()), "two of three is not present");
        std::fs::write(dir.join(MODEL_FILES[2].0), b"x").unwrap();
        assert!(model_present(tmp.path()));
    }

    #[test]
    fn load_refuses_tampered_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = model_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        for (name, _) in MODEL_FILES {
            std::fs::write(dir.join(name), b"not the real file").unwrap();
        }
        let err = match CrossEncoderReranker::load(tmp.path()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("tampered files must not load"),
        };
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn load_names_the_missing_file_and_the_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let err = match CrossEncoderReranker::load(tmp.path()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("missing files must not load"),
        };
        assert!(err.contains("model.safetensors"), "{err}");
        assert!(err.contains("manually"), "{err}");
    }

    /// Fail-open boundary test: an unusable model directory must produce a
    /// load error, never a panic — the caller keeps RRF order by simply not
    /// constructing the reranker.
    #[test]
    fn missing_model_dir_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(CrossEncoderReranker::load(tmp.path()).is_err());
    }

    #[test]
    fn sigmoid_maps_logits_into_unit_range() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        assert!(sigmoid(10.0) > 0.99);
        assert!(sigmoid(-10.0) < 0.01);
        assert!(sigmoid(-1e9) >= 0.0, "no NaN/overflow on extreme negatives");
    }

    /// Full inference against the real fetched model: `#[ignore]`d like
    /// the embedder's equivalent (needs ~91 MB in a local models dir; run
    /// `cargo test -p sessionmunch-llm --lib cross_encoder -- --ignored` after
    /// a fetch). Calibration ranges are loose deltas over the F1 Python
    /// benchmark run — a pooling/segment/head regression that flips
    /// relevance ordering must fail here.
    #[tokio::test]
    #[ignore = "needs the fetched ms-marco-MiniLM-L-6-v2 model files"]
    async fn real_model_scores_relevant_passages_higher() {
        let root = std::env::var("SESSIONMUNCH_TEST_MODELS_DIR")
            .expect("set SESSIONMUNCH_TEST_MODELS_DIR to a dir containing ms-marco-MiniLM-L-6-v2");
        let reranker = CrossEncoderReranker::load(Path::new(&root)).unwrap();
        let query = "How do I install sessionmunch on my machine?";
        let relevant = "Run cargo build and copy the sessionmunch binary into your PATH to install.";
        let irrelevant = "The forecast says clear skies and a high of 21 degrees tomorrow.";

        let rel = reranker.score(relevant, query).unwrap();
        let irr = reranker.score(irrelevant, query).unwrap();
        assert!(
            rel.0.is_finite() && irr.0.is_finite(),
            "scores must be finite, got {rel:?} / {irr:?}"
        );
        assert!(
            rel.0 > irr.0,
            "relevant passage ({rel:?}) must outrank the unrelated one ({irr:?})"
        );

        let batch = reranker.score_batch(&[relevant, irrelevant], query);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], Some(rel), "batch must match per-item scoring");
        assert_eq!(batch[1], Some(irr));

        // Determinism guard: same pair scores identically across calls.
        let again = reranker.score(relevant, query).unwrap();
        assert_eq!(again, rel);

        // Reranker surface: [0, 1] sigmoid scores with ids echoed, and the
        // relevant candidate strictly outranks the distractor.
        let candidates = vec![
            RerankCandidate {
                id: "rel".into(),
                title: "install".into(),
                snippet: relevant.into(),
            },
            RerankCandidate {
                id: "irr".into(),
                title: "weather".into(),
                snippet: irrelevant.into(),
            },
        ];
        let scores = reranker.rerank(query, &candidates).await.unwrap();
        assert_eq!(scores.len(), 2);
        for s in &scores {
            assert!(
                (0.0..=1.0).contains(&s.relevance),
                "rerank score must be in [0, 1], got {}",
                s.relevance
            );
        }
        let rel_relevance = scores.iter().find(|s| s.id == "rel").unwrap().relevance;
        let irr_relevance = scores.iter().find(|s| s.id == "irr").unwrap().relevance;
        assert!(
            rel_relevance > irr_relevance,
            "relevant candidate ({rel_relevance}) must outrank distractor ({irr_relevance})"
        );
    }
}
