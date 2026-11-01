# Bounded overlap for original inline downloads

InputPipeline::artifacts uses the native client's existing two bulk slots to
read at most two original artifacts concurrently. Each artifact's chunks remain
ordered; returned inputs preserve the original plan order. A pair uses structured
futures with no detached download tasks. Cancellation or either failure drops the
remaining local read future, commits no partial completion set and preserves all
original durable request IDs for explicit retry.

One pipeline bulk owner still covers the entire batch, including the unchanged
one plan and one completion publication. The existing total8MiB verified set,
per-chunk bound, HTTP slots, deadlines and wire0.1.7 are unchanged. A second read
can use buffer capacity already admitted by the native client's two-slot bound;
no additional concurrency setting or dependency was added. Remote read-only
handlers may complete after client cancellation under their own server bounds.
This is not a claim that a canceled request has no server-side activity.

A deterministic HTTP barrier proves two reads enter before either can finish.
With both bulk slots held, an authenticated exact Accept replay succeeds on the
independent control lane without extending its previous deadline. No artifact
completion receipt exists before release, the peer observes a maximum of two
reads, and the whole set still commits in exactly two journal publications.
Existing partial-network-failure, stable-ID retry and eight fsync-cut tests pass.
A separately enabled actual-Git case still downloads, imports, hydrates and joins
actual prepared inputs. Synthetic runtime/provider qualifications remain explicit.

This removes avoidable serialization. It does not fix the original2second
proposal/preparation coupling, certify a cold repository size or establish a
latency/throughput benchmark. The separately versioned accepted-preparation
protocol is still required for practical cold workspaces.
