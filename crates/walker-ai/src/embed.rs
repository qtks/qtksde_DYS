//! Local Chinese embedding via ONNX Runtime + bge-small-zh-v1.5.
//!
//! The model is never swapped silently: `MODEL_NAME` and `EMBED_DIM` are
//! constants, and a model directory that does not match them is rejected.
//! Everything here only *reads* user files.

use std::path::{Path, PathBuf};

use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::db::config_error;
use crate::Result;

pub const MODEL_NAME: &str = "bge-small-zh-v1.5";
pub const EMBED_DIM: usize = 512;

/// Bytes read from a file body when building the embedding text. bge-small
/// truncates at 512 tokens (~500 CJK characters), so reading more than this
/// would only waste I/O.
pub const BODY_PREFIX_BYTES: usize = 4096;
pub const BODY_PREFIX_CHARS: usize = 800;

/// Files larger than this are not embedded: body truncation would throw away
/// almost all of their content, and the index would fill up with near-identical
/// vectors for media archives.
pub const MAX_EMBED_FILE_BYTES: u64 = 512 * 1024;

pub const BATCH_SIZE: usize = 8;

pub fn default_model_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PANOPTICON_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.models")
        .join(MODEL_NAME)
}

/// ONNX Runtime shared library bundled next to the model (the vendor CDN is
/// unreachable here, so the `.so` comes from the official pypi wheel).
pub fn default_ort_lib() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PANOPTICON_ORT_LIB") {
        return Some(PathBuf::from(path));
    }
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.models/onnxruntime/onnxruntime/capi/libonnxruntime.so"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.models/onnxruntime/libonnxruntime.so"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub dir: String,
    pub model_path: String,
    pub tokenizer_path: String,
    pub name: &'static str,
    pub dim: usize,
    pub model_bytes: u64,
}

pub fn model_info(dir: &Path) -> Result<ModelInfo> {
    let model_path = dir.join("onnx/model.onnx");
    let tokenizer_path = dir.join("tokenizer.json");
    if !model_path.is_file() {
        return Err(config_error(format!(
            "bge model not found at {} (download it to .models/{MODEL_NAME}; \
             the code never falls back to another model)",
            model_path.display()
        )));
    }
    if !tokenizer_path.is_file() {
        return Err(config_error(format!(
            "tokenizer.json not found at {}",
            tokenizer_path.display()
        )));
    }
    let model_bytes = std::fs::metadata(&model_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    Ok(ModelInfo {
        dir: dir.display().to_string(),
        model_path: model_path.display().to_string(),
        tokenizer_path: tokenizer_path.display().to_string(),
        name: MODEL_NAME,
        dim: EMBED_DIM,
        model_bytes,
    })
}

/// One loaded ONNX session plus its tokenizer. `Session::run` needs `&mut self`,
/// so callers own this behind a `&mut`.
pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    _env: Environment,
    input_names: Vec<String>,
    pub info: ModelInfo,
}

impl Embedder {
    pub fn load(dir: &Path) -> Result<Self> {
        let info = model_info(dir)?;
        let builder = match std::env::var_os("ORT_DYLIB_PATH") {
            Some(_) => ort::init(),
            None => match default_ort_lib() {
                Some(lib) => ort::init_from(&lib).map_err(|error| {
                    config_error(format!(
                        "onnxruntime dynamic load failed from {}: {error} \
                         (set ORT_DYLIB_PATH to a libonnxruntime.so)",
                        lib.display()
                    ))
                })?,
                None => ort::init(),
            },
        };
        let env = builder
            .with_name("panopticon")
            .build()
            .map_err(|error| config_error(format!("onnxruntime init failed: {error}")))?;
        let session = Session::builder(&env)
            .and_then(|mut builder| builder.commit_from_file(&info.model_path))
            .map_err(|error| config_error(format!("onnxruntime session failed: {error}")))?;
        let input_names = session
            .inputs()
            .iter()
            .map(|outlet| outlet.name().to_string())
            .collect();

        let mut tokenizer = Tokenizer::from_file(&info.tokenizer_path)
            .map_err(|error| config_error(format!("tokenizer load failed: {error}")))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 512,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                ..Default::default()
            }))
            .map_err(|error| config_error(format!("tokenizer truncation failed: {error}")))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));

        Ok(Self {
            session,
            tokenizer,
            _env: env,
            input_names,
            info,
        })
    }

    pub fn dim(&self) -> usize {
        EMBED_DIM
    }

    /// Embed a batch of texts. CLS pooling + L2 normalization, matching the
    /// card guidance for bge-*-zh-v1.5.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|error| config_error(format!("tokenize failed: {error}")))?;
        let batch = encodings.len();
        let seq = encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for encoding in &encodings {
            let row = encoding.get_ids();
            ids.extend(row.iter().map(|value| *value as i64));
            mask.extend(
                encoding
                    .get_attention_mask()
                    .iter()
                    .map(|value| *value as i64),
            );
            let pad = seq.saturating_sub(row.len());
            ids.extend(std::iter::repeat_n(0i64, pad));
            mask.extend(std::iter::repeat_n(0i64, pad));
        }

        let mut inputs: Vec<(
            std::borrow::Cow<'_, str>,
            ort::session::SessionInputValue<'_>,
        )> = Vec::new();
        let mut push = |name: &str, data: Vec<i64>| -> Result<()> {
            let tensor = Tensor::from_array(([batch, seq], data))
                .map_err(|error| config_error(format!("input tensor {name}: {error}")))?;
            inputs.push((std::borrow::Cow::Owned(name.to_string()), tensor.into()));
            Ok(())
        };
        let ids_name = pick(&self.input_names, "input_ids");
        let mask_name = pick(&self.input_names, "attention_mask");
        push(&ids_name, ids)?;
        push(&mask_name, mask.clone())?;
        if self.input_names.iter().any(|name| name == "token_type_ids") {
            push("token_type_ids", vec![0i64; batch * seq])?;
        }

        let outputs = self
            .session
            .run(inputs)
            .map_err(|error| config_error(format!("onnxruntime run failed: {error}")))?;

        let (shape, data) = outputs
            .get("sentence_embedding")
            .and_then(|value| value.try_extract_tensor::<f32>().ok())
            .map(|(shape, data)| (shape.to_vec(), data.to_vec()))
            .or_else(|| {
                outputs
                    .get("last_hidden_state")
                    .and_then(|value| value.try_extract_tensor::<f32>().ok())
                    .map(|(shape, data)| (shape.to_vec(), data.to_vec()))
            })
            .ok_or_else(|| {
                config_error("model produced neither sentence_embedding nor last_hidden_state")
            })?;

        let mut vectors = Vec::with_capacity(batch);
        if shape.len() == 2 && shape[1] == EMBED_DIM as i64 {
            // Already pooled by the graph.
            for row in data.chunks_exact(EMBED_DIM) {
                vectors.push(normalize(row)?);
            }
        } else if shape.len() == 3 && shape[2] == EMBED_DIM as i64 {
            // CLS token of every sequence: token 0.
            for index in 0..batch {
                let start = index * seq * EMBED_DIM;
                vectors.push(normalize(&data[start..start + EMBED_DIM])?);
            }
        } else {
            return Err(config_error(format!(
                "unexpected model output shape {shape:?}, expected [batch,{EMBED_DIM}] or [batch,seq,{EMBED_DIM}]"
            )));
        }
        Ok(vectors)
    }
}

fn pick(names: &[String], preferred: &str) -> String {
    if names.iter().any(|name| name == preferred) {
        preferred.to_string()
    } else {
        names
            .first()
            .cloned()
            .unwrap_or_else(|| preferred.to_string())
    }
}

fn normalize(row: &[f32]) -> Result<Vec<f32>> {
    let norm = row.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(config_error("model produced a zero-norm embedding"));
    }
    Ok(row.iter().map(|value| value / norm).collect())
}

/// Text payload for one file: path, file name, and a body prefix.
///
/// Chunking is deliberately *not* used: one vector per file keeps the shard
/// layout simple, and the bge card recommends a single CLS vector for
/// retrieval. Long documents are truncated at 512 tokens by the tokenizer.
pub fn embedding_text(path: &str, name: &str, body: Option<&str>) -> String {
    let mut text = String::with_capacity(path.len() + name.len() + BODY_PREFIX_CHARS);
    text.push_str(path);
    text.push('\n');
    text.push_str(name);
    if let Some(body) = body {
        let snippet: String = body.chars().take(BODY_PREFIX_CHARS).collect();
        if !snippet.is_empty() {
            text.push('\n');
            text.push_str(&snippet);
        }
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    /// Magic bytes said text/*.
    TextMagic,
    /// No magic signature, but the content decodes as UTF-8 with almost no
    /// control characters. Extension names are never used for this decision.
    TextContent,
    /// Binary or invalid text.
    Binary,
    /// Larger than `MAX_EMBED_FILE_BYTES`.
    TooLarge,
    /// Missing or unreadable.
    Unreadable,
}

/// Decide whether a file is textual using magic bytes first, then a
/// content-only UTF-8 heuristic. The extension is deliberately ignored.
pub fn classify_file(path: &Path, size: u64) -> ContentKind {
    if size > MAX_EMBED_FILE_BYTES {
        return ContentKind::TooLarge;
    }
    match infer::get_from_path(path) {
        Ok(Some(kind)) => {
            let mime = kind.mime_type();
            if mime.starts_with("text/")
                || mime == "application/json"
                || mime == "application/xml"
                || mime.ends_with("+xml")
                || mime == "application/javascript"
                || mime == "application/x-sh"
                || mime == "application/x-ndjson"
            {
                ContentKind::TextMagic
            } else {
                ContentKind::Binary
            }
        }
        Ok(None) => match std::fs::read(path) {
            Ok(bytes) => {
                if looks_like_text(&bytes) {
                    ContentKind::TextContent
                } else {
                    ContentKind::Binary
                }
            }
            Err(_) => ContentKind::Unreadable,
        },
        Err(_) => ContentKind::Unreadable,
    }
}

pub fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    // UTF-8 validity over the sample is the strongest cheap signal.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut control = 0usize;
    for character in text.chars() {
        if character == '\n' || character == '\r' || character == '\t' {
            continue;
        }
        if character.is_control() {
            control += 1;
        }
    }
    let length = text.chars().count().max(1);
    control * 100 <= length
}

/// Read the body prefix of a text file. Returns `None` when the file is not
/// readable, and an empty string when it is empty.
pub fn read_prefix(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::with_capacity(BODY_PREFIX_BYTES);
    let mut reader = file.take(BODY_PREFIX_BYTES as u64);
    reader.read_to_end(&mut buffer).ok()?;
    match String::from_utf8(buffer) {
        Ok(text) => Some(text),
        Err(error) => {
            // Cut at the last valid character boundary.
            let bytes = error.into_bytes();
            let mut end = bytes.len();
            while end > 0 {
                if let Ok(text) = std::str::from_utf8(&bytes[..end]) {
                    return Some(text.to_string());
                }
                end -= 1;
            }
            None
        }
    }
}

pub struct TextSample {
    pub file_id: String,
    pub shard: String,
    pub text: String,
    pub mtime: i64,
    pub size: i64,
}

/// Build the text payload for a file row, together with the classification
/// that produced it. `mtime`/`size` come from the M1 index so the caller can
/// decide incrementality.
pub fn sample_for(
    file_id: &str,
    shard: &str,
    path: &str,
    name: &str,
    size: u64,
    mtime: i64,
) -> (ContentKind, Option<TextSample>) {
    let kind = classify_file(Path::new(path), size);
    if !matches!(kind, ContentKind::TextMagic | ContentKind::TextContent) {
        return (kind, None);
    }
    let body = read_prefix(Path::new(path)).unwrap_or_default();
    (
        kind,
        Some(TextSample {
            file_id: file_id.to_string(),
            shard: shard.to_string(),
            text: embedding_text(path, name, Some(&body)),
            mtime,
            size: size as i64,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_heuristic_rejects_binary() {
        assert!(looks_like_text("fn main() {}\n".as_bytes()));
        assert!(!looks_like_text(&[0u8, 1, 2, 3, 255]));
    }

    #[test]
    fn embedding_text_includes_path_and_body() {
        let text = embedding_text("/a/b/serial.py", "serial.py", Some("import serial\n"));
        assert!(text.starts_with("/a/b/serial.py\nserial.py\nimport serial"));
    }

    #[test]
    fn missing_model_is_a_config_error_not_a_fallback() {
        let error = model_info(Path::new("/nonexistent-model")).unwrap_err();
        assert!(matches!(error, crate::IndexError::Config(_)));
    }
}
