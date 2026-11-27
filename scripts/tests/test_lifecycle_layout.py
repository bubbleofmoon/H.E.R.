#!/usr/bin/env python3
"""Verify scripts/lifecycle layout, run-script-tests.py inventory, and CI/repo path invariants.

Run: python -I scripts/tests/test_lifecycle_layout.py
"""
from __future__ import annotations

import ast
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "scripts"
LIFECYCLE = SCRIPTS / "lifecycle"


def parse_run_script_tests() -> tuple[str, set[str], set[str]]:
    source = (SCRIPTS / "ci" / "run-script-tests.py").read_text(encoding="utf-8")
    tree = ast.parse(source)
    run_names: set[str] = set()
    exempt_names: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            if node.targets[0].id == "RUN" and isinstance(node.value, ast.List):
                for elt in node.value.elts:
                    if isinstance(elt, ast.Tuple) and elt.elts and isinstance(elt.elts[0], ast.Constant):
                        run_names.add(str(elt.elts[0].value))
                    elif isinstance(elt, ast.Starred) and isinstance(elt.value, ast.ListComp):
                        comp_elt = elt.value.elt
                        if isinstance(comp_elt, ast.Tuple) and comp_elt.elts and isinstance(comp_elt.elts[0], ast.Constant):
                            run_names.add(str(comp_elt.elts[0].value))
            elif node.targets[0].id == "EXEMPT" and isinstance(node.value, ast.Dict):
                for key in node.value.keys:
                    if isinstance(key, ast.Constant):
                        exempt_names.add(str(key.value))
    return source, run_names, exempt_names


class LifecycleLayoutTests(unittest.TestCase):
    def test_no_standalone_test_harnesses_in_scripts_root(self):
        strays = sorted(p.name for p in SCRIPTS.glob("test-*") if p.is_file())
        self.assertEqual(strays, [], f"standalone test harnesses must live under scripts/lifecycle/: {strays}")

    def test_run_script_tests_matches_lifecycle_dir(self):
        source, run_names, exempt_names = parse_run_script_tests()
        self.assertIn("root / 'scripts' / 'lifecycle'", source)
        self.assertEqual(run_names & exempt_names, set())
        on_disk = {p.name for p in LIFECYCLE.glob("test-*.py") if p.is_file()}
        self.assertTrue(on_disk, "expected test-*.py harnesses under scripts/lifecycle/")
        self.assertEqual(on_disk - (run_names | exempt_names), set())
        self.assertEqual((run_names | exempt_names) - on_disk, set())

    def test_checks_workflow_and_rust_tests_reference_lifecycle_paths(self):
        checks = (ROOT / ".github" / "workflows" / "checks.yml").read_text(encoding="utf-8")
        self.assertIn("scripts/lifecycle/test-ps-count.ps1", checks)
        self.assertIn("scripts/lifecycle/test-external-path.ps1", checks)

        install_root_rs = (ROOT / "her" / "tests" / "install_root.rs").read_text(encoding="utf-8")
        self.assertNotIn('"../scripts"', install_root_rs)
        self.assertIn('"../scripts/lifecycle"', install_root_rs)
        self.assertIn('"scripts/lifecycle"', install_root_rs)

        sup_deps_rs = (ROOT / "her" / "tests" / "supervisor_dependencies.rs").read_text(encoding="utf-8")
        self.assertIn("scripts/lifecycle/test-ps-count.ps1", sup_deps_rs)

    def test_moved_scripts_resolve_repo_root_and_shared_helpers(self):
        for rel in (
            "fixture_paths.py",
            "test-candidate-build.py",
            "test-lifecycle-fixture-encoding.py",
            "test-lifecycle-fixture-pinning.py",
            "test-release-chains.py",
            "test-release-proof.py",
            "test-stop-entry.py",
        ):
            text = (LIFECYCLE / rel).read_text(encoding="utf-8")
            self.assertNotRegex(
                text,
                r"Path\(__file__\)\.resolve\(\)\.parent\.parent\b",
                f"{rel} still uses 2-level parent for repo root",
            )
            self.assertIn("parents[2]", text, f"{rel} should resolve repo root via parents[2]")

        extra_chains = (LIFECYCLE / "lifecycle-extra-chains.mjs").read_text(encoding="utf-8")
        self.assertIn("from '../fixture-paths.mjs'", extra_chains)

        ps_count = (LIFECYCLE / "test-ps-count.ps1").read_text(encoding="utf-8")
        self.assertIn("Join-Path $PSScriptRoot '..\\..'", ps_count)

        ext_path = (LIFECYCLE / "test-external-path.ps1").read_text(encoding="utf-8")
        self.assertIn("Join-Path $PSScriptRoot '..\\..\\external-path.ps1'", ext_path)

        prod_move = (LIFECYCLE / "test-production-move.ps1").read_text(encoding="utf-8")
        self.assertIn("Join-Path $PSScriptRoot '..\\production-move.ps1'", prod_move)

        fixture_ps1 = (LIFECYCLE / "test-lifecycle-fixture.ps1").read_text(encoding="utf-8")
        self.assertIn("Split-Path -Parent (Split-Path -Parent $PSScriptRoot)", fixture_ps1)
        self.assertNotIn("scripts\\lifecycle-window-observer.ps1", fixture_ps1)
        self.assertNotIn("scripts\\fixture-paths.ps1", fixture_ps1)
        self.assertNotIn("scripts\\lifecycle-fake-resident.mjs", fixture_ps1)
        self.assertNotIn("scripts\\lifecycle-curl-adapter.cs", fixture_ps1)
        self.assertNotIn("scripts\\lifecycle-extra-chains.mjs", fixture_ps1)
        for name in (
            "lifecycle-window-observer.ps1",
            "fixture-paths.ps1",
            "lifecycle-fake-resident.mjs",
            "lifecycle-curl-adapter.cs",
            "lifecycle-extra-chains.mjs",
        ):
            self.assertTrue((LIFECYCLE / name).is_file())
            self.assertIn(f"scripts\\lifecycle\\{name}", fixture_ps1)

        # scripts/fixture-paths.mjs stays in scripts/ because her/tests/fixtures/*.mjs import it.
        self.assertTrue((SCRIPTS / "fixture-paths.mjs").is_file())
        self.assertTrue((SCRIPTS / "production-move.ps1").is_file())
        self.assertFalse((LIFECYCLE / "fixture-paths.mjs").exists())
        self.assertFalse((LIFECYCLE / "production-move.ps1").exists())
        self.assertEqual(re.findall(r"scripts/test-", (ROOT / "docs" / "agents" / "rust-validation.md").read_text(encoding="utf-8")), [])


if __name__ == "__main__":
    unittest.main()
