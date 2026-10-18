import importlib.util
from datetime import date
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("audit-with-temporary-rustls-exceptions.py")
SPEC = importlib.util.spec_from_file_location("audit_exceptions", SCRIPT)
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)


class AuditExceptionTests(unittest.TestCase):
    def run_audit(self, version, today):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        lock = Path(tmp.name) / "Cargo.lock"
        lock.write_text(
            f'[[package]]\nname = "rustls"\nversion = "{version}"\n',
            encoding="utf-8",
        )
        return lock, today

    def test_exception_is_limited_to_three_ids_before_expiry(self):
        lock, today = self.run_audit("0.23.40", date(2026, 11, 16))
        result = subprocess.CompletedProcess([], 0)
        with patch.object(AUDIT.subprocess, "run", return_value=result) as run:
            self.assertEqual(AUDIT.audit(lock, today), 0)
        command = run.call_args.args[0]
        self.assertIn("--deny", command)
        self.assertIn("warnings", command)
        self.assertEqual(
            [command[i + 1] for i, value in enumerate(command[:-1]) if value == "--ignore"],
            list(AUDIT.ADVISORIES),
        )

    def test_exception_expires_on_and_after_expiry(self):
        lock, _ = self.run_audit("0.23.40", date(2026, 11, 16))
        for today in (date(2026, 11, 17), date(2026, 11, 18)):
            with patch.object(AUDIT.subprocess, "run") as run:
                self.assertEqual(AUDIT.audit(lock, today), 1)
                run.assert_not_called()

    def test_upgraded_rustls_needs_no_exception_even_after_expiry(self):
        lock, today = self.run_audit("0.23.45", date(2026, 11, 18))
        with patch.object(
            AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 17)
        ) as run:
            self.assertEqual(AUDIT.audit(lock, today), 17)
        command = run.call_args.args[0]
        self.assertNotIn("--ignore", command)


if __name__ == "__main__":
    unittest.main()
