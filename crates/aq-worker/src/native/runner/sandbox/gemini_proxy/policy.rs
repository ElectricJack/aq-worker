use super::{KEY_HEADER, METHODS};
/// `/v1beta/models/<model>:<method>` with an optional `?alt=sse`. A `key=`
/// query (the other Gemini credential channel) is never forwarded.
pub(super) fn allowed_target(target: &str) -> bool {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(rest) = path.strip_prefix("/v1beta/models/") else {
        return false;
    };
    let Some((model, method)) = rest.split_once(':') else {
        return false;
    };
    valid_model(model) && METHODS.contains(&method) && (query.is_empty() || query == "alt=sse")
}
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        && !model.starts_with('.')
}

pub(super) fn dropped_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "authorization"
            | "accept-encoding"
            | KEY_HEADER
            | "x-goog-user-project"
            | "x-gemini-api-privileged-user-id"
    )
}

pub(super) fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|part| part == needle.as_bytes())
}
