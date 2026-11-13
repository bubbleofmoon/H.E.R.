# HER runs its own harness; DSH becomes an upstream

Status: accepted; stage 1 build approved

HER will run its own harness in Rust, calling model APIs directly (the DeepSeek API, and llama.cpp on the Rig). DSH is retired once no conversation, job or console view needs it. From then on DSH is an upstream: HER takes what it needs from DSH's code (MIT) and ideas, and watches its releases for improvements, instead of running it.

The owner uses DSH only through HER. Much of the bridge's hardest code exists only because the agent runs in a separate process HER cannot see into: the login cookie and its clock-skew failures, matching DSH's event stream, proving whether a prompt ran, and installing, pinning and supervising DSH. Local models plug into a harness HER owns without another host in between. The research is on the `research/dsh-replace` branch (issue #64).

The safety rules stay whatever runs the loop: durable ingress, the send outbox, conversation generations, tool authorization, and never resending an uncertain reply.

Scope and order: every participation mode moves to the harness, the Admin agent last. Existing DSH histories are archived read-only, and each conversation starts on the harness with a summary handover; there is no importer. Each conversation runs on exactly one of the two at a time and can return to DSH until DSH is retired. The Session inspector (adapted from DSH's web UI) ships with the harness; skills and the code workspace hand follow; plugins come with the public phase.

## Considered Options

- **Keep DSH:** rejected. It keeps every DSH-only failure mode and the install and pin work, for features HER does not use.
- **Port all of DSH to Rust:** rejected. Its shell, file, plugin and account features would have to track every DSH release. HER takes only what it needs, and coding work goes to Codex CLI or Claude Code through a hand.
