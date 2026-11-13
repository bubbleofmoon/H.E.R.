# One contract test may start a real, local DSH

Status: accepted

Tests never contact real DSH, with one exception. The DSH credential contract test starts the pinned DSH version locally, with a throwaway home, a random loopback port and no model key, and checks that a cookie minted by the bridge is accepted and that one dated in the future is rejected. It runs in CI and gates every DSH version change. Fake verifiers alone cannot catch a DSH rule nobody knows about, such as its clock-skew check.
