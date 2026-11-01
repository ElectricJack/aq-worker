# Pre-extraction worker command goldens

These complete `LocalCommand` JSON fixtures were captured from the worker
mapper at source baseline `08de5dc`, before recipe code moved into
`aq-codex-mapper`. A temporary unit-test capture invoked the existing real
`CodexExecProfile::prepare` boundary with its validated synthetic NativeOffer
and private-mode fixture directories. No Codex/provider process ran. Only the
random fixture temporary-root prefix was normalized to `<fixture>`; the
executable, argument/config order, values and environment were retained.

`base.json` is the ordinary recipe. `scoped.json` additionally calls the
existing `with_scoped_provider("http://127.0.0.1:32123")` before serialization.
The permanent tests compare full normalized JSON bytes, not selected flags.
The capture-only test was removed after producing these fixtures.

| Fixture | Bytes | SHA256 |
| --- | ---: | --- |
| base.json | 2120 | `76a1c26df295fbf0e6af2b7c9d202d18a2d292c06a15dc8ff7e42b0491e808c8` |
| scoped.json | 2571 | `ff74428307e337b83b43a3e4deb27bebc61a574005a773259dfa8fb47ec979a5` |

They contain synthetic model metadata and fixture paths, no startup payload,
provider key, local capability or live authority. Mapping success does not
qualify a host or establish central-supervisor execution readiness.
