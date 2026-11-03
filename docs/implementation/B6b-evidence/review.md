# Fresh whole-branch review and dispositions

A read-only gpt-6-astra reviewer examined `4373f02..a92b3a9` against the plan and task. It found two material issues; the author addressed both in the single test-first fix pass required by executing-plans. No second reviewer was dispatched.

- Critical: gzip-compressed upstream Access echoes bypass literal scanning. Reproduced with a real gzip credential echo in an HTTP 500. The proxy now requests identity encoding and rejects every nonidentity Content-Encoding before forwarding. The gzip test returns a generic empty 502. An additional RED/GREEN assertion verifies the identity request header.
- Important: agent UID could not read required startup context in the runner-private launch directory. The kernel harness now consumes and verifies `context-0.bin`. It failed before the fix, then passed after publishing only validated context to a separate immutable view under protected root-owned `/tmp`; permits, provider/Access keys and state/journal remain unreadable. No private directory permissions were widened.
- Extra coverage recommendation: the kernel agent also attempts a valid transition to the runner identity without capabilities and verifies refusal with its UID unchanged.
- Deferred minor coverage: dedicated wrong-owner secret-file and `Connection: Authorization` no-forwarding probes. The implementation already checks descriptor ownership and explicitly rejects that hop nomination.

## Boundary rulings

The review deliberately declined the following areas; the executor makes each disposition explicit:

1. Production B2/B6a2 HTTP transport installation remains with that owner; the CLI refuses until installed. Cost if the boundary is wrong: no production execution through this CLI yet.
2. C6a Codex configuration and C6b subscription authentication remain outside B6b. Basic required startup context delivery was fixed here. Cost: this evidence does not qualify a real Codex/subscription launch.
3. B7 owns agent credential issuance/lifecycle. The AQ proxy preserves caller Authorization. Cost: B7 must supply a valid agent credential.
4. C3a image accounts and E5 Access-token provisioning remain external inputs. Cost: missing accounts/files refuse startup.
5. Shared-kernel hostile-code isolation, namespaces and seccomp remain excluded. Cost: OS file/user separation must not be represented as a hostile-code boundary.
6. Final qualification was still running during review. The author owns all final checks and records their actual results below; pending or interrupted checks do not count as success. Cost: those results are author verification, not independent reruns by the reviewer.

Bootstrap retains exactly SETUID/SETGID/KILL for runner/guardian and clears all agent capabilities. Existing backend capability policies are unchanged; unsupported backends refuse. Cost if this mechanism is not acceptable to the owner: another cross-UID launch/stop mechanism is required before backend qualification.
