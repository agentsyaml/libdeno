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

EXCEPTIONS = AUDIT.EXCEPTIONS


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
            [
                ("rustls", "0.23.40"),
                ("crypto-bigint", "0.5.5"),
                ("crypto-bigint", "0.7.8"),
                ("zstd", "0.13.2"), ("zstd", "0.14.3"),
                ("zstd-safe", "7.2.4"), ("zstd-safe", "8.2.0"),
                ("zstd-sys", "2.0.16+zstd.1.5.7"),
                ("zstd-sys", "2.1.2"),
            ],
            date(2026, 11, 16),
        )
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2026, 11, 16)), 0)
        self.assertEqual(
            self.ignored(run),
            [advisory for exception in AUDIT.EXCEPTIONS for advisory in exception.advisories],
        )

    def test_unexcepted_audit_failure_is_propagated(self):
        lock = self.audit_lock([("unrelated-crate", "1.0.0")], date(2026, 11, 16))
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 17)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2026, 11, 16)), 17)
        self.assertEqual(self.ignored(run), [])

    def test_each_exception_is_scoped_to_its_exact_package_and_version(self):
        for exception in EXCEPTIONS:
            with self.subTest(package=exception.package):
                exact = self.audit_lock(
                    [(exception.package, exception.version)], date(2026, 11, 16)
                )
                other = self.audit_lock(
                    [(exception.package, "99.99.99")], date(2026, 11, 16)
                )
                with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                    self.assertEqual(AUDIT.audit(exact, date(2026, 11, 16)), 0)
                    self.assertEqual(self.ignored(run), list(exception.advisories))
                with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
                    self.assertEqual(AUDIT.audit(other, date(2026, 11, 16)), 0)
                    self.assertEqual(self.ignored(run), [])

    def test_each_exception_expires_on_its_own_date(self):
        for exception in EXCEPTIONS:
            lock = self.audit_lock(
                [(exception.package, exception.version)], exception.expires
            )
            with patch.object(AUDIT.subprocess, "run") as run:
                self.assertEqual(AUDIT.audit(lock, exception.expires), 1)
                run.assert_not_called()

    def test_upgraded_versions_need_no_exception_even_after_expiry(self):
        lock = self.audit_lock(
            [
                ("rustls", "0.23.46"),
                ("crypto-bigint", "0.7.8"),
                ("zstd", "0.14.3"),
                ("zstd-safe", "8.2.0"),
                ("zstd-sys", "2.1.2"),
            ],
            date(2027, 12, 31),
        )
        with patch.object(AUDIT.subprocess, "run", return_value=subprocess.CompletedProcess([], 17)) as run:
            self.assertEqual(AUDIT.audit(lock, date(2027, 12, 31)), 17)
        self.assertEqual(self.ignored(run), [])


if __name__ == "__main__":
    unittest.main()
