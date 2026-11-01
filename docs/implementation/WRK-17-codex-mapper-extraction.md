# Minimal reusable Codex recipe mapping

Status: bounded pure extraction implemented and source-tested; no central host
runtime or additional harness support is implied. The source baseline is
aq-worker `08de5dc`, including the fixed explicit tool PATH.

## Why this boundary

`aq-worker::native::adapter::codex::CodexExecProfile` currently combines a
small closed Codex 0.166 command recipe with the remote preparation boundary.
A central supervisor needs the recipe without importing the worker's pull
protocol, material store, HTTP client, durable journal, lease ownership and
remote task bindings. CentralPlan references alone do not resolve selected
settings or supply launch authority.

The smallest reusable unit is a same-workspace `aq-codex-mapper` crate. It
depends on existing `aq-session-protocol` contracts plus `anyhow` and `toml`.
It has no worker/admission/host, HTTP, Tokio, provider material, startup bytes,
filesystem effects or process APIs. No repo, service, generic harness registry
or new external dependency is introduced.

## Source inventory and ownership

| Existing responsibility | Owner after extraction |
| --- | --- |
| Exact NativeOffer/retained artifact validation and canonical manifest digest | Existing worker wrapper |
| Selected harness/model/permission digest equality and installed adapter/renderer equality | Existing worker wrapper |
| Supported Codex version/provider/reasoning/capabilities and noninteractive flag | Pure mapper, called from the same wrapper boundary |
| Descriptor shape, renderer/module pin, exact payload/constituent digests and startup budget | Existing worker wrapper |
| Actual canonical workspace, private-home mode/UID/emptiness or planned nonexistence | Existing worker wrapper |
| Installed mutable/sealed/named image revalidation | Existing worker installation owners and unchanged prepare signatures |
| Closed argv, fixed tool PATH, private-home environment and disabled implicit config | Pure mapper |
| Numeric local Responses endpoint config overrides | Pure mapper from a closed endpoint value; existing worker string validation retained |
| Sensitive exact startup owner/write, remote manifest digest, auth derivation, MaterialHandoff/SourcePin, transport lifetime | Existing worker wrapper/launch/material owners |
| JSONL observations, physical image witness, tmux, permits, stop/ECHILD, paired ACK and recovery | Existing owners; not part of this extraction |

The permission digest comparison is an exact worker policy join, not a reason
to interpret remote permissions in the pure mapper. Likewise the core does
not claim its CLI flags enforce the model output cap, aggregate cost, private
credential isolation or mandatory resource limits. Those remain the concrete
transport/environment/host qualification obligations.

## Reusable narrow API

```rust
// Borrowed metadata only; no actor, session, offer, lease or permit.
pub struct Settings<'a> {
    pub harness: &'a HarnessSettings,
    pub model: &'a ModelSettings,
    pub terminal_input_enabled: bool,
}
pub struct Paths<'a> {
    pub executable: &'a Path,
    pub workspace: &'a Path,
    pub private_home: &'a Path,
}
pub struct Recipe {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}
pub fn validate_settings(settings: Settings<'_>) -> Result<()>;
pub fn map(settings: Settings<'_>, paths: Paths<'_>) -> Result<Recipe>;
pub struct LoopbackResponsesEndpoint { /* nonzero port, exact 127.0.0.1 */ }
impl LoopbackResponsesEndpoint {
    pub fn new(port: u16) -> Result<Self>;
}
pub fn responses_overrides(endpoint: LoopbackResponsesEndpoint) -> Vec<String>;
```

Paths are recipe metadata. Lexical/UTF-8 checks prevent malformed argv; they
are not installation, directory ownership or confinement claims. The worker
keeps its actual filesystem checks and creates `LocalCommand` from the three
recipe fields. Its `PreparedCodexExec` continues to own the exact sensitive
payload and remote manifest digest, with no Serialize/Debug or spawn method.

`scoped_origin` retains existing exact URL validation and canonicalization in
the worker. Only its already-validated nonzero port enters the pure endpoint
constructor. The pure helper emits the same ordered config strings; it never
binds, probes or authenticates an endpoint. The original scoped launch owner
continues to bind endpoint, capability, derived digest and authority. Recipe
mapping is not an independently qualified adapter installation.

## Compatibility and bounded validation

Keep `CodexExecProfile`, `CodexExecInput`, prepare/prepare_native/planned
signatures, `PreparedCodexExec` and public worker command paths unchanged.
No frozen remote wire, manifest, profile identity or package archive is
rewritten. Unsupported settings remain refused and all existing safety checks
remain required before returning prepared input.

Capture command/settings/environment goldens from the pre-extraction worker,
normalizing only the fixture temporary root, both for the base command and
the explicit scoped endpoint. After extraction, compare the wrapper's complete
serialized command bytes with those captured fixtures. Run focused mapping,
startup/settings/private-home tamper, authentication and event tests plus
strict affected checks. Do not launch Codex or providers. Keep production
admission refused; successful mapping never changes runtime capabilities.

The captured goldens and their provenance are in
[`codex/goldens`](../../crates/aq-worker/src/native/adapter/codex/goldens/README.md).
The extracted worker path matches both complete normalized commands byte for
byte, including all settings and environment values. Source-only checks:

- Pure mapper: three tests passed in 0.00s (1.99s initial compile).
- Worker: eleven focused Codex cases passed in 0.15s (14.32s compile), including
  both command goldens, closed origin refusals and retained private-home,
  payload/settings, image reuse, auth, JSONL and offline Python checks.
- Strict affected all-target/all-feature Clippy passed in 17.62s; formatting
  and diff checks passed. No Codex or provider process was launched.

A separate read-only source review found no concrete remote behavior or
authority regression. These checks establish source compatibility, not a new
package/environment qualification or central runtime readiness.

## Central integration remains a separate contract bridge

The pure central authority core uses a real CentralConversation, exact
SessionIdentity::Central generation, immutable profile/artifact refs, bounded
mail refs and structural permits. A future authenticated composition owner
must resolve those refs into exact shared settings/startup and a separately
versioned subject-neutral HostPlan. Existing LaunchManifest/LaunchBindings
still require real project/task/attempt fields; never fabricate them to call
the remote mapper. Central must retain its own startup/material/physical
ownership joins and qualify an installation/environment before execution.
Its future plan should pin the actual mapper artifact/version and environment
qualification. This extraction supplies recipe code only.
