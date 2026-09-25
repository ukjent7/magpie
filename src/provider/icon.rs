use std::{fs, io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{config, settings};

use super::MAX_ICON_BYTES;

pub(super) fn from_value(value: &str) -> Result<String> {
    let path = Path::new(value);
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(value.to_owned());
    };
    if metadata.is_dir() {
        return Ok(value.to_owned());
    }
    ensure!(
        metadata.len() <= MAX_ICON_BYTES as u64,
        "the picture is over 1 MB; pick a smaller one"
    );

    let file = fs::File::open(path).with_context(|| format!("open picture {}", path.display()))?;
    let mut reader = file.take((MAX_ICON_BYTES + 1) as u64);
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_ICON_BYTES as u64) as usize);
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("read picture {}", path.display()))?;
    ensure!(
        bytes.len() <= MAX_ICON_BYTES,
        "the picture is over 1 MB; pick a smaller one"
    );
    let extension = icon_extension(&bytes)
        .context("not a supported picture; use PNG, JPEG, GIF, WebP, ICO, or SVG")?;

    let digest = Sha256::digest(&bytes);
    let mut name = String::with_capacity(16 + extension.len() + 1);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push('.');
    name.push_str(extension);

    let directory = settings::providers_path()
        .parent()
        .context("provider path has no parent directory")?
        .join("icons");
    fs::create_dir_all(&directory)
        .with_context(|| format!("create provider icon directory {}", directory.display()))?;
    let stored = directory.join(&name);
    config::atomic_write_for_settings(&stored, &bytes)
        .with_context(|| format!("store provider icon {}", stored.display()))?;
    Ok(format!("file:{name}"))
}

fn icon_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
        Some("webp")
    } else if bytes.starts_with(b"\0\0\x01\0") {
        Some("ico")
    } else {
        let head = &bytes[..bytes.len().min(1024)];
        if head
            .windows(4)
            .any(|window| window.eq_ignore_ascii_case(b"<svg"))
        {
            Some("svg")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::icon_extension;

    #[test]
    fn recognizes_supported_icon_formats_by_content() {
        assert_eq!(icon_extension(b"\x89PNG\r\n\x1a\nimage"), Some("png"));
        assert_eq!(icon_extension(b"\xff\xd8\xffimage"), Some("jpg"));
        assert_eq!(icon_extension(b"GIF89aimage"), Some("gif"));
        assert_eq!(icon_extension(b"RIFF1234WEBPimage"), Some("webp"));
        assert_eq!(icon_extension(b"\0\0\x01\0image"), Some("ico"));
        assert_eq!(icon_extension(b"<?xml><SVG></SVG>"), Some("svg"));
        assert_eq!(icon_extension(b"not an image"), None);
    }
}
