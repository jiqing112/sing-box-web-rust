// 前端静态资源：构建期 embed，运行期按 ETag 协商缓存。
// ETag 由嵌入资源的实际内容算出：前端内容变了 ETag 自动变，
// 避免手写版本号忘改导致浏览器一直命中 304。

use std::sync::OnceLock;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

static INDEX_HTML: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/dist/index.html"));
static APP_JS: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/dist/app.js"));
static STYLES_CSS: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/dist/styles.css"));

fn etag() -> &'static str {
    static ETAG: OnceLock<String> = OnceLock::new();
    ETAG.get_or_init(|| {
        let mut data = Vec::new();
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.js", APP_JS),
            ("styles.css", STYLES_CSS),
        ] {
            data.extend_from_slice(name.as_bytes());
            data.extend_from_slice(&body.len().to_le_bytes());
            data.extend_from_slice(body);
        }
        crate::util::sha256_hex(&data)[..16].to_string()
    })
}

fn file_for(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/index.html" => Some(("text/html; charset=utf-8", INDEX_HTML)),
        "/app.js" => Some(("application/javascript; charset=utf-8", APP_JS)),
        "/styles.css" => Some(("text/css; charset=utf-8", STYLES_CSS)),
        _ => None,
    }
}

/// 没有内容哈希的文件名，统一走协商缓存 + SPA fallback：
/// 路径里没有 "." 的都回 index.html（前端是单页）。
pub fn handler(uri_path: &str, headers: &HeaderMap) -> Response {
    // ETag 命中直接 304
    let et = etag();
    let quoted = format!("\"{et}\"");
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|t| t.trim() == quoted || t.trim() == format!("W/{quoted}")) {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            resp.headers_mut().insert(header::ETAG, HeaderValue::from_str(&quoted).unwrap());
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            );
            return resp;
        }
    }

    let (ctype, body) = match file_for(uri_path) {
        Some(hit) => hit,
        None => {
            if uri_path == "/" || !uri_path.contains('.') {
                ("text/html; charset=utf-8", INDEX_HTML)
            } else {
                let mut resp = Response::new(axum::body::Body::from("{\"error\":\"not_found\",\"message\":\"404 page not found\"}"));
                *resp.status_mut() = StatusCode::NOT_FOUND;
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                );
                return resp;
            }
        }
    };

    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    resp.headers_mut().insert(header::ETAG, HeaderValue::from_str(&quoted).unwrap());
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    resp
}
