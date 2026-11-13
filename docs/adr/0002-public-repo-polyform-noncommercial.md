# Publish as a fresh public repo under PolyForm Noncommercial 1.0.0

Status: accepted, amended

The bridge is published publicly so that the supervisor's self-update can fetch GitHub releases without a stored token, and so that CI runs on GitHub's standard hosted runners at no cost. It is published as a new repo with fresh history built from a cleaned snapshot (code, scripts, docs and CI only). After the cutover, the public repo is the development repo: code, docs, agent docs, CI, tickets, batons and handoffs live there, and every merge runs CI on the hosted runners. The private repo becomes a vault that receives no code: evidence, `.scratch/`, bot memory, the patched copy of SnowLuma's non-commercial code and production incident detail stay there, because its history and `.scratch/` hold real session and chat samples. Nothing moves from the vault to the public repo. Public commits and comments pass the public scan (secrets, numbers of 7 or more digits, user paths, chat text) before they leave the machine; a public ticket links vault evidence by path only.

The names are fixed: development is `bubbleofmoon/H.E.R.`, the vault is `bubbleofmoon/H.E.R.-vault`, and the vault checkout on the Windows host is `C:/her-vault`. The integration checkout stays `C:/her`, so its `scripts/cargo-slot.ps1` remains the central build-slot script.

The license is PolyForm Noncommercial 1.0.0 with a `NOTICE` carrying the Required Notice. Commercial use is forbidden, and anyone who redistributes the code must keep the attribution.

## Considered Options

- **GPL v3:** rejected. It must allow commercial use, so it cannot forbid monetisation.
- **CC BY-NC 4.0:** rejected. Creative Commons advises against using it for software.
- **Making the existing repo public:** rejected. It would expose the material above, and rewriting the history is error-prone.

## Consequences

- The code is source-available, not OSI open source.
- Hosted-runner CI is the merge check once it is green; local gates keep the native, elevated and production checks the hosted runners cannot run.
- A public repo never gets a self-hosted runner, and pull requests from forks need approval before their workflows run.
- People who only run it privately cannot be required to credit the author; only redistribution can be.
