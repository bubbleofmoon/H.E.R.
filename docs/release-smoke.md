# Desktop smoke check (every Windows release)

Record the candidate tag, Windows version, WebView2 Runtime version, installing
account, date and results in the release evidence. Use a disposable installation
and fake SnowLuma/DSH dependencies first. Production checks belong to the approved
release procedure.

For every app launch or reopen in a disposable installation, use
`<install>/current/her.exe app --run-store desktop-run.json`. Logon registration
then stays in `<install>/runtime/data/desktop-run.json`; omit `--register-logon`
so the UI opens. Flag contracts: `docs/agents/windows-lifecycle.md`.

- Open the app. Confirm a HER window renders the selected runtime's
  console in WebView2, signs in without copying a token, and opens no console
  window. Check the console still requires authentication in a separate browser.
- Open the tray menu. Confirm **Open**, **Restart** and the update-availability
  item are present. With unavailable release status, the update item is disabled
  and says status unavailable. With policy off it is disabled and says updates
  are disabled; with no newer release it says HER is up to date.
- With the fake dependencies healthy, confirm the tray reports healthy. Make a
  dependency unhealthy, wait for the next health poll and confirm the icon and
  tooltip show trouble. Restore it and confirm recovery.
- Configure a disposable SnowLuma WebUI endpoint and its selected session token
  file (see `docs/agents/windows-lifecycle.md`). Report a connected hook with
  `loggedIn:false,status:"loaded"` and OneBot `online:false`. Within 30 seconds,
  confirm the app banner/title and tray say **QQ needs login**. Verify the app's
  **Open QQ login window** button and matching tray action foreground the existing
  native QQ window for the SnowLuma-reported, verified PID, including a minimized
  window. Record PID/start/image before and after; nothing is started, stopped,
  injected or captured. Use only the approved disposable native QQ instance for
  this manual step; an HTTP fake alone does not prove native focus.
- Hide the HER window, keep the fake QQ offline and refuse private sends. At the
  next login poll, confirm a native Windows **QQ needs login** balloon appears.
  Keep polling through login-needed and unknown states: no repeated balloon.
  Restore confirmed login, then require login again: one new balloon. Record
  Windows notification settings and actual native display evidence; unit tests
  prove only the notification decision. Open HER and navigate/reload the console
  during login-needed: the banner and button remain after the document loads.
  Restore QQ send capability and confirm the pending private notice arrives once,
  including after restarting the supervisor while delivery was unavailable.
- Close that native window or report an exited/mismatched PID. Click each login
  action and confirm a visible **No verified QQ login window was found** message.
  Check another QQ process/window is left alone. If Windows refuses foreground
  activation, confirm the refusal is visible instead of claiming success.
- Return the fake to logged-in and confirm the banner/action clears. Disconnect
  the fake or expire the WebUI session and set OneBot offline/unknown: show login
  state unavailable, with no login-needed notice or new episode alert. Confirm
  only one administrator notice per episode across supervisor restarts, then
  confirmed login followed by login-needed produces one new notice. No QQ QR
  image, account ID or dependency credential appears in HER.
- Under notify, serve a fake release feed with a newer release. Confirm the app
  banner and enabled **Update available** tray item show the same current/new
  versions. Confirm the Schedule page selects the current update policy and retains
  policy edits after reload. Before approval, no candidate is staged or switched.
- On fresh disposable installations, approve through the console button, app
  banner button and tray item. Each action must use the displayed version and
  reach the existing fixed-helper trial/handover path with no foreground console.
  Observe current/last-good and dependency PID/start/image before and after.
  Change the offered version between display and click: require a visible
  refusal and refresh instead of silently installing the replacement. Revoke
  console credentials or make the console unavailable: tray refusal stays
  visible, and availability becomes disabled on the next failed poll.
- In the fake administrator's private QQ session, send `/update`: observe the
  current/new version reply before staging. Repeat with refused/lost sends:
  nothing stages. Groups and non-administrators receive a refusal. `/update deps`
  lists configured pins and says dependency upgrades are unavailable until that
  machinery exists. Under auto, keep a fake turn/reply in flight: no staging;
  after completion, observe the same trial/handover path. At the fixture's
  pre-start barrier, verify turn admission stays held after the old supervisor
  exits and until the helper finishes judging the replacement. These process checks
  complement portable automated coverage; record native tray rendering and
  hidden-window evidence manually.
- Against a throwaway installed root and fake dependencies, trial a staged
  release and switch using the fixed-root `herupdate.exe`. Record helper and old
  supervisor PID/start/image, the real current junction target, hidden startup,
  a passing post-switch assert-running, and last-good advancement. Repeat with
  trial failure (current unchanged), post-switch failure (rollback within the
  two-minute health deadline and a recorded administrator alert), and three
  unhealthy starts within ten minutes (fallback). Require identical SnowLuma and
  DSH PID/start/image across both success and rollback. Check pruning keeps the
  newest three releases plus protected current/last-good if they are older.
- Close the HER window. Confirm it disappears, the tray remains, the supervisor
  and bot continue running, and the console remains reachable. Use **Open** to
  restore the window; verify console navigation still works.
- Select **Restart** once. Confirm the console records the restart request and
  its result, the supervisor remains responsible for recovery, and the window
  reconnects. Check refusal is visible when the supervisor is unavailable.
- Quit only the desktop app from its tray menu. Confirm the bot and supervisor
  continue. Reopen the app and confirm sign-in and tray health recover.
- Verify startup commands with fake Scheduler/Run stores: the single elevated
  `HER` task starts the supervisor and the per-user desktop command starts the
  tray without elevation or a foreground console. Tests never register real
  tasks or Run keys. For an already installed, approved release, observe ordinary
  logon without changing its registrations and record the desktop process token.
- Revoke/rotate the console token or restart the console. Confirm old desktop
  sessions stop authenticating and reopening the window signs in again. Confirm
  the release evidence includes the automated different-user sign-in refusal
  fixture; do not create Windows accounts for the check.

Attach screenshots of the window, healthy/unhealthy tray tooltips and menu, plus
the restart result and process identities before/after closing the window.
Mark each item pass/fail/not run with its evidence; a checklist alone is not a
record of a smoke run.
