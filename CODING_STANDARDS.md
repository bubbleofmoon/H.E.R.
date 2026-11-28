# Coding standards

Read by reviewers and merge gates; each rule is a judgement call no automated check covers. Report a violation as a required finding with file:line.

## Outbound chat text

- The bridge sends model text to a QQ conversation through the model's own send tools (`qq_send_*`) or as a backend answer classified as successful. An eligible turn that exhausts its backend routes may also attempt one literal failure line the owner configured on its active persona. The same outbound policy and durable send guards apply. Raw process output, backend or provider error text, refusals, exception messages and log lines go to the owner's private chat as an administrator alert, or to a log, never to a group or a non-owner private chat. A provider refusal printed with exit 0 is still an error, not a reply.
- A new or changed backend, provider or CLI route has seam tests for its failure modes as well as its answer: refusal or empty answer, quota/429, 5xx, timeout, and an error printed with exit 0. Assert silence for failures by default and cover the owner-authored persona line wherever the turn can exhaust its routes.

## Model-facing text

- Chat text that reaches a model (prompts, tool results, extraction inputs) goes through `model_view` filtering (`socialV2.context.stripFromModelText` and the model-text blacklist). Stored data, logs, console views and outgoing messages keep the original chat text relative to model-view filtering. Diagnostic and harness model-text record copies still apply session-token masking (`docs/agents/dependency-recovery.md`, `docs/agents/harness.md`).
- Every model-bound text rule (filtering, masking, length caps, blacklists) holds for the final delivered copy. A review follows the text from its source through each transform that follows (truncation, joining, escaping, templating, re-encoding) to the last string handed to the model and checks that copy; a rule applied only before a later transform is a required finding.

## Concurrency

- A new lock states its acquisition order against every existing lock it can be held with, in a comment where it is defined, and each path takes the locks in that order. Nothing waits while holding the store-style gate (the lock that serialises a store's reads and writes): no await, channel receive, other lock, sleep or I/O to another store. A path that can take two locks in opposite orders, or waits under the gate, is a required finding.
- When a CLI command or route acquires a non-blocking supervisor gate (`launch_gate` or `installation_gate`), verify how the background supervisor loop handles contention while the gate is held; unhandled contention that publishes a false refusal or alert is a required finding.

## Comparisons

- A canonical form used for equality or change detection treats a missing value, null and empty alike, on both sides of the comparison and on every path that builds it (stored, incoming, default). A comparison where one side maps missing to empty and the other does not is a required finding.

## Tests

- A new test's red receipt must show it failing for the reason the ticket names (the assertion or panic message, not a compile error or an unrelated failure); a test that could pass without the fix is a required finding.
