//! Native CLI diagnostics retain causes without exposing enrollment or input bytes.
use aq_worker::config::WorkerConfig;
use std::process::ExitCode;

pub(super) fn refuse(
    error: &anyhow::Error,
    worker: Option<&WorkerConfig>,
    disposition: &str,
) -> ExitCode {
    eprintln!("error: {}", message(error, worker));
    // Operator diagnostics: the error's capture site (code locations only).
    if std::env::var_os("AQ_NATIVE_BACKTRACE").is_some() {
        eprintln!("backtrace:\n{}", error.backtrace());
    }
    super::usage_err(disposition)
}

fn message(error: &anyhow::Error, worker: Option<&WorkerConfig>) -> String {
    let mut message = error
        .chain()
        .map(|cause| {
            // Parser errors can quote private input, even before WorkerConfig loads.
            if cause.is::<toml::de::Error>() {
                "invalid TOML configuration (input redacted)".to_string()
            } else if let Some(error) = cause.downcast_ref::<serde_json::Error>() {
                format!(
                    "invalid JSON {:?} at line {} column {} (input redacted)",
                    error.classify(),
                    error.line(),
                    error.column()
                )
            } else if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
                // A request URL can contain authentication in userinfo or query.
                let text = error.to_string();
                match error.url() {
                    Some(url) => text.replace(url.as_str(), "<redacted URL>"),
                    None => text,
                }
            } else {
                cause.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(": ");
    if let Some(worker) = worker {
        let mut secrets: Vec<_> = [
            Some(worker.credential.as_str()),
            worker.cf_client_id.as_deref(),
            worker.cf_client_secret.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|secret| !secret.is_empty())
        .collect();
        // Replace longer values first when credentials overlap.
        secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        for secret in secrets {
            message = message.replace(secret, "<redacted>");
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_diagnostics_preserve_context_and_redact_enrollment_secrets() {
        let worker = WorkerConfig {
            server_url: "https://example.invalid".into(),
            node_id: uuid::Uuid::from_u128(1),
            credential: "private-token".into(),
            cf_client_id: Some("private-token-id".into()),
            cf_client_secret: Some("private-token-secret".into()),
        };
        let error = anyhow::anyhow!("relay refused private-token-id private-token-secret")
            .context("native preflight private-token");
        assert_eq!(
            message(&error, Some(&worker)),
            "native preflight <redacted>: relay refused <redacted> <redacted>"
        );
    }

    #[test]
    fn native_diagnostics_redact_parser_input_before_enrollment_loads() {
        let error =
            toml::from_str::<WorkerConfig>("credential = 'private-token'\ninvalid").unwrap_err();
        let error = anyhow::Error::new(error).context("parse worker configuration");
        assert_eq!(
            message(&error, None),
            "parse worker configuration: invalid TOML configuration (input redacted)"
        );

        let error = serde_json::from_str::<u32>("\"private-token\"").unwrap_err();
        let error = anyhow::Error::new(error).context("invalid native configuration");
        let message = message(&error, None);
        assert!(message.starts_with("invalid native configuration: invalid JSON Data"));
        assert!(!message.contains("private-token"));
    }
}
