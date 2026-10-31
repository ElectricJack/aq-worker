# Native source API1 consumer

The client consumes the exact worker0.2 TransferManifest through the native
current-credential API. It never uses the fixture transport's source routes.
GET `/v1/worker/profiled/source/{attempt}` returns a bounded manifest; GET
`.../{attempt}/{manifest|bundle}?offset=...&limit=...` returns one bounded range.
Every request and successful response requires `aq-source-api-version: 1`;
responses require `no-store`. Chunk replies repeat the exact manifest in the
base64url-no-padding `aq-source-transfer` header and the exact `aq-source-offset`.

The runtime supplies the SourceAdmission from its verified retained execution
spec. Status validates the exact attempt/session/generation/spec digest, base
role, repository/binding generation and resolved SHA1 object. The first manifest
must be journaled; later status reads may not change any field. Each chunk pins
that entire manifest, original offset and exact remaining range length. The
member enum admits only manifest or bundle. Unknown versions, changed metadata,
wrong offsets, short/oversized bodies, redirects and unsupported source modes
refuse. Existing TLS/proxy/retry policy applies. Reads consume the bounded bulk
lane and cannot consume the reserved renewal lane.

The initial source profile requires positive bundle bytes and
`transfer_id == artifact_id == export_operation_id`. Generic worker0.2
reference-only/zero-bundle transfers remain unsupported by this native profile.
No bundle is assembled into the inline artifact buffer. The actual source owner
must stream to its private files, verify whole manifest and bundle digests,
validate the imported repo/generation/base and obtain the real confined workspace.
Only after that may the driver create `aq.source-manifest/{export_operation_id}@1`
and `aq.source-bundle/{export_operation_id}@1` preparation receipts. The producer
reserves two slots under Prepare's128-artifact bound (at most126 inline inputs).
This client does not emit those receipts or infer hydration from downloaded bytes.

Three loopback HTTP tests exercise exact status/replay/ranges and malformed
metadata/versions/offsets/body sizes/redirects, using synthetic source bytes.
They are transport evidence, not actual Git import or native producer integration.
The actual daemon route/migration qualification is retained independently in AQ.
