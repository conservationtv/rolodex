use std::{fs, path::PathBuf};

use rolodex_core::{
    ProcessorOptions, SourceIdentity, parse_duration_ns, process_recording, source::FileSource,
};

use crate::{next_value, parse_number};

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output_directory: PathBuf,
    options: ProcessorOptions,
}

fn usage() -> &'static str {
    "usage: rolodex index [--max-width N] [--max-height N] [--quality N] \
     [--thumbnail-interval DURATION] INPUT.mp4 OUTPUT_DIRECTORY"
}

fn parse_args(values: impl IntoIterator<Item = String>) -> std::result::Result<Args, String> {
    let mut values = values.into_iter();
    let mut options = ProcessorOptions::default();
    let thumbnail = options.thumbnail.as_mut().expect("default thumbnail");
    let mut positional = Vec::new();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--max-width" => thumbnail.max_width = parse_number(&mut values, "--max-width")?,
            "--max-height" => thumbnail.max_height = parse_number(&mut values, "--max-height")?,
            "--quality" => thumbnail.quality = parse_number(&mut values, "--quality")?,
            "--thumbnail-interval" => {
                let value = next_value(&mut values, "--thumbnail-interval")?;
                thumbnail.interval_ns = Some(
                    parse_duration_ns(&value)
                        .map_err(|error| format!("invalid --thumbnail-interval: {error}"))?,
                );
            }
            "-h" | "--help" => return Err(usage().into()),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option {flag}\n{}", usage()));
            }
            _ => positional.push(PathBuf::from(value)),
        }
    }
    if positional.len() != 2 {
        return Err(usage().into());
    }
    Ok(Args {
        input: positional.remove(0),
        output_directory: positional.remove(0),
        options,
    })
}

fn run(args: Args) -> std::result::Result<(), String> {
    let source = FileSource::open(&args.input)?;
    let output = futures::executor::block_on(process_recording(
        &source,
        SourceIdentity {
            object_key: args.input.to_string_lossy().into_owned(),
            etag: "local-file".into(),
        },
        args.options,
    ))
    .map_err(|error| error.to_string())?;

    let init_directory = args.output_directory.join("init");
    fs::create_dir_all(&init_directory)
        .map_err(|error| format!("create {}: {error}", init_directory.display()))?;
    let thumbnail_path = output
        .thumbnail_jpeg
        .as_ref()
        .map(|_| args.output_directory.join("thumbnail.jpg"));
    let metadata_path = args.output_directory.join("index.json");
    let init_path = init_directory.join(format!("{}.mp4", output.metadata.init.sha256));
    if let (Some(thumbnail_path), Some(thumbnail_jpeg)) = (&thumbnail_path, &output.thumbnail_jpeg)
    {
        fs::write(thumbnail_path, thumbnail_jpeg)
            .map_err(|error| format!("write {}: {error}", thumbnail_path.display()))?;
    }
    let metadata = serde_json::to_vec_pretty(&output.metadata)
        .map_err(|error| format!("serialize index: {error}"))?;
    fs::write(&metadata_path, metadata)
        .map_err(|error| format!("write {}: {error}", metadata_path.display()))?;
    fs::write(&init_path, &output.init_mp4)
        .map_err(|error| format!("write {}: {error}", init_path.display()))?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "thumbnail": thumbnail_path,
            "index": metadata_path,
            "init": init_path,
            "metrics": output.metrics,
        }))
        .map_err(|error| format!("serialize metrics: {error}"))?
    );
    Ok(())
}

pub(crate) fn run_cli(values: impl IntoIterator<Item = String>) -> Result<(), String> {
    parse_args(values).and_then(run)
}
