#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactKeys {
    pub thumbnail: String,
    pub index: String,
    repair: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveKeys {
    pub stream: String,
    pub root: String,
    pub day: String,
}

#[allow(dead_code)] // Used by the Worker target; host builds retain only its tests.
pub fn derive_artifact_keys(
    source_key: &str,
    source_prefix: &str,
    artifact_prefix: &str,
    init_prefix: &str,
) -> Option<ArtifactKeys> {
    if source_key.starts_with(artifact_prefix) || source_key.starts_with(init_prefix) {
        return None;
    }
    let relative = source_key.strip_prefix(source_prefix)?;
    let stem = relative
        .strip_suffix(".mp4")
        .or_else(|| relative.strip_suffix(".MP4"))?;
    if stem.is_empty() || stem.ends_with('/') {
        return None;
    }
    let (stream, recording_name) = stem.rsplit_once('/')?;
    if stream.is_empty() || recording_name.is_empty() {
        return None;
    }
    let recording_root = format!("{artifact_prefix}media/{stem}/");
    let index_root = format!("{artifact_prefix}{stream}/indexes/");
    Some(ArtifactKeys {
        thumbnail: format!("{recording_root}thumbnail.jpg"),
        index: format!("{index_root}{recording_name}.json"),
        repair: format!("repairs/{stem}/"),
    })
}

#[allow(dead_code)] // Used by the Worker target; host builds retain only its tests.
pub fn init_key(init_prefix: &str, sha256: &str) -> String {
    format!("{init_prefix}{sha256}.mp4")
}

#[allow(dead_code)] // Used by the Worker target; host builds retain only its tests.
pub fn derive_archive_keys(
    source_key: &str,
    source_prefix: &str,
    artifact_prefix: &str,
    date: &str,
) -> Option<ArchiveKeys> {
    let relative = source_key.strip_prefix(source_prefix)?;
    let stem = relative
        .strip_suffix(".mp4")
        .or_else(|| relative.strip_suffix(".MP4"))?;
    if stem.is_empty() || stem.ends_with('/') {
        return None;
    }
    let (stream, recording_name) = stem.rsplit_once('/')?;
    if stream.is_empty() || recording_name.is_empty() {
        return None;
    }
    let artifact_stream = format!("{artifact_prefix}{stream}")
        .trim_end_matches('/')
        .to_string();
    if artifact_stream.is_empty() {
        return None;
    }
    let logical_stream = artifact_stream
        .split_once('/')
        .map_or(artifact_stream.as_str(), |(_, stream)| stream)
        .to_string();
    Some(ArchiveKeys {
        stream: logical_stream,
        root: format!("{artifact_stream}/archive.json"),
        day: format!("{artifact_stream}/archive/{date}/head.json"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_recording_to_stream_index_and_grouped_thumbnail() {
        assert_eq!(
            derive_artifact_keys(
                "recordings/camera-12/2026-08-09_184501.mp4",
                "recordings/",
                "artifacts/",
                "init/",
            ),
            Some(ArtifactKeys {
                thumbnail: "artifacts/media/camera-12/2026-08-09_184501/thumbnail.jpg".into(),
                index: "artifacts/camera-12/indexes/2026-08-09_184501.json".into(),
                repair: "repairs/camera-12/2026-08-09_184501/".into(),
            })
        );
    }

    #[test]
    fn timestamped_index_keys_sort_chronologically_within_stream_prefix() {
        let older = derive_artifact_keys(
            "recordings/h264/demo/2026-08-09_18-45-01-000001.mp4",
            "recordings/h264/",
            "artifacts/h264/",
            "init/",
        )
        .unwrap();
        let newer = derive_artifact_keys(
            "recordings/h264/demo/2026-08-10_00-00-02-000001.mp4",
            "recordings/h264/",
            "artifacts/h264/",
            "init/",
        )
        .unwrap();
        assert!(older.index < newer.index);
        assert_eq!(
            older.repair_prefix(),
            "repairs/demo/2026-08-09_18-45-01-000001/"
        );
        let uppercase = derive_artifact_keys(
            "recordings/demo/clip.MP4",
            "recordings/",
            "artifacts/",
            "init/",
        )
        .unwrap();
        assert_eq!(uppercase.repair_prefix(), "repairs/demo/clip/");
        assert_eq!(
            older.index,
            "artifacts/h264/demo/indexes/2026-08-09_18-45-01-000001.json"
        );
        assert_eq!(
            newer.index,
            "artifacts/h264/demo/indexes/2026-08-10_00-00-02-000001.json"
        );
    }

    #[test]
    fn preserves_nested_stream_path_and_rejects_root_clips() {
        assert_eq!(
            derive_artifact_keys(
                "recordings/site-a/camera-12/2026-08-09_184501.mp4",
                "recordings/",
                "artifacts/",
                "init/",
            )
            .unwrap()
            .index,
            "artifacts/site-a/camera-12/indexes/2026-08-09_184501.json"
        );
        assert_eq!(
            derive_artifact_keys(
                "recordings/2026-08-09_184501.mp4",
                "recordings/",
                "artifacts/",
                "init/",
            ),
            None
        );
        assert_eq!(
            derive_archive_keys(
                "recordings/2026-08-09_184501.mp4",
                "recordings/",
                "artifacts/",
                "2026-08-09",
            ),
            None
        );
    }

    #[test]
    fn ignores_non_mp4_and_generated_paths() {
        assert_eq!(
            derive_artifact_keys(
                "recordings/camera/a.txt",
                "recordings/",
                "artifacts/",
                "init/"
            ),
            None
        );
        assert_eq!(
            derive_artifact_keys("artifacts/a.mp4", "", "artifacts/", "init/"),
            None
        );
        assert_eq!(
            derive_artifact_keys("init/a.mp4", "", "artifacts/", "init/"),
            None
        );
    }

    #[test]
    fn content_addresses_init_object() {
        assert_eq!(init_key("init/", "abc123"), "init/abc123.mp4");
    }

    #[test]
    fn maps_recording_to_stream_and_daily_archive_manifests() {
        let keys = derive_archive_keys(
            "recordings/h265/cam/clip.mp4",
            "recordings/h265/",
            "artifacts/h265/",
            "2026-08-13",
        )
        .unwrap();
        assert_eq!(keys.day_prefix(), "artifacts/h265/cam/archive/2026-08-13");
        assert_eq!(keys.archive_prefix(), "artifacts/h265/cam/archive/");
        assert_eq!(
            derive_archive_keys(
                "recordings/h265/cam/2026-08-13_12-00-00.mp4",
                "recordings/h265/",
                "artifacts/h265/",
                "2026-08-13",
            ),
            Some(ArchiveKeys {
                stream: "h265/cam".into(),
                root: "artifacts/h265/cam/archive.json".into(),
                day: "artifacts/h265/cam/archive/2026-08-13/head.json".into(),
            })
        );
    }
}

#[allow(dead_code)]
impl ArtifactKeys {
    pub fn repair_prefix(&self) -> String {
        self.repair.clone()
    }
}

#[allow(dead_code)]
impl ArchiveKeys {
    pub fn day_prefix(&self) -> &str {
        self.day
            .strip_suffix("/head.json")
            .expect("derived daily head key")
    }
    pub fn archive_prefix(&self) -> String {
        format!(
            "{}archive/",
            self.root
                .strip_suffix("archive.json")
                .expect("derived archive key")
        )
    }
}
