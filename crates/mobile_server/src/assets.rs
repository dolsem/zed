use std::borrow::Cow;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use rust_embed::RustEmbed;
use sha2::{Digest as _, Sha256};

pub const WEB_DIR_ENV_VAR: &str = "ZED_MOBILE_WEB_DIR";

const INDEX_FILE: &str = "index.html";
const IMMUTABLE_PREFIX: &str = "assets/";
const NO_CACHE_FILES: &[&str] = &[INDEX_FILE, "sw.js", "manifest.webmanifest"];

#[derive(RustEmbed)]
#[folder = "web/dist"]
struct EmbeddedAssets;

struct Asset {
    contents: Cow<'static, [u8]>,
    etag: String,
}

/// Where static files come from: the files embedded at build time, or a
/// directory on disk while developing the PWA.
pub enum AssetSource {
    Embedded,
    Directory(PathBuf),
}

impl AssetSource {
    pub fn from_environment() -> Self {
        match std::env::var_os(WEB_DIR_ENV_VAR) {
            Some(directory) if !directory.is_empty() => {
                let directory = PathBuf::from(directory);
                log::info!("serving PWA files from {directory:?}");
                Self::Directory(directory)
            }
            _ => Self::Embedded,
        }
    }

    async fn load(&self, path: &str) -> Option<Asset> {
        match self {
            Self::Embedded => {
                let file = EmbeddedAssets::get(path)?;
                let etag = etag_from_hash(&file.metadata.sha256_hash());
                Some(Asset {
                    contents: file.data,
                    etag,
                })
            }
            Self::Directory(directory) => {
                let mut file_path = directory.clone();
                for segment in path.split('/') {
                    file_path.push(segment);
                }
                let contents = tokio::fs::read(&file_path).await.ok()?;
                let etag = etag_from_hash(&Sha256::digest(&contents).into());
                Some(Asset {
                    contents: Cow::Owned(contents),
                    etag,
                })
            }
        }
    }

    pub async fn respond(&self, method: Method, uri: Uri, request_headers: HeaderMap) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }

        let requested_path = uri.path().trim_start_matches('/');
        let requested_path = if requested_path.is_empty() {
            INDEX_FILE
        } else {
            requested_path
        };
        let requested_asset = match sanitize_path(requested_path) {
            Some(path) => self.load(path).await,
            None => None,
        };

        let (path, asset) = match requested_asset {
            Some(asset) => (requested_path, asset),
            // Hashed bundles must never be answered with the HTML shell, or a
            // stale page would try to execute it as a script.
            None if requested_path.starts_with(IMMUTABLE_PREFIX) => {
                return StatusCode::NOT_FOUND.into_response();
            }
            None => match self.load(INDEX_FILE).await {
                Some(asset) => (INDEX_FILE, asset),
                None => return StatusCode::NOT_FOUND.into_response(),
            },
        };

        let mut response_headers = HeaderMap::new();
        if let Ok(content_type) = HeaderValue::from_str(&content_type(path)) {
            response_headers.insert(header::CONTENT_TYPE, content_type);
        }
        response_headers.insert(header::CACHE_CONTROL, cache_control(path));
        if let Ok(etag) = HeaderValue::from_str(&asset.etag) {
            response_headers.insert(header::ETAG, etag);
        }

        let is_fresh = request_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|candidate| candidate.trim() == asset.etag)
            });
        if is_fresh {
            return (StatusCode::NOT_MODIFIED, response_headers).into_response();
        }

        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(asset.contents)
        };
        (StatusCode::OK, response_headers, body).into_response()
    }
}

/// Rejects anything that could escape the asset directory when served from
/// disk, such as `..` segments or Windows drive and separator characters.
fn sanitize_path(path: &str) -> Option<&str> {
    let is_safe = path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment.contains(['\\', ':', '\0'])
    });
    is_safe.then_some(path)
}

fn content_type(path: &str) -> String {
    if path.ends_with(".webmanifest") {
        return "application/manifest+json".to_string();
    }
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let is_text = mime.type_() == mime_guess::mime::TEXT
        || matches!(
            mime.essence_str(),
            "application/javascript" | "application/json"
        );
    if is_text {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    }
}

fn cache_control(path: &str) -> HeaderValue {
    if path.starts_with(IMMUTABLE_PREFIX) && !NO_CACHE_FILES.contains(&path) {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    }
}

fn etag_from_hash(hash: &[u8; 32]) -> String {
    let hex = hash
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("\"{hex}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_path() {
        assert_eq!(sanitize_path(""), None);
        assert_eq!(sanitize_path("assets/app.js"), Some("assets/app.js"));
        assert_eq!(sanitize_path("../state.json"), None);
        assert_eq!(sanitize_path("assets/../../x"), None);
        assert_eq!(sanitize_path("C:/Windows"), None);
        assert_eq!(sanitize_path("assets\\..\\x"), None);
        assert_eq!(sanitize_path("assets//x"), None);
    }

    #[test]
    fn test_headers() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("manifest.webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(content_type("icon.png"), "image/png");
        assert_eq!(
            cache_control("assets/index-abc123.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("index.html"), "no-cache");
    }
}
