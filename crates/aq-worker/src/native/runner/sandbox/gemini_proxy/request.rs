use super::*;
use policy::{allowed_target, dropped_header};
pub(super) async fn read(
    state: &State,
    socket: &mut TcpStream,
) -> Result<(String, HeaderMap, usize)> {
    let mut head = Zeroizing::new(Vec::new());
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await?);
        ensure!(head.len() <= MAX_HEAD, "Gemini request header bound");
    }
    let text = std::str::from_utf8(&head)?;
    let mut lines = text.split("\r\n");
    let target = request_target(lines.next().unwrap_or_default())?;
    let (headers, length) = read_headers(state, lines)?;
    Ok((target, headers, length))
}
fn request_target(line: &str) -> Result<String> {
    let mut first = line.split(' ');
    ensure!(first.next() == Some("POST"), "Gemini request method");
    let target = first.next().unwrap_or_default().to_owned();
    ensure!(
        allowed_target(&target) && first.next() == Some("HTTP/1.1") && first.next().is_none(),
        "Gemini request target"
    );
    Ok(target)
}
fn read_headers<'a>(
    state: &State,
    lines: impl Iterator<Item = &'a str>,
) -> Result<(HeaderMap, usize)> {
    let mut headers = HeaderMap::new();
    let mut names = std::collections::BTreeSet::new();
    let mut length = None;
    let mut host = false;
    let mut authorized = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Gemini request header"))?;
        let name = HeaderName::from_bytes(name.as_bytes())?;
        let value = HeaderValue::from_str(value.trim())?;
        ensure!(
            names.insert(name.as_str().to_owned()),
            "duplicate Gemini request header"
        );
        ensure!(
            !matches!(name.as_str(), "transfer-encoding" | "upgrade" | "expect"),
            "unsupported Gemini framing"
        );
        match name.as_str() {
            "host" => host = value.as_bytes() == state.authority.as_bytes(),
            KEY_HEADER => authorized = value.as_bytes() == state.capability.as_bytes(),
            "content-length" => {
                let text = value.to_str()?;
                ensure!(
                    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
                    "Gemini request length"
                );
                length = Some(text.parse::<usize>()?);
            }
            _ => {}
        }
        if !dropped_header(name.as_str()) {
            headers.append(name, value);
        }
    }
    // Refused before any upstream connection: no capability, no forwarding.
    ensure!(authorized, "Gemini capability refused");
    let length = length.ok_or_else(|| anyhow::anyhow!("Gemini request length"))?;
    ensure!(host && length <= MAX_BODY, "Gemini authority/body bound");
    upstream_headers(state, &mut headers)?;
    Ok((headers, length))
}
fn upstream_headers(state: &State, headers: &mut HeaderMap) -> Result<()> {
    let connection: Vec<String> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for name in connection.iter().filter(|v| !v.is_empty()) {
        headers.remove(name.as_str());
    }
    let mut key = HeaderValue::from_str(&state.key)?;
    key.set_sensitive(true);
    headers.insert(HeaderName::from_static(KEY_HEADER), key);
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    Ok(())
}
