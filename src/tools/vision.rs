//! Vision: `view_image` reads an image file and attaches it to the model
//! context as a base64 ContentBlock so a vision-capable model can see it.

use crate::tools::{err_json, ToolContext};
use base64::Engine as _;

const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;

/// Infer a MIME type from a file extension.
pub fn media_type_for(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Read an image file, returning (media_type, base64 data).
pub fn load_image(path: &std::path::Path) -> Result<(String, String), String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {:.1} MB; images over 15 MB are not supported",
            path.display(),
            meta.len() as f64 / 1e6
        ));
    }
    let media = media_type_for(path).ok_or_else(|| {
        format!(
            "{}: unsupported image type (png/jpg/gif/webp only)",
            path.display()
        )
    })?;
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    Ok((
        media.to_string(),
        base64::engine::general_purpose::STANDARD.encode(&bytes),
    ))
}

pub fn view_image(
    ctx: &ToolContext,
    args: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
        return Ok(err_json(
            "view_image: missing required argument `path`".into(),
        ));
    };
    let path = ctx.resolve(path_str);
    if !path.is_file() {
        return Ok(err_json(format!(
            "view_image: {} is not a file",
            path.display()
        )));
    }
    let (media, data) = match load_image(&path) {
        Ok(v) => v,
        Err(msg) => return Ok(err_json(format!("view_image: {msg}"))),
    };
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "media_type": media,
        "size_bytes": data.len() * 3 / 4,
        "attached": true,
        "note": "The image has been attached to the conversation for vision-capable models.",
    }))
}

/// Caller-side hook: re-read the image a `view_image` call pointed at so the
/// agent loop can attach it as a real image ContentBlock.
pub fn attachment_for(args: &serde_json::Value) -> Option<(String, String)> {
    let path = args.get("path")?.as_str()?;
    load_image(std::path::Path::new(path)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_media_types() {
        assert_eq!(
            media_type_for(std::path::Path::new("a.png")),
            Some("image/png")
        );
        assert_eq!(
            media_type_for(std::path::Path::new("a.JPG")),
            Some("image/jpeg")
        );
        assert_eq!(
            media_type_for(std::path::Path::new("a.webp")),
            Some("image/webp")
        );
        assert_eq!(media_type_for(std::path::Path::new("a.txt")), None);
    }

    #[test]
    fn loads_and_encodes_small_png() {
        let dir = std::env::temp_dir().join(format!("fxrs-vision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("px.png");
        // 1x1 transparent PNG
        std::fs::write(&f, [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let (media, data) = load_image(&f).unwrap();
        assert_eq!(media, "image/png");
        assert!(!data.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
