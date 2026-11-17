//! A local Git smart-HTTP endpoint for result pushes (`git http-backend`).
use axum::{Router, body::Bytes, extract::Path, http::HeaderMap, routing::any};
use std::sync::Arc;
use uuid::Uuid;

pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().into()
}
/// Git smart HTTP for the attempt's remote: `git http-backend` over `root/remote.git`.
/// Each request is recorded as `METHOD attempt path authorization cf-id cf-secret`.
pub fn smart_http(root: std::path::PathBuf, seen: Arc<std::sync::Mutex<Vec<String>>>) -> Router {
    Router::new().route(
        "/v1/execution/git/{attempt}/{*rest}",
        any(
            move |method: axum::http::Method,
                  Path((attempt, rest)): Path<(Uuid, String)>,
                  uri: axum::http::Uri,
                  h: HeaderMap,
                  body: Bytes| async move {
                let text = |name: &str| {
                    let value = h.get(name).and_then(|v| v.to_str().ok());
                    value.unwrap_or_default().to_string()
                };
                seen.lock().unwrap().push(format!(
                    "{method} {attempt} {rest} {} {} {}",
                    text("authorization"),
                    text("cf-access-client-id"),
                    text("cf-access-client-secret")
                ));
                let mut child = std::process::Command::new("/usr/bin/git")
                    .arg("http-backend")
                    .env("GIT_PROJECT_ROOT", &root)
                    .env("GIT_HTTP_EXPORT_ALL", "1")
                    .env("PATH_INFO", format!("/remote.git/{rest}"))
                    .env("REQUEST_METHOD", method.as_str())
                    .env("QUERY_STRING", uri.query().unwrap_or_default())
                    .env("CONTENT_TYPE", text("content-type"))
                    .env("CONTENT_LENGTH", body.len().to_string())
                    .env("HTTP_CONTENT_ENCODING", text("content-encoding"))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut stdin = child.stdin.take().unwrap();
                let writer = std::thread::spawn(move || {
                    let _ = std::io::Write::write_all(&mut stdin, &body);
                });
                let out = child.wait_with_output().unwrap();
                writer.join().unwrap();
                let cut = out
                    .stdout
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .unwrap();
                let mut response = axum::http::Response::builder();
                for line in std::str::from_utf8(&out.stdout[..cut]).unwrap().lines() {
                    let (name, value) = line.split_once(": ").unwrap();
                    response = if name == "Status" {
                        response.status(value[..3].parse::<u16>().unwrap())
                    } else {
                        response.header(name, value)
                    };
                }
                response
                    .body(axum::body::Body::from(out.stdout[cut + 4..].to_vec()))
                    .unwrap()
            },
        ),
    )
}
