use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Consumed by the wasm-only Queue adapter.
pub struct R2Event {
    pub(crate) account: String,
    pub(crate) action: String,
    pub(crate) bucket: String,
    pub(crate) object: R2EventObject,
    pub(crate) event_time: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Consumed by the wasm-only Queue adapter.
pub(crate) struct R2EventObject {
    pub(crate) key: String,
    pub(crate) size: Option<u64>,
    #[serde(rename = "eTag")]
    pub(crate) e_tag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cloudflare_r2_notification_shape() {
        let event: R2Event = serde_json::from_str(
            r#"{
                "account":"3f4b7e3dcab231cbfdaa90a6a28bd548",
                "action":"CopyObject",
                "bucket":"rolodex-recordings",
                "object":{
                    "key":"recordings/camera-12/clip.mp4",
                    "size":65536,
                    "eTag":"c846ff7a18f28c2e262116d6e8719ef0"
                },
                "eventTime":"2026-08-09T19:36:44.379Z",
                "copySource":{"bucket":"source","object":"original.mp4"}
            }"#,
        )
        .unwrap();
        assert_eq!(event.action, "CopyObject");
        assert_eq!(event.object.key, "recordings/camera-12/clip.mp4");
        assert_eq!(event.object.size, Some(65_536));
        assert_eq!(
            event.object.e_tag.as_deref(),
            Some("c846ff7a18f28c2e262116d6e8719ef0")
        );
    }
}
