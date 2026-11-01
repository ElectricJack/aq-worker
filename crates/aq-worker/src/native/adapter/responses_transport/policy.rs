use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use zeroize::Zeroize;
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 65536;
const MAX_DEPTH: usize = 32;
/// Exact immutable attempt selection. Construction validates a byte policy;
/// it supplies no execution Grant, upstream quota or aggregate budget authority.
pub struct ResponsesScope {
    binding: LaunchBinding,
    model: String,
    reasoning: String,
    cap: u64,
    deadline_ns: u64,
}
impl ResponsesScope {
    pub fn new(binding: LaunchBinding, model: &ModelSettings, deadline_ns: u64) -> Result<Self> {
        binding.validate()?;
        ensure!(
            model.provider.as_str() == "openai"
                && model.spending_budget.is_none()
                && model.max_output_tokens >= 16
                && model.max_output_tokens <= 1048576
                && matches!(model.reasoning.as_str(), "low" | "medium" | "high"),
            "unsupported Responses selection"
        );
        let scope = Self {
            binding,
            model: model.model.as_str().into(),
            reasoning: model.reasoning.as_str().into(),
            cap: model.max_output_tokens,
            deadline_ns,
        };
        scope.remaining()?;
        Ok(scope)
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
                && manifest.model.provider.as_str() == "openai"
                && manifest.model.model.as_str() == self.model
                && manifest.model.reasoning.as_str() == self.reasoning
                && manifest.model.max_output_tokens == self.cap
                && manifest.model.spending_budget.is_none(),
            "scoped Responses exact retained manifest selection"
        );
        self.remaining()?;
        Ok(())
    }
    pub(super) fn remaining(&self) -> Result<Duration> {
        let now = crate::native::http::boot_ns()?;
        ensure!(now < self.deadline_ns, "scoped Responses authority expired");
        Ok(Duration::from_nanos(self.deadline_ns - now))
    }
    /// Preserves every accepted JSON setting except adding the selected cap.
    /// Duplicate fields, nonstreaming/stateful routes and changed selection
    /// refuse before any upstream credential-bearing request.
    pub fn prepare(&self, body: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.remaining()?;
        ensure!(
            !body.is_empty() && body.len() <= MAX_REQUEST_BYTES,
            "Responses request byte bound"
        );
        let mut deserializer = serde_json::Deserializer::from_slice(body);
        let mut nodes = 0;
        let mut root = Erased(
            Seed {
                nodes: &mut nodes,
                depth: 0,
            }
            .deserialize(&mut deserializer)?,
        );
        deserializer.end()?;
        let object = root
            .0
            .as_object_mut()
            .context("Responses body must be object")?;
        for name in object.keys() {
            ensure!(
                matches!(
                    name.as_str(),
                    "model"
                        | "stream"
                        | "service_tier"
                        | "input"
                        | "tools"
                        | "tool_choice"
                        | "parallel_tool_calls"
                        | "reasoning"
                        | "store"
                        | "stream_options"
                        | "include"
                        | "prompt_cache_key"
                        | "text"
                        | "client_metadata"
                        | "access_programs"
                        | "max_output_tokens"
                ),
                "unsupported Responses request setting"
            );
        }
        ensure!(
            object.get("model").and_then(Value::as_str) == Some(self.model.as_str())
                && object.get("stream") == Some(&Value::Bool(true))
                && object.get("store") == Some(&Value::Bool(false))
                && object.get("input").is_some_and(Value::is_array),
            "Responses request selection/stream/storage mismatch"
        );
        ensure!(
            object
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
                == Some(self.reasoning.as_str()),
            "Responses reasoning selection mismatch"
        );
        ensure!(
            object.get("access_programs").is_none_or(Value::is_null),
            "unqualified Responses access program"
        );
        super::items::validate(object)?;
        if let Some(existing) = object.get("max_output_tokens") {
            ensure!(
                existing.as_u64() == Some(self.cap),
                "Responses conflicting output cap"
            );
        }
        object.insert("max_output_tokens".into(), Value::from(self.cap));
        let bytes = Zeroizing::new(serde_json::to_vec(&root.0)?);
        ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "Responses injected request bound"
        );
        Ok(bytes)
    }
}
struct Erased(Value);
impl Drop for Erased {
    fn drop(&mut self) {
        erase(&mut self.0);
    }
}
fn erase(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(a) => a.iter_mut().for_each(erase),
        Value::Object(o) => {
            for (mut k, mut v) in std::mem::take(o) {
                k.zeroize();
                erase(&mut v);
            }
        }
        _ => {}
    }
}
struct Seed<'a> {
    nodes: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        *self.nodes += 1;
        if *self.nodes > MAX_NODES || self.depth > MAX_DEPTH {
            return Err(serde::de::Error::custom("Responses JSON structural bound"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded unique-field JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            nodes: self.nodes,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate Responses JSON field"));
            }
            let value = map.next_value_seed(Seed {
                nodes: self.nodes,
                depth: self.depth + 1,
            })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
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
        let mut scope = scope();
        let mut body: Value = serde_json::from_slice(&body()).unwrap();
        body["max_output_tokens"] = 100.into();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &scope.prepare(&serde_json::to_vec(&body).unwrap()).unwrap()
            )
            .unwrap(),
            body
        );
        scope.deadline_ns = 0;
        assert!(scope.prepare(&self::body()).is_err());
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
