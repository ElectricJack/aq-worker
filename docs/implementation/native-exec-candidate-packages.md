# Native exec source-package candidates

Source snapshot: `1864d2ab9ec6db870730edc2fb376886e3609d67`. These are unpublished local candidate archives. Existing package archives are not rewritten.

| Component | Version | Archive SHA256 |
| --- | --- | --- |
| aq-session-host | 0.1.5 | `b47eeee1857e4656f31a6f0848bfc3258dff10f83d409fde0fee6c81cd17aacb` |
| aq-terminal-bridge | 0.1.2 | `77718f1244ba4ba5acd4706d628e064a7226a2d4465f30dd42e30e47254ef68c` |
| aq-worker | 0.3.4 | `276886e69ddc01f0aebccfbb1ae21fb4ace4b94244eb245196ee51d5d4949e9e` |

Artifacts and extracted package trees are retained under `/home/jkern/dev/.aq-qualification/native-exec-2026-11-01/1864d2ab9ec6`. The adjacent JSON records normalized exact AQ dependencies, all packaged-file byte counts and SHA256s, and local paths. Each source/test/README file and original Cargo manifest was compared byte-for-byte with the selected source checkout; extracted files were then checked against the archive manifest.

Changed dependency closure: workspace and bridge pin host `=0.1.5`; worker pins bridge `=0.1.2` and inherits host `=0.1.5`. No other lockfile package versions changed. Frozen protocol/admission/relay packages retain their versions and bytes.

Commands: `cargo metadata --offline --format-version 1`; then `cargo package -p <component> --no-verify --offline --exclude-lockfile --target-dir <candidate-local>/cargo-packages` with explicit exact local AQ source overrides for dependency resolution. The normalized distributable manifests contain version requirements rather than checkout paths. No registry publication or network dependency lookup occurred.

`--no-verify` is packaging-only evidence. Extracted builds/tests, target-host binaries, standalone executor behavior, provider/model support, runtime advertisement and task completion are **not qualified** by this stage. An extracted consumer must provide the exact AQ dependency packages; the packaging overrides are not a deployment dependency resolver.

The candidate is identified by source revision and archive digest. Later adapter/runner source changes require a newly identified candidate snapshot and repeated source/package checks before runtime acceptance. The native owners import the manifest commit before further candidate builds.
