"""In-process output capture tests.

Capture redirects the process-global CRT fds, which Rust std's stdout bypasses
on Windows, so the capturing cases are skipped there.
"""

import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import libdeno

CAPTURE_UNSUPPORTED = sys.platform == "win32"
capture_only = unittest.skipIf(
    CAPTURE_UNSUPPORTED, "in-process output capture is not supported on Windows"
)


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = TemporaryDirectory()
        self.root = Path(self.tempdir.name)

    def tearDown(self):
        self.tempdir.cleanup()

    def write(self, relative, source):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")
        return path

    @capture_only
    def test_run_with_output_captures_stdout(self):
        entry = self.write("print.js", "console.log('hello stdout');")
        output = libdeno.run_with_output(entry, allow_all_permissions=True)

        self.assertEqual(output.exit_code, 0)
        self.assertIn(b"hello stdout", output.stdout)
        self.assertEqual(output.stderr, b"")
        self.assertFalse(output.capture_truncated)

    @capture_only
    def test_run_with_output_captures_stderr_separately(self):
        entry = self.write("print-stderr.js", "console.error('only stderr');")
        output = libdeno.run_with_output(entry, allow_all_permissions=True)

        self.assertEqual(output.stdout, b"")
        self.assertIn(b"only stderr", output.stderr)

    def test_run_with_output_capture_flags_can_be_disabled(self):
        # No capture requested, so this path is allowed on Windows too.
        entry = self.write(
            "both-streams.js",
            "console.log('out'); console.error('err');",
        )
        output = libdeno.run_with_output(
            entry,
            allow_all_permissions=True,
            capture_stdout=False,
            capture_stderr=False,
        )

        self.assertEqual(output.exit_code, 0)
        self.assertEqual(output.stdout, b"")
        self.assertEqual(output.stderr, b"")

    @capture_only
    def test_max_capture_bytes_truncates_and_sets_flag(self):
        entry = self.write("chatty.js", "console.log('x'.repeat(65536));")
        output = libdeno.run_with_output(
            entry, allow_all_permissions=True, max_capture_bytes=64
        )

        self.assertTrue(output.capture_truncated)
        self.assertLessEqual(len(output.stdout), 64)

    @capture_only
    def test_runtime_source_and_output_entries(self):
        runtime = libdeno.Runtime(self.root)
        entry = self.write("runtime-print.js", "console.log('from runtime');")
        output = runtime.run_with_output(entry, allow_all_permissions=True)
        self.assertEqual(output.exit_code, 0)
        self.assertIn(b"from runtime", output.stdout)

        source_output = runtime.run_source_with_output(
            "console.log('from source');", allow_all_permissions=True
        )
        self.assertIn(b"from source", source_output.stdout)


if __name__ == "__main__":
    unittest.main()
