"""Option validation ordering: invalid options win over a missing entry."""

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import libdeno


class OptionValidationTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = TemporaryDirectory()
        self.root = Path(self.tempdir.name)

    def tearDown(self):
        self.tempdir.cleanup()

    def bindings(self):
        runtime = libdeno.Runtime(self.root)
        return (libdeno.run, libdeno.run_with_output, runtime.run, runtime.run_with_output)

    def test_invalid_options_are_reported_before_a_missing_entry(self):
        # The Rust API validates resource limits before resolving the entry, so
        # a missing entry must not mask an invalid limit.
        missing = self.root / "missing-entry.js"
        for binding in self.bindings():
            with self.subTest(binding=binding, option="max_heap_bytes"):
                with self.assertRaises(libdeno.ConfigurationError):
                    binding(missing, allow_all_permissions=True, max_heap_bytes=1)
            with self.subTest(binding=binding, option="execution_deadline"):
                with self.assertRaises(libdeno.ConfigurationError):
                    # 1e300 seconds exceeds Duration's range on every platform;
                    # a near-u64 value is representable on Windows, whose
                    # Instant starts from a smaller epoch.
                    binding(
                        missing,
                        allow_all_permissions=True,
                        execution_deadline=1e300,
                    )

    def test_valid_options_keep_the_missing_entry_error(self):
        missing = self.root / "missing-entry.js"
        for binding in self.bindings():
            with self.subTest(binding=binding):
                with self.assertRaises(libdeno.EntryError):
                    binding(missing, allow_all_permissions=True, max_heap_bytes=8 << 20)


if __name__ == "__main__":
    unittest.main()
