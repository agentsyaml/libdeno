import importlib.util
from datetime import date
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("audit-with-temporary-exceptions.py")
SPEC = importlib.util.spec_from_file_location("audit_exceptions", SCRIPT)
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)

RUSTLS, CRYPTO_BIGINT = AUDIT.EXCEPTIONS


class AuditExceptionTests(unittest.TestCase):
    def audit_lock(self, packages, today):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        lock = Path(tmp.name) / "Cargo.lock"
        lock.write_text(
            "".join(
                f'[[package]]\nname = "{name}"\nversion = "{version}"\n\n'
                for name, version in packages
            ),
            encoding="utf-8",
        )
        return lock

    def ignored(self, run):
        command = run.call_args.args[0]
        self.assertIn("--deny", command)
        self.assertIn("warnings", command)
        return [command[i + 1] for i, value in enumerate(command[:-1]) if value == "--ignore"]

    def test_exception_is_limited_to_the_pinned_versions(self):
        lock = self.audit_lock(
            [("rustls", "0.23.40"), ("crypto-bigint", "0.5.5"), ("crypto-bigint", "0.7.8")],
            date(2026, 11, 16),
        )
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2026, 11, 16)), 0)
        self.assertEqual(
            self.ignored(run), list(RUSTLS.advisories) + list(CRYPTO_BIGINT.advisories)
        )

    def test_only_the_older_crypto_bigint_pins_an_exception(self):
        lock = self.audit_lock([("crypto-bigint", "0.7.8")], date(2026, 11, 16))
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 17)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2026, 11, 16)), 17)
        self.assertEqual(self.ignored(run), [])

    def test_each_exception_expires_on_its_own_date(self):
        for exception in (RUSTLS, CRYPTO_BIGINT):
            lock = self.audit_lock(
                [(exception.package, exception.version)], exception.expires
            )
            with patch.object(AUDIT.subprocess, "run") as run:
                self.assertEqual(AUDIT.audit(lock, exception.expires), 1)
                run.assert_not_called()

    def test_upgraded_versions_need_no_exception_even_after_expiry(self):
        lock = self.audit_lock(
            [("rustls", "0.23.46"), ("crypto-bigint", "0.7.8")], date(2027, 12, 31)
        )
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 17)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2027, 12, 31)), 17)
        self.assertEqual(self.ignored(run), [])


if __name__ == "__main__":
    unittest.main()