#![cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]

use std::collections::HashMap;

use rolodex_core::{ProcessorOptions, processor::PROCESSOR_VERSION};

const SOURCE_ETAG: &str = "media-processor-source-etag";
const SOURCE_VERSION: &str = "media-processor-source-version";
const VERSION: &str = "media-processor-version";
const INIT_SHA256: &str = "media-processor-init-sha256";
const INIT_KEY: &str = "media-processor-init-key";
const THUMBNAIL_KEY: &str = "media-processor-thumbnail-key";
const THUMBNAIL_STATUS: &str = "media-processor-thumbnail-status";
const THUMBNAIL_AVAILABLE: &str = "available";
const THUMBNAIL_UNAVAILABLE: &str = "unavailable";
const MAX_WIDTH: &str = "media-processor-max-width";
const MAX_HEIGHT: &str = "media-processor-max-height";
const JPEG_QUALITY: &str = "media-processor-jpeg-quality";
const THUMBNAIL_INTERVAL: &str = "media-processor-thumbnail-interval-ns";
const MAX_SPRITE_PIXELS: &str = "media-processor-max-sprite-pixels";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtifactIdentity {
    source_etag: String,
    source_version: String,
    init_sha256: String,
    init_key: String,
    thumbnail_key: Option<String>,
    max_width: u32,
    max_height: u32,
    jpeg_quality: u8,
    thumbnail_interval: String,
    max_sprite_pixels: u64,
}

impl ArtifactIdentity {
    pub(crate) fn new(
        source_etag: String,
        source_version: String,
        init_sha256: String,
        init_key: String,
        thumbnail_key: Option<String>,
        options: ProcessorOptions,
    ) -> Self {
        Self {
            source_etag,
            source_version,
            init_sha256,
            init_key,
            thumbnail_key,
            max_width: options.max_width,
            max_height: options.max_height,
            jpeg_quality: options.jpeg_quality,
            thumbnail_interval: interval_identity(options),
            max_sprite_pixels: options.max_sprite_pixels,
        }
    }

    pub(crate) fn source_matches(
        source_etag: &str,
        options: ProcessorOptions,
        metadata: &HashMap<String, String>,
    ) -> bool {
        metadata
            .get(SOURCE_ETAG)
            .is_some_and(|value| value == source_etag)
            && metadata
                .get(VERSION)
                .is_some_and(|value| value == PROCESSOR_VERSION)
            && metadata.contains_key(INIT_KEY)
            && valid_thumbnail_identity(metadata)
            && metadata.get(MAX_WIDTH) == Some(&options.max_width.to_string())
            && metadata.get(MAX_HEIGHT) == Some(&options.max_height.to_string())
            && metadata.get(JPEG_QUALITY) == Some(&options.jpeg_quality.to_string())
            && metadata.get(THUMBNAIL_INTERVAL) == Some(&interval_identity(options))
            && metadata.get(MAX_SPRITE_PIXELS) == Some(&options.max_sprite_pixels.to_string())
    }

    pub(crate) fn artifact_keys(
        metadata: &HashMap<String, String>,
    ) -> Option<(&str, Option<&str>)> {
        let thumbnail = match metadata.get(THUMBNAIL_STATUS)?.as_str() {
            THUMBNAIL_AVAILABLE => Some(metadata.get(THUMBNAIL_KEY)?.as_str()),
            THUMBNAIL_UNAVAILABLE if !metadata.contains_key(THUMBNAIL_KEY) => None,
            _ => return None,
        };
        Some((metadata.get(INIT_KEY)?.as_str(), thumbnail))
    }

    pub(crate) fn metadata(&self) -> HashMap<String, String> {
        let mut metadata = HashMap::from([
            (SOURCE_ETAG.into(), self.source_etag.clone()),
            (SOURCE_VERSION.into(), self.source_version.clone()),
            (VERSION.into(), PROCESSOR_VERSION.into()),
            (INIT_SHA256.into(), self.init_sha256.clone()),
            (INIT_KEY.into(), self.init_key.clone()),
            (MAX_WIDTH.into(), self.max_width.to_string()),
            (MAX_HEIGHT.into(), self.max_height.to_string()),
            (JPEG_QUALITY.into(), self.jpeg_quality.to_string()),
            (THUMBNAIL_INTERVAL.into(), self.thumbnail_interval.clone()),
            (MAX_SPRITE_PIXELS.into(), self.max_sprite_pixels.to_string()),
        ]);
        if let Some(thumbnail_key) = &self.thumbnail_key {
            metadata.insert(THUMBNAIL_STATUS.into(), THUMBNAIL_AVAILABLE.into());
            metadata.insert(THUMBNAIL_KEY.into(), thumbnail_key.clone());
        } else {
            metadata.insert(THUMBNAIL_STATUS.into(), THUMBNAIL_UNAVAILABLE.into());
        }
        metadata
    }
}

fn valid_thumbnail_identity(metadata: &HashMap<String, String>) -> bool {
    match metadata.get(THUMBNAIL_STATUS).map(String::as_str) {
        Some(THUMBNAIL_AVAILABLE) => metadata.contains_key(THUMBNAIL_KEY),
        Some(THUMBNAIL_UNAVAILABLE) => !metadata.contains_key(THUMBNAIL_KEY),
        _ => false,
    }
}

fn interval_identity(options: ProcessorOptions) -> String {
    options
        .thumbnail_interval_ns
        .map_or_else(|| "single".into(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ArtifactIdentity {
        ArtifactIdentity::new(
            "source-etag".into(),
            "source-version".into(),
            "abc123".into(),
            "init/abc123.mp4".into(),
            Some("artifacts/camera/clip/thumbnail.jpg".into()),
            ProcessorOptions::default(),
        )
    }

    #[test]
    fn exact_commit_metadata_is_idempotent() {
        let metadata = identity().metadata();
        assert!(ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
        assert_eq!(
            ArtifactIdentity::artifact_keys(&metadata),
            Some((
                "init/abc123.mp4",
                Some("artifacts/camera/clip/thumbnail.jpg")
            ))
        );
    }

    #[test]
    fn unavailable_thumbnail_is_an_idempotent_commit() {
        let identity = ArtifactIdentity::new(
            "source-etag".into(),
            "source-version".into(),
            "abc123".into(),
            "init/abc123.mp4".into(),
            None,
            ProcessorOptions::default(),
        );
        let metadata = identity.metadata();

        assert!(ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
        assert_eq!(
            ArtifactIdentity::artifact_keys(&metadata),
            Some(("init/abc123.mp4", None))
        );
        assert!(!metadata.contains_key(THUMBNAIL_KEY));
    }

    #[test]
    fn source_or_processor_change_requires_regeneration() {
        let mut metadata = identity().metadata();
        assert!(!ArtifactIdentity::source_matches(
            "different",
            ProcessorOptions::default(),
            &metadata
        ));
        metadata.insert(VERSION.into(), "different".into());
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
        let metadata = identity().metadata();
        let mut options = ProcessorOptions::default();
        options.max_width += 2;
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            options,
            &metadata
        ));
        let options = ProcessorOptions {
            thumbnail_interval_ns: Some(10_000_000_000),
            ..ProcessorOptions::default()
        };
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            options,
            &metadata
        ));
    }

    #[test]
    fn legacy_or_partial_commit_requires_regeneration() {
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &HashMap::new()
        ));
        let mut metadata = identity().metadata();
        metadata.remove(INIT_KEY);
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
        let mut metadata = identity().metadata();
        metadata.remove(THUMBNAIL_STATUS);
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
        let mut metadata = identity().metadata();
        metadata.insert(THUMBNAIL_STATUS.into(), THUMBNAIL_UNAVAILABLE.into());
        assert!(!ArtifactIdentity::source_matches(
            "source-etag",
            ProcessorOptions::default(),
            &metadata
        ));
    }
}
