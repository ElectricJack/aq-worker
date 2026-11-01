//! Worker-owned launch/manifest joins around a neutral transport byte policy.
use super::*;
pub use aq_responses_transport::MAX_REQUEST_BYTES;
use aq_session_protocol::ModelSettings;
use std::time::Duration;
pub struct ResponsesScope {
    binding: LaunchBinding,
    model: ModelSettings,
    policy: aq_responses_transport::ResponsesPolicy,
}
impl ResponsesScope {
    pub fn new(binding: LaunchBinding, model: &ModelSettings, deadline_ns: u64) -> Result<Self> {
        binding.validate()?;
        let owner = crate::native::journal::canonical_digest(&binding)?;
        let policy = aq_responses_transport::ResponsesPolicy::new(owner, model, deadline_ns)?;
        Ok(Self {
            binding,
            model: model.clone(),
            policy,
        })
    }
    pub fn binding(&self) -> &LaunchBinding {
        &self.binding
    }
    pub(super) fn validate_manifest(
        &self,
        manifest: &aq_session_protocol::LaunchManifest,
    ) -> Result<()> {
        manifest.validate()?;
        ensure!(
            self.binding.launch_manifest_digest
                == crate::native::journal::canonical_digest(manifest)?
                && self.binding.session == manifest.bindings.session
                && self.binding.attempt_id == manifest.bindings.attempt_id
                && self.binding.assignment_generation == manifest.bindings.assignment_generation
                && self.binding.graph_access_profile_digest
                    == manifest.bindings.graph_access_profile.digest
                && manifest.model == self.model,
            "scoped Responses exact retained manifest selection"
        );
        self.remaining()?;
        Ok(())
    }
    fn remaining(&self) -> Result<Duration> {
        let now = crate::native::http::boot_ns()?;
        let deadline = self.policy.original_deadline_boot_ns();
        ensure!(now < deadline, "scoped Responses authority expired");
        Ok(Duration::from_nanos(deadline - now))
    }
    pub fn prepare(&self, body: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.policy.prepare(body)
    }
    pub(super) fn into_parts(self) -> (LaunchBinding, aq_responses_transport::ResponsesPolicy) {
        (self.binding, self.policy)
    }
}
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::Value;
    const MAX_NODES: usize = 65536;
    pub fn scope() -> ResponsesScope {
        let fixture: Value = serde_json::from_slice(include_bytes!(
            "../../../../../../vendor/aq-admission-0.1.9/artifacts/startup.fixture.json"
        ))
        .unwrap();
        let mut offer: aq_admission::startup::NativeOffer =
            serde_json::from_value(fixture["native_offer"].clone()).unwrap();
        let manifest = &mut offer.offered.offer.manifest;
        manifest.model.provider = aq_session_protocol::Text::new("openai").unwrap();
        manifest.model.model = aq_session_protocol::Text::new("offline-model").unwrap();
        manifest.model.reasoning = aq_session_protocol::Text::new("medium").unwrap();
        manifest.model.max_output_tokens = 100;
        manifest.model.spending_budget = None;
        offer.offered.offer.binding.launch_manifest_digest =
            crate::native::journal::canonical_digest(manifest).unwrap();
        ResponsesScope::new(
            offer.offered.offer.binding.clone(),
            &manifest.model,
            crate::native::http::boot_ns().unwrap() + 10_000_000_000,
        )
        .unwrap()
    }
    pub fn body() -> Vec<u8> {
        br#"{"model":"offline-model","stream":true,"store":false,"input":[{"type":"message","role":"user","content":"literal startup"}],"reasoning":{"effort":"medium","summary":"auto"},"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"text":{"verbosity":"low"},"include":[],"client_metadata":{"a":"b"}}"#.to_vec()
    }
    #[test]
    fn injects_exact_response_cap_preserving_every_other_setting() {
        let scope = scope();
        assert_eq!(
            scope.prepare(&body()).unwrap().as_slice(),
            include_bytes!("goldens/capped-request.json"),
            "pre-extraction accepted request bytes must stay exact"
        );
        let original: Value = serde_json::from_slice(&body()).unwrap();
        let mut got: Value = serde_json::from_slice(&scope.prepare(&body()).unwrap()).unwrap();
        assert_eq!(
            got.as_object_mut().unwrap().remove("max_output_tokens"),
            Some(Value::from(100))
        );
        assert_eq!(got, original);
    }
    #[test]
    fn changed_selection_ambiguous_json_stateful_and_unbounded_requests_refuse() {
        let scope = scope();
        for (field, value) in [
            ("model", Value::from("other-model")),
            ("stream", Value::from(false)),
            ("store", Value::from(true)),
            ("max_output_tokens", Value::from(101)),
            ("max_output_tokens", Value::from(99)),
            ("previous_response_id", Value::from("resp_foreign")),
            ("background", Value::from(true)),
            ("access_programs", serde_json::json!({"cyber":"standard"})),
            ("reasoning", serde_json::json!({"effort":"high"})),
        ] {
            let mut body: Value = serde_json::from_slice(&body()).unwrap();
            body[field] = value;
            assert!(
                scope.prepare(&serde_json::to_vec(&body).unwrap()).is_err(),
                "{field}"
            );
        }
        for input in [
            br#"{"model":"a","model":"b"}"#.as_slice(),
            br#"{"reasoning":{"effort":"medium","effort":"high"}}"#.as_slice(),
            br#"[]"#.as_slice(),
            br#"{} trailing"#.as_slice(),
        ] {
            assert!(scope.prepare(input).is_err());
        }
        assert!(scope.prepare(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let depth = format!("{}0{}", "[".repeat(40), "]".repeat(40));
        assert!(scope.prepare(depth.as_bytes()).is_err());
        let nodes = format!("[{}]", vec!["0"; MAX_NODES + 1].join(","));
        assert!(scope.prepare(nodes.as_bytes()).is_err());
    }
    #[test]
    fn expired_scope_and_identical_cap_refuse_or_remain_exact() {
        let scope = scope();
        let mut body: Value = serde_json::from_slice(&body()).unwrap();
        body["max_output_tokens"] = 100.into();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &scope.prepare(&serde_json::to_vec(&body).unwrap()).unwrap()
            )
            .unwrap(),
            body
        );
        let expired = ResponsesScope::new(
            scope.binding.clone(),
            &scope.model,
            crate::native::http::boot_ns().unwrap() + 1_000_000,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(expired.prepare(&self::body()).is_err());
    }
    #[test]
    fn server_side_references_and_hosted_tools_refuse_before_cap_insertion() {
        let scope = scope();
        for item in [
            serde_json::json!({"id":"item_foreign"}),
            serde_json::json!({"type":"item_reference","id":"item_foreign"}),
            serde_json::json!({"type":"message","role":"user","content":[{"type":"input_file","file_id":"file_foreign"}]}),
            serde_json::json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://foreign.example/image"}]}),
            serde_json::json!({"type":"additional_tools","role":"developer","tools":[{"type":"web_search"}]}),
        ] {
            let mut value: Value = serde_json::from_slice(&body()).unwrap();
            value["input"] = serde_json::json!([item]);
            assert!(scope.prepare(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        for tool in [
            "web_search",
            "file_search",
            "mcp",
            "computer_use_preview",
            "code_interpreter",
        ] {
            let mut value: Value = serde_json::from_slice(&body()).unwrap();
            value["tools"] = serde_json::json!([{"type":tool,"name":"foreign"}]);
            assert!(scope.prepare(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }
}
