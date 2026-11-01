"""The Rust fixes must reach the Python binding, not just the Rust crate.

Each case here mirrors a documented Rust behavior that changed: a `cwd` that
does not resolve is a configuration error, and `max_capture_bytes` is a byte
budget (so a truncation can split a multi-byte character).
"""

import os
import tempfile
import unittest
from pathlib import Path

import libdeno


class CwdValidationTest(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.entry = self.root / "main.js"
        self.entry.write_text("console.log(Deno.cwd());", encoding="utf-8")

    def tearDown(self):
        self._dir.cleanup()

    def test_missing_cwd_is_a_configuration_error(self):
        ghost = self.root / "does-not-exist"
        with self.assertRaises(libdeno.ConfigurationError) as caught:
            libdeno.run(self.entry, allow_all_permissions=True, cwd=ghost)
        self.assertIn("is not a usable directory", str(caught.exception))

    def test_file_as_cwd_is_a_configuration_error(self):
        afile = self.root / "afile"
        afile.write_text("x", encoding="utf-8")
        with self.assertRaises(libdeno.ConfigurationError) as caught:
            libdeno.run(self.entry, allow_all_permissions=True, cwd=afile)
        self.assertIn("is not a directory", str(caught.exception))

    def test_existing_cwd_still_runs(self):
        sub = self.root / "sub"
        sub.mkdir()
        self.assertEqual(
            libdeno.run(self.entry, allow_all_permissions=True, cwd=sub),
            0,
        )


class CaptureByteBudgetTest(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)

    def tearDown(self):
        self._dir.cleanup()

    def test_the_cap_counts_bytes_not_characters(self):
        entry = self.root / "emoji.js"
        entry.write_text("console.log('\U0001f600\U0001f600');", encoding="utf-8")
        # Each emoji is 4 bytes; 5 keeps one whole emoji plus one stray byte.
        output = libdeno.run_with_output(
            entry,
            allow_all_permissions=True,
            max_capture_bytes=5,
        )
        self.assertEqual(len(output.stdout), 5)
        self.assertTrue(output.capture_truncated)
        # The tail is a partial character, so strict decoding must fail: the
        # limit is a byte budget, not a character budget.
        with self.assertRaises(UnicodeDecodeError):
            output.stdout.decode("utf-8", errors="strict")
        self.assertTrue(output.stdout.decode("utf-8", errors="replace").startswith("\U0001f600"))

    def test_a_zero_cap_keeps_nothing(self):
        entry = self.root / "hello.js"
        entry.write_text("console.log('hello');", encoding="utf-8")
        output = libdeno.run_with_output(
            entry,
            allow_all_permissions=True,
            max_capture_bytes=0,
        )
        self.assertEqual(output.stdout, b"")
        self.assertTrue(output.capture_truncated)


if __name__ == "__main__":
    unittest.main()
