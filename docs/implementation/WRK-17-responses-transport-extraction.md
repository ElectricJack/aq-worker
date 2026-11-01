# Scoped Responses transport extraction

Source-first seam frozen before extraction. Baseline is worker `45420eb` plus
regression-only `4ff62a5`; all14 bounded tests passed before factoring
([record](evidence/responses-transport-extraction/baseline.json)). No production transport,
launch, material owner, helper or admission behavior changes in that baseline.
No provider request or capsule process is part of qualification.

## Component boundary

Add same-workspace `aq-responses-transport`, `publish=false`, depending only on
existing `anyhow`, `aq-session-protocol`, `libc`, `reqwest`, `serde`, `serde_json`,
`tokio`, `uuid` and `zeroize`. Development tests may use existing `axum`.
It must not depend on aq-worker, admission, session-host, a daemon or a material
store. The shared protocol supplies only Digest/ModelSettings validation.

Move the existing exact HTTP parser, closed request/item policy, cap injection,
bounded SSE forwarding, original BOOTTIME deadline and cancellation owner into
the crate. Keep header/request/stream/request-count limits unchanged. Production
upstream stays the fixed HTTPS OpenAI Responses route with no redirects/proxy;
the numeric loopback peer constructor remains test-support only. No provider
connectivity or model-semantic qualification is inferred from the extraction.

```rust
pub struct ResponsesPolicy { /* neutral immutable policy, no launch authority */ }
impl ResponsesPolicy {
    pub fn new(owner_scope: Digest, model: &ModelSettings,
               original_deadline_boot_ns: u64) -> anyhow::Result<Self>;
    pub fn owner_scope(&self) -> &Digest;
    pub fn scope_digest(&self) -> &Digest;
    pub fn original_deadline_boot_ns(&self) -> u64;
    pub fn prepare(&self, body: &[u8])
        -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>>;
}
pub trait RetainedUpstreamCredential: Send + Sync {
    fn read_current(&self) -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>>;
}
impl ScopedResponsesTransport {
    pub async fn start_on_listener(
        listener: std::net::TcpListener,
        expected_origin: &str,
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        credential: std::sync::Arc<dyn RetainedUpstreamCredential>,
    ) -> anyhow::Result<Self>;
    pub fn origin(&self) -> &str;
    pub fn owner_scope(&self) -> &Digest;
    pub fn scope_digest(&self) -> &Digest;
    pub fn original_deadline_boot_ns(&self) -> u64;
    pub fn local_capability_digest(&self) -> Digest;
    pub fn local_capability(&self) -> zeroize::Zeroizing<Vec<u8>>;
    pub fn cancel(&self);
    pub async fn stop(self) -> anyhow::Result<()>;
}
```

The consumed listener is independently checked as a listening TCP socket whose
local address is exactly numeric IPv4 `127.0.0.1:<nonzero>`. Expected origin must
be exactly its canonical `http://127.0.0.1:<port>` form. No DNS, caller path,
wildcard/IPv6 address, rebinding or network-namespace transition is performed.
Set nonblocking and consume that same descriptor into Tokio; do not bind a
replacement. This seam can later receive a prepared listener without importing
or implementing that capsule profile.

The scope digest is the canonical closed recipe digest of
`contract=aq.responses-policy-1`, opaque
owner-scope digest, full shared ModelSettings and original deadline. It is
nonsecret metadata, not a grant. The local capability is newly generated for
each production constructor and remains zeroizing parent data; metadata only
exposes its digest. The test-support constructor may retain explicitly supplied
synthetic keys so existing worker tests/public test-support API remain usable.

## Credential and worker joins

`RetainedUpstreamCredential` supplies already-authorized parent-only bytes. The
core invokes it through bounded `spawn_blocking`, once initially and before
every forwarded request, and compares the current key with the original key.
Check original deadline before and after each read and immediately before
forwarding. The callback must preserve its owner's bounded regular-file,
original-record and source identity checks; it cannot mint authority or extend
expiry. No upstream key is serialized, debugged, returned in metadata or sent
to the local caller.

`cancel()` triggers a synchronous local deny latch and wakes serving shutdown;
it is not completed closure or a stop receipt. Awaited `stop()`/serving-task
completion must close forwarding before the aggregate claims closure.
Cancellation drops the serving future while a blocking read may finish later.
That late value is zeroizing and cannot resume forwarding. Stop/drop fences
before any physical capsule stop; an in-progress filesystem read itself is not
claimed forcibly canceled. Do not return a stopped-process receipt.

The worker keeps `ResponsesScope` and `ScopedResponsesTransport` as public
wrappers with the current constructor/method signatures. Scope retains the real
LaunchBinding; its opaque owner digest is the canonical digest of that binding.
Worker code still validates exact manifest/model/session/attempt/generation/
graph-access joins, MaterialReceipt and original InstalledMaterial/SourcePin.
Its retained credential adapter reuses the existing validated Codex key and
original installed owner on every current read. Legacy start binds its parent
loopback listener then delegates to this same consumed-listener core. No launch
eligibility, grant or adapter production gate changes.

## Baseline and acceptance

Before factoring source, freeze/run existing policy, transport, retained-material
and logical-stop tests plus these added regression-only cases:

- Exact canonical capped request bytes pinned in `goldens/capped-request.json`.
- Wrong HTTP route/host/local capability/MIME/length, duplicate auth and forbidden
  Upgrade/Expect framing cause zero upstream requests and no successful response.
- Upstream redirect or non-SSE response cannot become a local success; redirects
  never reach the foreign route.

Keep existing request/item refusal cases, distinct synthetic key/SSE fidelity,
fresh retained capability, changed original record/key refusal, manifest
substitution refusal, and synchronous logical-stop fence tests. After extraction
run the same worker group, core's standalone group and Codex scoped-input golden.
Add consumed-listener origin/socket refusal and current-credential held-read
cancel/expiry/change regressions in the core. Check no worker/host/admission
dependency enters the core graph. No provider or new capsule execution is used.

The baseline test lease and subsequent focused qualification lease are parent
scheduled; source work creates no new target cache or broad workspace build.
