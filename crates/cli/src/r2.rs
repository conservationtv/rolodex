use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rolodex_clip::{Artifact, ArtifactStore, Error, Result, UploadOutcome};
use rolodex_core::hex;
use sha2::{Digest, Sha256};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

pub struct R2Store {
    endpoint: Url,
    bucket: String,
    access_key: String,
    secret_key: String,
    agent: ureq::Agent,
}

impl R2Store {
    pub fn new(
        endpoint: &str,
        bucket: String,
        access_key: String,
        secret_key: String,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint)
            .map_err(|error| Error::Store(format!("invalid R2 endpoint: {error}")))?;
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err(Error::Store(
                "R2 endpoint must not contain a query or fragment".into(),
            ));
        }
        if bucket.is_empty() || bucket.contains('/') {
            return Err(Error::Store("invalid R2 bucket name".into()));
        }
        Ok(Self {
            endpoint,
            bucket,
            access_key,
            secret_key,
            agent: ureq::AgentBuilder::new().build(),
        })
    }

    fn request(
        &self,
        method: &str,
        key: &str,
        payload_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<ureq::Request> {
        let path = format!(
            "{}/{}/{}",
            self.endpoint.path().trim_end_matches('/'),
            self.bucket,
            key
        );
        let canonical_uri = aws_uri_encode(&path);
        let host = match self.endpoint.port() {
            Some(port) => format!("{}:{port}", self.endpoint.host_str().unwrap_or_default()),
            None => self.endpoint.host_str().unwrap_or_default().to_string(),
        };
        if host.is_empty() {
            return Err(Error::Store("R2 endpoint has no host".into()));
        }
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date}/auto/s3/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex(&Sha256::digest(canonical_request.as_bytes()))
        );
        let date_key = sign(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        )?;
        let region_key = sign(&date_key, b"auto")?;
        let service_key = sign(&region_key, b"s3")?;
        let signing_key = sign(&service_key, b"aws4_request")?;
        let signature = hex(&sign(&signing_key, string_to_sign.as_bytes())?);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );
        let request_url = format!("{}://{host}{canonical_uri}", self.endpoint.scheme());
        Ok(self
            .agent
            .request(method, &request_url)
            .set("Host", &host)
            .set("x-amz-content-sha256", payload_hash)
            .set("x-amz-date", &amz_date)
            .set("Authorization", &authorization))
    }
}

#[async_trait(?Send)]
impl ArtifactStore for R2Store {
    async fn upload(&self, name: &str, artifact: &Artifact) -> Result<UploadOutcome> {
        let now = DateTime::<Utc>::from(std::time::SystemTime::now());
        let empty_hash = hex(&Sha256::digest([]));
        match self.request("HEAD", name, &empty_hash, now)?.call() {
            Ok(_) => return Ok(UploadOutcome::AlreadyExists),
            Err(ureq::Error::Status(404, _)) => {}
            Err(error) => return Err(Error::Store(format!("HEAD {name}: {error}"))),
        }

        let payload_hash = hex(&Sha256::digest(&artifact.bytes));
        self.request("PUT", name, &payload_hash, now)?
            .set("Content-Type", artifact.content_type)
            .set("Cache-Control", "public, max-age=31536000, immutable")
            .send_bytes(&artifact.bytes)
            .map_err(|error| Error::Store(format!("PUT {name}: {error}")))?;
        Ok(UploadOutcome::Created)
    }
}

fn sign(key: &[u8], value: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| Error::Store("invalid SigV4 HMAC key".into()))?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn aws_uri_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    use super::*;

    fn server(statuses: Vec<u16>) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(header_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                let reason = if status == 200 { "OK" } else { "Not Found" };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), requests, handle)
    }

    #[test]
    fn encodes_sigv4_paths_without_encoding_separators() {
        assert_eq!(aws_uri_encode("/bucket/a b/c.mp4"), "/bucket/a%20b/c.mp4");
    }

    #[test]
    fn heads_before_putting_an_absent_artifact() {
        let (endpoint, requests, handle) = server(vec![404, 200]);
        let store =
            R2Store::new(&endpoint, "bucket".into(), "access".into(), "secret".into()).unwrap();
        let outcome = futures::executor::block_on(store.upload(
            "clips/camera/clip.mp4",
            &Artifact {
                content_type: "video/mp4",
                bytes: vec![1, 2, 3],
            },
        ))
        .unwrap();
        handle.join().unwrap();
        let requests = requests.lock().unwrap();

        assert_eq!(outcome, UploadOutcome::Created);
        assert!(requests[0].starts_with("HEAD /bucket/clips/camera/clip.mp4 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: aws4-hmac-sha256")
        );
        assert!(requests[1].starts_with("PUT /bucket/clips/camera/clip.mp4 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("cache-control: public, max-age=31536000, immutable")
        );
    }

    #[test]
    fn skips_put_when_artifact_exists() {
        let (endpoint, requests, handle) = server(vec![200]);
        let store =
            R2Store::new(&endpoint, "bucket".into(), "access".into(), "secret".into()).unwrap();
        let outcome = futures::executor::block_on(store.upload(
            "clips/camera/index.m3u8",
            &Artifact {
                content_type: "application/vnd.apple.mpegurl",
                bytes: Vec::new(),
            },
        ))
        .unwrap();
        handle.join().unwrap();

        assert_eq!(outcome, UploadOutcome::AlreadyExists);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
