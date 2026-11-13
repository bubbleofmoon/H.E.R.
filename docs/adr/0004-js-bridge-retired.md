# The JS bridge is retired with no way back

Status: accepted

Production runs the Rust bridge, and the JS bridge (`qq-bridge/`) is retired permanently, effective immediately, with no rollback window. All code that exists only to move between JS and Rust is deleted. That covers:
- the cutover scripts,
- the projection of Rust state into the JS layout,
- the legacy launch fences,
- the import of JS config.

The architecture candidates "legacy-fence module" and "cutover as a Rust state machine" are therefore dropped, not deferred.

Memory is exempt from this deletion and stays untouched.

## Considered Options

- **A 14-day rollback window after the layout switchover:** rejected. Keeping both bridges working costs more than an unverified JS fallback is worth.
