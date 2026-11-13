# Self-update by trial handover, judged by a short-lived updater

Status: accepted

The supervisor updates itself from GitHub releases with no long-running updater and no second scheduled task:
1. It downloads and verifies the new release into its own folder.
2. It runs the new release's console image as a read-only trial, using the shared `assert-running` check against the current release's verified HER owners. The trial takes no owner gate and dispatches no turns or jobs.
3. Once the new release passes the shared health check, the supervisor hands over to `herupdate.exe`, a small helper that runs only during an update, from a fixed folder outside every release.
4. `herupdate.exe` waits for the old supervisor to exit, switches the `current` link and judges the new supervisor. If the new one is not healthy within 2 minutes, it switches back and alerts the administrator.

Dependencies (SnowLuma, DSH) keep running throughout and are adopted by identity. They never update automatically: their versions are pinned, and an upgrade happens only on the administrator's approval, with automatic rollback.

The updater invokes the candidate's CLI for retirement so the previous release need not implement the handover protocol. Retirement verifies PID/start/image, retires the old supervisor before its live/console owners, and leaves desired state running. Trial-only health authority does not authorize process control.

## Considered Options

- **An always-running updater with its own task, like Chrome's Omaha:** rejected. It is a second long-lived process and a second task.
- **A permanent launcher in front of the supervisor:** rejected. It is a fixed process in the start path for every boot.
- **The supervisor switching on its own, without a helper:** rejected. It leaves no older process to roll back a release that crashes after the switch.
