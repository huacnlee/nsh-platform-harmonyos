//! Managed WebView download translation between ArkWeb callbacks/commands and
//! `nsh_core::webview` (`DownloadOffer`, `DownloadProgress`, `DownloadEnd`, `WebViewReply`).

use std::{fs, io, path::Path};

use nsh_core::{
    EventPayload,
    webview::{
        DownloadEnd, DownloadOffer, DownloadOutcome, DownloadProgress, WebDownloadSource,
        download_end, download_offer, download_progress, reply,
    },
};

pub(crate) fn sanitize_filename(suggested: &str) -> String {
    let base = suggested
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download".to_owned()
    } else {
        cleaned.to_owned()
    }
}

pub(crate) fn parse_request_id(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse::<u64>().ok()
}

/// Validates the core's staging file path before handing it to ArkWeb: it must
/// be an absolute UTF-8 path with no NUL bytes inside an existing directory,
/// and the leaf itself must not exist yet (including as a broken symlink).
pub(crate) fn validate_staging_path(staging: &Path) -> Result<String, String> {
    if !staging.is_absolute() {
        return Err(format!(
            "the staging path `{}` is not absolute",
            staging.display()
        ));
    }
    let Some(path_str) = staging.to_str().filter(|path| !path.contains('\0')) else {
        return Err(format!(
            "the staging path `{}` is not valid UTF-8",
            staging.display()
        ));
    };
    if staging.file_name().is_none_or(|name| name.is_empty()) {
        return Err(format!(
            "the staging path `{}` does not name a file",
            staging.display()
        ));
    }
    let Some(parent) = staging.parent() else {
        return Err(format!(
            "the staging path `{}` has no parent directory",
            staging.display()
        ));
    };
    if !parent.is_dir() {
        return Err(format!(
            "the staging directory `{}` does not exist",
            parent.display()
        ));
    }
    match fs::symlink_metadata(staging) {
        Ok(_) => {
            return Err(format!(
                "the staging file already exists: `{}`",
                staging.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "the staging path `{}` cannot be checked: {error}",
                staging.display()
            ));
        }
    }
    Ok(path_str.to_owned())
}

fn nonneg_u64(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

/// Declared transfer sizes only apply when the engine reports a positive length:
/// Chromium/ArkWeb returns `-1` or `0` when the total length is unknown, and
/// reporting `Some(0)` would fail `nsh-core`'s publish length check.
fn positive_u64(value: f64) -> Option<u64> {
    (value.is_finite() && value > 0.0).then_some(value as u64)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

pub(crate) fn build_offer_payload(
    id: String,
    url: String,
    filename: &str,
    mime_type: Option<String>,
    bytes: f64,
    method: Option<String>,
) -> EventPayload {
    download_offer(&DownloadOffer {
        id,
        url,
        filename: sanitize_filename(filename),
        mime_type: non_empty(mime_type),
        bytes: positive_u64(bytes),
        method: non_empty(method),
        source: WebDownloadSource::Transfer,
    })
}

pub(crate) fn build_progress_payload(id: String, received: f64) -> EventPayload {
    download_progress(&DownloadProgress {
        id,
        received: nonneg_u64(received).unwrap_or(0),
    })
}

pub(crate) fn build_end_payload(
    id: String,
    state: &str,
    bytes: f64,
    error_code: Option<String>,
    numeric_code: Option<i32>,
    message: Option<String>,
) -> Option<EventPayload> {
    let outcome = match state {
        "finished" => DownloadOutcome::Finished {
            bytes: nonneg_u64(bytes).unwrap_or(0),
        },
        "failed" => {
            let message = non_empty(message).unwrap_or_else(|| match numeric_code {
                Some(code) => format!("download failed (error {code})"),
                None => "download failed".to_owned(),
            });
            DownloadOutcome::Failed {
                code: non_empty(error_code),
                message,
            }
        }
        _ => return None,
    };
    Some(download_end(&DownloadEnd { id, outcome }))
}

pub(crate) fn build_reply_payload(
    request_id: Option<&str>,
    ok: bool,
    message: Option<String>,
) -> Option<EventPayload> {
    let id = parse_request_id(request_id)?;
    let result = if ok {
        Ok(serde_json::Value::Null)
    } else {
        Err(non_empty(message).unwrap_or_else(|| "WebView download request failed".to_owned()))
    };
    Some(reply(id, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_suggested_filenames() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("/tmp/nested/file.zip"), "file.zip");
        assert_eq!(sanitize_filename("C:\\Users\\a\\doc.txt"), "doc.txt");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("."), "download");
        assert_eq!(sanitize_filename("   "), "download");
        assert_eq!(sanitize_filename("a\x00b\nc.txt"), "a_b_c.txt");
    }

    #[test]
    fn validates_staging_paths() {
        let dir = std::env::temp_dir().join(format!(
            "nsh-ohos-web-downloads-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        assert!(validate_staging_path(Path::new("relative.bin")).is_err());
        assert!(validate_staging_path(&dir.join("missing/file.bin")).is_err());

        let ok_path = dir.join("staged.bin");
        assert_eq!(
            validate_staging_path(&ok_path).unwrap(),
            ok_path.to_str().unwrap()
        );

        let existing = dir.join("existing.bin");
        fs::write(&existing, b"data").unwrap();
        assert!(validate_staging_path(&existing).is_err());

        #[cfg(unix)]
        {
            let broken = dir.join("broken-link.bin");
            std::os::unix::fs::symlink(dir.join("does-not-exist"), &broken).unwrap();
            assert!(validate_staging_path(&broken).is_err());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn builds_download_offer_progress_end_and_reply_payloads() {
        let offer = build_offer_payload(
            "dl-1".into(),
            "https://example.com/a.bin".into(),
            "dir/a.bin",
            Some("application/octet-stream".into()),
            -1.0,
            Some("GET".into()),
        );
        let EventPayload::Json(json) = offer else {
            panic!("expected Json payload");
        };
        assert_eq!(json["id"], "dl-1");
        assert_eq!(json["url"], "https://example.com/a.bin");
        assert_eq!(json["filename"], "a.bin");
        assert_eq!(json["mime_type"], "application/octet-stream");
        assert!(json["bytes"].is_null());
        assert_eq!(json["method"], "GET");
        assert_eq!(json["source"], "transfer");

        // Zero declared size is treated as unknown (`None`), matching Windows/Linux
        // so chunked/unknown-size transfers never fail core's publish size check.
        let zero_offer = build_offer_payload(
            "dl-zero".into(),
            "https://example.com/stream".into(),
            "stream.bin",
            None,
            0.0,
            None,
        );
        let EventPayload::Json(json) = zero_offer else {
            panic!("expected Json payload");
        };
        assert!(json["bytes"].is_null());

        let sized_offer = build_offer_payload(
            "dl-2".into(),
            "https://example.com/b.bin".into(),
            "",
            Some("  ".into()),
            4096.0,
            None,
        );
        let EventPayload::Json(json) = sized_offer else {
            panic!("expected Json payload");
        };
        assert_eq!(json["filename"], "download");
        assert!(json["mime_type"].is_null());
        assert_eq!(json["bytes"], 4096);

        let EventPayload::Json(progress) = build_progress_payload("dl-1".into(), 1024.0) else {
            panic!("expected Json payload");
        };
        assert_eq!(progress["id"], "dl-1");
        assert_eq!(progress["received"], 1024);

        let EventPayload::Json(finished) =
            build_end_payload("dl-1".into(), "finished", 4096.0, None, None, None).unwrap()
        else {
            panic!("expected Json payload");
        };
        assert_eq!(finished["outcome"], "finished");
        assert_eq!(finished["bytes"], 4096);

        let EventPayload::Json(failed) =
            build_end_payload("dl-1".into(), "failed", 0.0, None, Some(17), None).unwrap()
        else {
            panic!("expected Json payload");
        };
        assert_eq!(failed["outcome"], "failed");
        assert!(failed["code"].is_null());
        assert_eq!(failed["message"], "download failed (error 17)");

        let EventPayload::Json(teardown) = build_end_payload(
            "dl-1".into(),
            "failed",
            0.0,
            Some("ERR_NSH_DOWNLOAD_TEARDOWN".into()),
            None,
            Some("the web view went away".into()),
        )
        .unwrap() else {
            panic!("expected Json payload");
        };
        assert_eq!(teardown["outcome"], "failed");
        assert_eq!(teardown["code"], "ERR_NSH_DOWNLOAD_TEARDOWN");
        assert_eq!(teardown["message"], "the web view went away");

        let EventPayload::Json(ok_reply) = build_reply_payload(Some("42"), true, None).unwrap()
        else {
            panic!("expected Json payload");
        };
        assert_eq!(ok_reply["id"], 42);
        assert_eq!(ok_reply["ok"], true);

        let EventPayload::Json(err_reply) =
            build_reply_payload(Some("9007199254740993"), false, Some("gone".into())).unwrap()
        else {
            panic!("expected Json payload");
        };
        assert_eq!(err_reply["id"], 9007199254740993u64);
        assert_eq!(err_reply["ok"], false);
        assert_eq!(err_reply["error"], "gone");
        assert!(build_reply_payload(Some("not-a-number"), true, None).is_none());
    }
}
