//! Wraps any Docker transport with the scripted double's one-shot fault
//! effects and request recording, so live tests share the same semantics.
use super::docker_double::{Effect, Rule};
use aq_worker::backend::docker_local::{
    DockerMethod, DockerRequest, DockerResponse, DockerTransport, TransportError,
};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct FaultTransport<T> {
    inner: T,
    rules: Arc<Mutex<Vec<Rule>>>,
    requests: Arc<Mutex<Vec<(DockerMethod, String)>>>,
}

impl<T> FaultTransport<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            rules: Arc::default(),
            requests: Arc::default(),
        }
    }
    pub fn script(&self, rule: Rule) {
        self.rules.lock().unwrap().push(rule);
    }
    pub fn requests(&self) -> Vec<(DockerMethod, String)> {
        self.requests.lock().unwrap().clone()
    }
    pub fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

impl<T: DockerTransport> DockerTransport for FaultTransport<T> {
    async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
        self.requests
            .lock()
            .unwrap()
            .push((request.method, request.path.clone()));
        let effect = {
            let mut rules = self.rules.lock().unwrap();
            let index = rules.iter().position(|rule| {
                rule.method == request.method && request.path.starts_with(&rule.path_prefix)
            });
            index.map(|index| rules.remove(index).effect)
        };
        match effect {
            None => self.inner.send(request).await,
            Some(Effect::FailBefore(status)) => Ok(DockerResponse {
                status,
                body: br#"{"message":"scripted failure"}"#.to_vec(),
            }),
            Some(Effect::LieOkWithoutApply) => Ok(DockerResponse {
                status: match request.method {
                    DockerMethod::Get | DockerMethod::Put => 200,
                    DockerMethod::Post if request.path.contains("/create") => 201,
                    DockerMethod::Post | DockerMethod::Delete => 204,
                },
                body: b"{}".to_vec(),
            }),
            Some(Effect::DropReplyAfterApply) => {
                self.inner.send(request).await?;
                Err(TransportError::io("connection reset after apply"))
            }
        }
    }
}
