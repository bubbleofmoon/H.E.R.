# Coding standards

Read by reviewers and merge gates; each rule is a judgement call no automated check covers. Report a violation as a required finding with file:line.

## Outbound chat text

- Text reaches a QQ conversation from exactly two sources: the model's own send tools (`qq_send_*`), and a backend answer the code has classified as a successful reply. Raw process output, backend or provider error text, refusals, exception messages and log lines go to the owner's private chat as an administrator alert, or to a log, never to a group or a non-owner private chat. A provider refusal printed with exit 0 is still an error, not a reply.
- A new or changed backend, provider or CLI route has seam tests for its failure modes as well as its answer: refusal or empty answer, quota/429, 5xx, timeout, and an error printed with exit 0. Each failure sends nothing to the conversation.

## Model-facing text

- Chat text that reaches a model (prompts, tool results, extraction inputs) goes through `model_view` filtering (`socialV2.context.stripFromModelText` and the model-text blacklist). Stored data, logs, console views and outgoing messages keep the original chat text relative to model-view filtering. Diagnostic and harness model-text record copies still apply session-token masking (`docs/agents/dependency-recovery.md`, `docs/agents/harness.md`).
