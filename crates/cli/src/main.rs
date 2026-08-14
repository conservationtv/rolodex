use std::{
    env,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
};

use async_trait::async_trait;
use rolodex_core::{
    ByteSource, Error, ProcessorOptions, Result, SourceIdentity, parse_duration_ns,
    process_recording,
};

struct FileSource {
    file: Mutex<File>,
    length: u64,
}

impl FileSource {
    fn open(path: &Path) -> std::result::Result<Self, String> {
        let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        let length = file
            .metadata()
            .map_err(|error| format!("stat {}: {error}", path.display()))?
            .len();
        Ok(Self {
            file: Mutex::new(file),
            length,
        })
    }
}

#[async_trait(?Send)]
impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.length
    }

    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| Error::Source("local file lock poisoned".into()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| Error::Source(format!("seek to {offset}: {error}")))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|error| Error::Source(format!("read {length} bytes at {offset}: {error}")))?;
        Ok(bytes)
    }
}

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output_directory: PathBuf,
    options: ProcessorOptions,
}

fn usage() -> &'static str {
    "usage: rolodex [--max-width N] [--max-height N] [--quality N] \
     [--thumbnail-interval DURATION] INPUT.mp4 OUTPUT_DIRECTORY"
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut values = env::args().skip(1);
    let mut options = ProcessorOptions::default();
    let mut positional = Vec::new();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--max-width" => options.max_width = parse_number(&mut values, "--max-width")?,
            "--max-height" => options.max_height = parse_number(&mut values, "--max-height")?,
            "--quality" => options.jpeg_quality = parse_number(&mut values, "--quality")?,
            "--thumbnail-interval" => {
                let value = values
                    .next()
                    .ok_or_else(|| "--thumbnail-interval requires a value".to_string())?;
                options.thumbnail_interval_ns = Some(
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

fn parse_number<T: std::str::FromStr>(
    values: &mut impl Iterator<Item = String>,
    option: &str,
) -> std::result::Result<T, String> {
    values
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?
        .parse()
        .map_err(|_| format!("invalid value for {option}"))
}

fn run() -> std::result::Result<(), String> {
    let args = parse_args()?;
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

fn main() {
    if let Err(error) = run() {
        eprintln!("rolodex: {error}");
        std::process::exit(1);
    }
}
