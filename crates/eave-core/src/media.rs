//! Images on a user turn.
//!
//! The journal and the transcript stay text (the composed prompt with
//! `@mentions`). Image **bytes** are send-time only: the engine hands each
//! adapter the local path, the adapter reads it, and the adapter encodes
//! the vendor's documented content part. Nothing here is a blob store.
//!
//! Wire shapes are copied from T3 Code's production adapters, not invented:
//!
//! - Claude stream-json: `{type:image, source:{type:base64, media_type, data}}`
//!   (`t3code/apps/server/src/provider/Layers/ClaudeAdapter.ts`
//!   `buildClaudeImageContentBlock`)
//! - Codex `turn/start` input: `{type:image, url:"data:{mime};base64,{data}"}`
//!   (`CodexAdapter.ts` `resolveAttachment` + `CodexSessionRuntime.ts`
//!   `buildTurnStartParams`)
//! - ACP `session/prompt` (Grok, Hermes): `{type:image, data, mimeType}`
//!   (`GrokAdapter.ts`; Cursor uses the same block, but Cursor is not on
//!   this tree)
//!
//! Pi's prompt is a string with no content array. Images still travel as
//! `@mentions` in that string; this module is not called.
//!
//! Mime types and the 10 MiB cap are T3's
//! `PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES` /
//! `PROVIDER_SEND_TURN_MAX_IMAGE_BYTES` in
//! `packages/contracts/src/orchestration.ts`.

use std::io;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

/// T3 Code `PROVIDER_SEND_TURN_MAX_IMAGE_BYTES`.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// One image the operator staged, to be encoded at send time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub path: PathBuf,
    /// `image/png` and friends. Callers set this from [`send_media_type`].
    pub media_type: String,
}

/// Bytes the adapter puts on the vendor's image part. Never journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImage {
    pub media_type: String,
    pub base64: String,
}

/// T3's send-turn image types: gif, jpeg, png, webp. Broader extensions
/// (svg, bmp, heic, tiff) can still chip and mention; they do not become
/// native image blocks.
pub fn send_media_type(path: impl AsRef<Path>) -> Option<&'static str> {
    match path
        .as_ref()
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Read `img.path` and base64 it. `Ok(None)` = the file is empty, larger
/// than [`MAX_IMAGE_BYTES`], or not a send-turn mime — the `@mention` in
/// the prompt still names it. `Err` = the path could not be read.
pub fn encode_ref(img: &ImageRef) -> io::Result<Option<EncodedImage>> {
    if !is_send_mime(&img.media_type) {
        return Ok(None);
    }
    let bytes = std::fs::read(&img.path)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Ok(None);
    }
    Ok(Some(EncodedImage {
        media_type: img.media_type.clone(),
        base64: BASE64.encode(&bytes),
    }))
}

/// Encode every ref, skipping the ones [`encode_ref`] returns `None` for.
pub fn encode_all(images: &[ImageRef]) -> io::Result<Vec<EncodedImage>> {
    let mut out = Vec::with_capacity(images.len());
    for img in images {
        if let Some(enc) = encode_ref(img)? {
            out.push(enc);
        }
    }
    Ok(out)
}

/// Claude stream-json image content block.
pub fn claude_image_block(img: &EncodedImage) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": img.media_type,
            "data": img.base64,
        },
    })
}

/// ACP `ContentBlock` image (Grok / Hermes `session/prompt`).
pub fn acp_image_block(img: &EncodedImage) -> Value {
    json!({
        "type": "image",
        "data": img.base64,
        "mimeType": img.media_type,
    })
}

/// Codex `turn/start` `UserInput` image item (T3's data-URL form).
pub fn codex_image_item(img: &EncodedImage) -> Value {
    json!({
        "type": "image",
        "url": format!("data:{};base64,{}", img.media_type, img.base64),
    })
}

fn is_send_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1×1 transparent PNG.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn tmp_png() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eave-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, TINY_PNG).unwrap();
        path
    }

    #[test]
    fn send_media_type_is_t3s_four() {
        assert_eq!(send_media_type("a.PNG"), Some("image/png"));
        assert_eq!(send_media_type("a.jpg"), Some("image/jpeg"));
        assert_eq!(send_media_type("a.jpeg"), Some("image/jpeg"));
        assert_eq!(send_media_type("a.gif"), Some("image/gif"));
        assert_eq!(send_media_type("a.webp"), Some("image/webp"));
        assert_eq!(send_media_type("a.svg"), None);
        assert_eq!(send_media_type("a.bmp"), None);
        assert_eq!(send_media_type("a.rs"), None);
    }

    #[test]
    fn encode_ref_reads_the_file_at_send_time() {
        let path = tmp_png();
        let img = ImageRef {
            path: path.clone(),
            media_type: "image/png".into(),
        };
        let enc = encode_ref(&img).unwrap().expect("tiny png encodes");
        assert_eq!(enc.media_type, "image/png");
        assert_eq!(enc.base64, BASE64.encode(TINY_PNG));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_silent_skip() {
        let img = ImageRef {
            path: PathBuf::from("/no/such/eave-media.png"),
            media_type: "image/png".into(),
        };
        assert!(encode_ref(&img).is_err());
    }

    #[test]
    fn an_oversize_file_skips_the_native_block() {
        let dir = std::env::temp_dir().join(format!(
            "eave-media-big-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.png");
        let mut bytes = vec![0u8; (MAX_IMAGE_BYTES as usize) + 1];
        bytes[..TINY_PNG.len()].copy_from_slice(TINY_PNG);
        std::fs::write(&path, &bytes).unwrap();
        let img = ImageRef {
            path: path.clone(),
            media_type: "image/png".into(),
        };
        assert_eq!(encode_ref(&img).unwrap(), None);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn claude_block_is_t3s_source_object() {
        let img = EncodedImage {
            media_type: "image/png".into(),
            base64: "QQ==".into(),
        };
        let v = claude_image_block(&img);
        assert_eq!(v["type"], "image");
        assert_eq!(v["source"]["type"], "base64");
        assert_eq!(v["source"]["media_type"], "image/png");
        assert_eq!(v["source"]["data"], "QQ==");
    }

    #[test]
    fn acp_block_is_t3s_content_block() {
        let img = EncodedImage {
            media_type: "image/jpeg".into(),
            base64: "QQ==".into(),
        };
        let v = acp_image_block(&img);
        assert_eq!(v["type"], "image");
        assert_eq!(v["data"], "QQ==");
        assert_eq!(v["mimeType"], "image/jpeg");
    }

    #[test]
    fn codex_item_is_t3s_data_url() {
        let img = EncodedImage {
            media_type: "image/webp".into(),
            base64: "QQ==".into(),
        };
        let v = codex_image_item(&img);
        assert_eq!(v["type"], "image");
        assert_eq!(v["url"], "data:image/webp;base64,QQ==");
    }
}
