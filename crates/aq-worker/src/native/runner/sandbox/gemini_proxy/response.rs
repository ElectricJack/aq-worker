use super::*;
use policy::{contains, dropped_header};
pub(super) async fn encode(state: &State, mut response: reqwest::Response) -> Result<Vec<u8>> {
    ensure!(
        !response.status().is_redirection(),
        "Gemini upstream redirect refused"
    );
    ensure!(
        response
            .headers()
            .get_all("content-encoding")
            .iter()
            .all(|value| value
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity"))),
        "encoded Gemini upstream response refused"
    );
    let status = response.status().as_u16();
    let mut result = format!("HTTP/1.1 {status} Upstream\r\nConnection: close\r\n").into_bytes();
    for (name, value) in response.headers() {
        ensure!(
            !contains(value.as_bytes(), &state.key),
            "Gemini upstream returned protected material"
        );
        if !dropped_header(name.as_str()) && !name.as_str().starts_with("set-cookie") {
            result.extend_from_slice(name.as_str().as_bytes());
            result.extend_from_slice(b": ");
            result.extend_from_slice(value.as_bytes());
            result.extend_from_slice(b"\r\n");
        }
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE,
            "Gemini upstream body bound"
        );
        body.extend_from_slice(&chunk);
    }
    ensure!(
        !contains(&body, &state.key),
        "Gemini upstream returned protected material"
    );
    result.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    result.extend_from_slice(&body);
    Ok(result)
}
