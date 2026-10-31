# WRK-14 dense output continuation

Source `de434393f4fcd46c2a206f81b97dfd943f398a7f` follows the separately frozen
reader checkpoint `cfe2c33`/`26a852b`. The original negative output source and
packet-limit evidence remain unchanged. No frozen public contract/archive is
modified. Candidate host0.1.2/bridge0.1.1 remain unpublished and unadopted.

The strict `ControlDecoder::feed` API still refuses over64events. Streaming
`feed_bounded` reports the exact consumed prefix and at most64events, leaving
the caller responsible for the untouched suffix. OutputObserver retains at most
one64KiB packet and exhausts it before reading again. It cannot submit a drained
lease receipt until that packet is exhausted and the actual pipe reaches
EAGAIN. No byte is invented, skipped or duplicated. Absolute256KiB line and
64KiB decoded-event bounds still refuse; malformed/truncated input permanently
poisons continuation. No renderer or snapshot follows from this byte parser.

Targeted normal host/bridge tests passed33tests with34ignored. After that frozen
run, only ignored crash-fixture diagnostics changed; production hashes stayed
identical. Final actual host suite passed30/30 in9.22s, and actual bridge suite
passed3/3 in0.59s, including the unchanged queue-overflow regression that
previously failed. Final strict workspace all-target/all-feature Clippy and
formatting passed. The earlier reader checkpoint's full workspace303pass applies
to that exact source, not a fabricated full-workspace rerun here.

Dense pure tests preserve1000Unicode/ANSI events across every byte/escape boundary
and bounded packet continuations, and retain old strict-cap/oversized-line/
decoded-byte/EOF refusals. The actual queue test enforces both64events and64KiB
retained wire per poll,8192retained tail bytes and exact eviction gaps. A slow
consumer produced four data-bearing polls with a retained raw packet and was
independently cut off after1.036s; its harness continued beyond4MiB. The stopped
sole-client case recorded20,709,376bytes of subsequent harness progress. These
are finite local synthetic proofs, not a scale or hard scheduling guarantee.

Two earlier actual suite attempts each passed29/30 but timed out in different
pre-existing crash-boundary fixtures waiting for their marker. The guardian case
then passed focused, and the final diagnostic suite passed all30. Their precise
historical cause is unresolved; passing retries do not erase the failures. The
old subprocess had discarded `launch(1500)` errors and suppressed stderr, so a
pre-checkpoint launch refusal could appear as a generic5second marker timeout.
It now persistently records the requested stage, unchanged1500ms permit, elapsed
launch time and Result; a missing marker preserves its private fixture directory
and reports that Result. No launch TTL, test wait, retry policy or test
parallelism was widened. Both failed logs and focused/final results are retained.
There is no full runtime-adoption qualification until startup failure causes and
integration bounds are reviewed independently.

The next scope is an explicitly named bounded renderer over ordered Bytes/Resize,
with complete parser/buffer state, exact codec and width profile. Actual blank
baseline before harness exec and a proved resize sequencer remain required before
minting a shared Snapshot. capture_text, layout notices or quiet output sequence
cannot substitute for that proof or fresh unknown-input capture authority.

`WRK-14-dense-output.json` hashes the retained evidence in
`evidence/wrk14-dense-output`, including pre-diagnostic frozen and exact final
source inventories. Environment remains the shared WSL2/tmux3.4 host and two test
threads described in prior reader evidence; no providers/deployment were used.
