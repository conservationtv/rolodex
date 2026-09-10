use crate::archive_store::{
    ArchiveError, MAX_DAILY_ARCHIVE_BYTES, load_json, update_archive_manifests,
};
use rolodex_core::{Error as CoreError, ProcessorOptions, SourceIdentity, parse_duration_ns};
use serde_json::{Value, json};
use worker::{
    Context, Env, HttpMetadata, MessageBatch, MessageExt, Result as WorkerResult, console_log,
};

use crate::{
    event::R2Event,
    identity::ArtifactIdentity,
    key::{ArchiveKeys, ArtifactKeys, derive_artifact_keys, init_key},
    r2_source::R2Source,
};

const BUCKET_BINDING: &str = "MEDIA_BUCKET";
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const MANIFEST_CACHE_CONTROL: &str = "public, max-age=5, must-revalidate";

#[derive(Debug)]
struct Config {
    source_prefix: String,
    artifact_prefix: String,
    init_prefix: String,
    source_bucket: Option<String>,
    retention_seconds: u64,
    options: ProcessorOptions,
}

impl Config {
    fn from_env(env: &Env) -> std::result::Result<Self, String> {
        let mut options = ProcessorOptions::default();
        let thumbnail = options
            .thumbnail
            .as_mut()
            .expect("default thumbnail options");
        thumbnail.max_width = optional_number(env, "MAX_WIDTH", thumbnail.max_width)?;
        thumbnail.max_height = optional_number(env, "MAX_HEIGHT", thumbnail.max_height)?;
        thumbnail.quality = optional_number(env, "JPEG_QUALITY", thumbnail.quality)?;
        thumbnail.interval_ns = optional_string(env, "THUMBNAIL_INTERVAL")
            .map(|value| {
                parse_duration_ns(&value).map(Some).map_err(|error| {
                    format!("environment variable THUMBNAIL_INTERVAL is invalid: {error}")
                })
            })
            .unwrap_or(Ok(thumbnail.interval_ns))?;
        options.max_thumbnail_frames =
            optional_number(env, "MAX_THUMBNAIL_FRAMES", options.max_thumbnail_frames)?;
        options.max_sprite_pixels =
            optional_number(env, "MAX_SPRITE_PIXELS", options.max_sprite_pixels)?;
        options.fragment_read_ahead_bytes = optional_number(
            env,
            "FRAGMENT_READ_AHEAD_BYTES",
            options.fragment_read_ahead_bytes,
        )?;
        options.max_samples_indexed =
            optional_number(env, "MAX_SAMPLES_INDEXED", options.max_samples_indexed)?;
        options.limits.initial_read_bytes =
            optional_number(env, "INITIAL_READ_BYTES", options.limits.initial_read_bytes)?;
        options.limits.full_read_threshold_bytes = optional_number(
            env,
            "FULL_READ_THRESHOLD_BYTES",
            options.limits.full_read_threshold_bytes,
        )?;
        options.limits.max_total_bytes_read = optional_number(
            env,
            "MAX_R2_BYTES_PER_JOB",
            options.limits.max_total_bytes_read,
        )?;
        options.limits.max_samples_decoded = optional_number(
            env,
            "MAX_SAMPLES_DECODED",
            options.limits.max_samples_decoded,
        )?;
        options.limits.max_samples_examined = optional_number(
            env,
            "MAX_SAMPLES_EXAMINED",
            options.limits.max_samples_examined,
        )?;
        options.limits.max_fragments =
            optional_number(env, "MAX_PARTS_PER_RECORDING", options.limits.max_fragments)?;
        options.limits.max_top_level_boxes = optional_number(
            env,
            "MAX_TOP_LEVEL_BOXES",
            options.limits.max_top_level_boxes,
        )?;
        options = options
            .validate()
            .map_err(|error| format!("invalid Worker processor configuration: {error}"))?;

        let source_prefix =
            optional_string(env, "SOURCE_PREFIX").unwrap_or_else(|| "recordings/".into());
        let artifact_prefix =
            optional_string(env, "ARTIFACT_PREFIX").unwrap_or_else(|| "artifacts/".into());
        let init_prefix = optional_string(env, "INIT_PREFIX").unwrap_or_else(|| "init/".into());
        if artifact_prefix.is_empty()
            || init_prefix.is_empty()
            || source_prefix == artifact_prefix
            || source_prefix == init_prefix
            || artifact_prefix == init_prefix
        {
            return Err("source, artifact, and init prefixes must be distinct".into());
        }
        Ok(Self {
            source_prefix,
            artifact_prefix,
            init_prefix,
            source_bucket: optional_string(env, "SOURCE_BUCKET"),
            retention_seconds: optional_number(env, "RETENTION_SECONDS", 0)?,
            options,
        })
    }
}

fn optional_string(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
}

fn optional_number<T>(env: &Env, name: &str, default: T) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    match optional_string(env, name) {
        Some(value) => value
            .parse()
            .map_err(|_| format!("environment variable {name} is not a valid number")),
        None => Ok(default),
    }
}

#[derive(Debug)]
enum JobResult {
    Ignored(&'static str),
    AlreadyExists {
        index_key: String,
        archive_key: String,
        daily_archive_key: String,
    },
    Created {
        keys: ArtifactKeys,
        archive_keys: ArchiveKeys,
        init_key: String,
        init_created: bool,
        thumbnail_created: bool,
        replaced_index: bool,
        metrics: rolodex_core::ProcessorMetrics,
    },
}

#[derive(Debug)]
enum JobFailure {
    Permanent(String),
    Retry(String),
}

impl JobFailure {
    fn from_core(error: CoreError) -> Self {
        if error.is_retryable() {
            Self::Retry(error.to_string())
        } else {
            Self::Permanent(error.to_string())
        }
    }
}

#[worker::event(queue)]
pub async fn queue(batch: MessageBatch<R2Event>, env: Env, _ctx: Context) -> WorkerResult<()> {
    let messages = batch.messages()?;
    let config = Config::from_env(&env);
    for message in messages {
        let message_id = message.id();
        let result = match &config {
            Ok(config) => process(message.body(), &env, config).await,
            Err(error) => Err(JobFailure::Retry(error.clone())),
        };
        match result {
            Ok(result) => {
                log_success(&message_id, message.body(), result);
                message.ack();
            }
            Err(failure) => {
                let (outcome, error, should_retry) = match failure {
                    JobFailure::Permanent(error) => ("permanent_failure", error, false),
                    JobFailure::Retry(error) => ("retry", error, true),
                };
                log_outcome(
                    &message_id,
                    message.body(),
                    "error",
                    outcome,
                    json!({ "error": error }),
                );
                if should_retry {
                    message.retry();
                } else {
                    message.ack();
                }
            }
        }
    }
    Ok(())
}

async fn process(
    event: &R2Event,
    env: &Env,
    config: &Config,
) -> std::result::Result<JobResult, JobFailure> {
    if !matches!(
        event.action.as_str(),
        "PutObject" | "CopyObject" | "CompleteMultipartUpload"
    ) {
        return Ok(JobResult::Ignored("not_an_object_create_action"));
    }
    if config
        .source_bucket
        .as_ref()
        .is_some_and(|expected| expected != &event.bucket)
    {
        return Ok(JobResult::Ignored("source_bucket_mismatch"));
    }
    let Some(keys) = derive_artifact_keys(
        &event.object.key,
        &config.source_prefix,
        &config.artifact_prefix,
        &config.init_prefix,
    ) else {
        return Ok(JobResult::Ignored("key_not_in_scope"));
    };

    let bucket = env
        .bucket(BUCKET_BINDING)
        .map_err(|error| JobFailure::Retry(format!("missing {BUCKET_BINDING} binding: {error}")))?;
    let source_object = bucket
        .head(&event.object.key)
        .await
        .map_err(|error| JobFailure::Retry(format!("R2 source HEAD failed: {error}")))?
        .ok_or_else(|| JobFailure::Permanent("source object no longer exists".into()))?;
    let source_etag = source_object.etag();
    let source_version = source_object.version();

    let existing_index = bucket
        .head(&keys.index)
        .await
        .map_err(|error| JobFailure::Retry(format!("R2 index HEAD failed: {error}")))?;
    let replaced_index = existing_index.is_some();
    if let Some(existing) = existing_index.as_ref() {
        let metadata = existing
            .custom_metadata()
            .map_err(|error| JobFailure::Retry(format!("R2 index metadata failed: {error}")))?;
        if ArtifactIdentity::source_matches(&source_etag, config.options, &metadata) {
            let (committed_init, committed_thumbnail) = ArtifactIdentity::artifact_keys(&metadata)
                .ok_or_else(|| {
                    JobFailure::Retry("committed index is missing artifact keys".into())
                })?;
            let committed_init = committed_init.to_string();
            let committed_thumbnail = committed_thumbnail.map(ToString::to_string);
            let thumbnail_key_matches = committed_thumbnail
                .as_deref()
                .map_or(true, |thumbnail| thumbnail == keys.thumbnail.as_str());
            if thumbnail_key_matches && committed_init.starts_with(&config.init_prefix) {
                let thumbnail_exists = if let Some(thumbnail) = committed_thumbnail.as_deref() {
                    bucket
                        .head(thumbnail)
                        .await
                        .map_err(|error| {
                            JobFailure::Retry(format!("R2 thumbnail HEAD failed: {error}"))
                        })?
                        .is_some()
                } else {
                    true
                };
                let init_exists = bucket
                    .head(&committed_init)
                    .await
                    .map_err(|error| JobFailure::Retry(format!("R2 init HEAD failed: {error}")))?
                    .is_some();
                if thumbnail_exists && init_exists {
                    let metadata: rolodex_core::RecordingMetadata =
                        load_json(&bucket, &keys.index, MAX_DAILY_ARCHIVE_BYTES)
                            .await?
                            .ok_or_else(|| {
                                JobFailure::Retry(
                                    "committed index disappeared before manifest update".into(),
                                )
                            })?
                            .value;
                    let archive_keys = update_archive_manifests(
                        &bucket,
                        &config.source_prefix,
                        &config.artifact_prefix,
                        config.retention_seconds,
                        &metadata,
                        committed_init,
                        committed_thumbnail,
                    )
                    .await?;
                    return Ok(JobResult::AlreadyExists {
                        index_key: keys.index,
                        archive_key: archive_keys.root,
                        daily_archive_key: archive_keys.day,
                    });
                }
            }
        }
    }

    let source = R2Source::new(
        bucket.clone(),
        event.object.key.clone(),
        source_object.size(),
        source_etag.clone(),
    );
    let sink = crate::repair_sink::R2RepairSink::new(bucket.clone(), keys.repair_prefix());
    let output = rolodex_core::processor::process_recording_with_repairs(
        &source,
        SourceIdentity {
            object_key: event.object.key.clone(),
            etag: source_etag.clone(),
        },
        config.options,
        &sink,
    )
    .await
    .map_err(JobFailure::from_core)?;
    let init_key = init_key(&config.init_prefix, &output.metadata.init.sha256);
    let identity = ArtifactIdentity::new(
        source_etag,
        source_version,
        output.metadata.init.sha256.clone(),
        init_key.clone(),
        output
            .thumbnail_jpeg
            .as_ref()
            .map(|_| keys.thumbnail.clone()),
        config.options,
    );
    let artifact_metadata = identity.metadata();

    let existing_init = bucket
        .head(&init_key)
        .await
        .map_err(|error| JobFailure::Retry(format!("R2 init HEAD failed: {error}")))?;
    let init_created = existing_init.is_none();
    if let Some(existing) = existing_init {
        if existing.size() != output.init_mp4.len() as u64 {
            return Err(JobFailure::Permanent(
                "content-addressed init key exists with a different size".into(),
            ));
        }
    } else {
        bucket
            .put(&init_key, output.init_mp4)
            .http_metadata(HttpMetadata {
                content_type: Some("video/mp4".into()),
                cache_control: Some(IMMUTABLE_CACHE_CONTROL.into()),
                ..HttpMetadata::default()
            })
            .execute()
            .await
            .map_err(|error| JobFailure::Retry(format!("R2 init PUT failed: {error}")))?;
    }

    let thumbnail_created = output.thumbnail_jpeg.is_some();
    if let Some(thumbnail_jpeg) = output.thumbnail_jpeg {
        bucket
            .put(&keys.thumbnail, thumbnail_jpeg)
            .http_metadata(HttpMetadata {
                content_type: Some("image/jpeg".into()),
                cache_control: Some(IMMUTABLE_CACHE_CONTROL.into()),
                ..HttpMetadata::default()
            })
            .custom_metadata(artifact_metadata.clone())
            .execute()
            .await
            .map_err(|error| JobFailure::Retry(format!("R2 thumbnail PUT failed: {error}")))?;
    }

    let metadata_json = serde_json::to_vec(&output.metadata)
        .map_err(|error| JobFailure::Permanent(format!("index serialization failed: {error}")))?;
    bucket
        .put(&keys.index, metadata_json)
        .http_metadata(HttpMetadata {
            content_type: Some("application/json".into()),
            cache_control: Some(MANIFEST_CACHE_CONTROL.into()),
            ..HttpMetadata::default()
        })
        .custom_metadata(artifact_metadata)
        .execute()
        .await
        .map_err(|error| JobFailure::Retry(format!("R2 index PUT failed: {error}")))?;

    let archive_keys = update_archive_manifests(
        &bucket,
        &config.source_prefix,
        &config.artifact_prefix,
        config.retention_seconds,
        &output.metadata,
        init_key.clone(),
        thumbnail_created.then(|| keys.thumbnail.clone()),
    )
    .await?;

    Ok(JobResult::Created {
        keys,
        archive_keys,
        init_key,
        init_created,
        thumbnail_created,
        replaced_index,
        metrics: output.metrics,
    })
}

fn log_success(message_id: &str, event: &R2Event, result: JobResult) {
    let (outcome, extra_fields) = match result {
        JobResult::Ignored(reason) => (
            "ignored",
            json!({
                "reason": reason,
            }),
        ),
        JobResult::AlreadyExists {
            index_key,
            archive_key,
            daily_archive_key,
        } => (
            "already_exists",
            json!({
                "index_key": index_key,
                "archive_key": archive_key,
                "daily_archive_key": daily_archive_key,
            }),
        ),
        JobResult::Created {
            keys,
            archive_keys,
            init_key,
            init_created,
            thumbnail_created,
            replaced_index,
            metrics,
        } => (
            "created",
            json!({
                "thumbnail_key": thumbnail_created.then_some(keys.thumbnail),
                "index_key": keys.index,
                "archive_key": archive_keys.root,
                "daily_archive_key": archive_keys.day,
                "init_key": init_key,
                "init_created": init_created,
                "thumbnail_created": thumbnail_created,
                "replaced_index": replaced_index,
                "metrics": metrics,
            }),
        ),
    };
    log_outcome(message_id, event, "info", outcome, extra_fields);
}

fn log_outcome(message_id: &str, event: &R2Event, level: &str, outcome: &str, extra_fields: Value) {
    let mut value = json!({
        "level": level,
        "outcome": outcome,
        "message_id": message_id,
        "account": event.account,
        "bucket": event.bucket,
        "source_key": event.object.key,
        "event_time": event.event_time,
        "event_size": event.object.size,
        "etag": event.object.e_tag,
        "build_codec": rolodex_core::BUILD_CODEC,
    });
    let fields = value
        .as_object_mut()
        .expect("base log fields are a JSON object");
    if let Value::Object(extra_fields) = extra_fields {
        fields.extend(extra_fields);
    }
    console_log!("{}", value);
}

impl From<ArchiveError> for JobFailure {
    fn from(error: ArchiveError) -> Self {
        match error {
            ArchiveError::Permanent(message) => Self::Permanent(message),
            ArchiveError::Retry(message) => Self::Retry(message),
        }
    }
}
