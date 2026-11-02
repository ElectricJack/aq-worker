# Refresh output relay source qualification

Source `3146cf462093be887864a492f86a0ff5ee0ca97d` fixes the frozen-only Started lookup exposed by the first explicit refresh static case. That case reached actual Running and Stopped/ECHILD, then output attachment refused; it did not establish ACK/Watch/output qualification. Its result remains separate evidence.

The existing relay now privately retains either the frozen or refresh Started request. Attachment checks the matching actual historical execution context and original Running/session. Registration rereads that exact nominal branch, compares the retained request digest and validates its matching typed ACK before borrowing the common original receipt for the unchanged Register. Mixed branches, changed context, substituted Running/request and wrong ACK refuse. Pending ACK allows local draining and continues to refuse registration. The public API, relay instance, tail/sequence ownership, packet/register wire, quotas and network lanes are unchanged. No disk-format, package-version or contract-payload changes occur.

Ten hostless tests passed in1.02s after17.47s compilation: four profile/context/ACK cases plus six existing finite tail, loss/retry, high-byte packet and owner-drop cases. All-feature library/test strict Clippy passed in11.65s; owned formatting and diff checks passed. The JSON record pins exact commands and log digests. Existing physical duplicate-attachment and sequence tests remain unchanged; this source gate did not execute them or launch a harness, tmux, Git or provider.

Runner rebuilt and pinned a smoke-only binary separately from clean source3146cf4 after this source gate. An actual changed static case requires Root's separate authorization and qualification; this document does not claim that outcome.
