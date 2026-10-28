"""Runtime state reuse, refresh, and concurrency tests."""

import json
import os
import threading
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import libdeno


class RuntimeStateTests(unittest.TestCase):
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

    def test_runtime_rebuilds_after_config_fingerprint_changes(self):
        one = self.write("one.js", "export const value = 'one';")
        two = self.write("two.js", "export const value = 'two';")
        first_entry = Path(
            os.path.realpath(
                self.write(
                    "first.js",
                    "import { value } from 'virtual'; if (value !== 'one') Deno.exit(1);",
                )
            )
        )
        second_entry = Path(
            os.path.realpath(
                self.write(
                    "second.js",
                    "import { value } from 'virtual'; if (value !== 'two') Deno.exit(1);",
                )
            )
        )
        config = self.root / "deno.json"
        config.write_text(json.dumps({"imports": {"virtual": f"./{one.name}"}}), encoding="utf-8")
        runtime = libdeno.Runtime(self.root)
        permission = (f"--allow-read={os.path.realpath(self.root)}",)

        self.assertEqual(runtime.run(first_entry, permissions=permission), 0)
        config.write_text(json.dumps({"imports": {"virtual": f"./{two.name}"}}), encoding="utf-8")
        self.assertEqual(runtime.run(second_entry, permissions=permission), 0)

    def test_runtime_refresh_rebuilds_after_nested_package_config_changes(self):
        self.write("package.json", '{"name":"refresh-project","private":true}')
        self.write(
            "node_modules/nested-pkg/package.json",
            '{"name":"nested-pkg","version":"1.0.0","type":"module","main":"index.js"}',
        )
        self.write(
            "node_modules/nested-pkg/index.js",
            "import { marker } from 'nested-dep';\nexport { marker };",
        )
        self.write(
            "node_modules/nested-pkg/node_modules/nested-dep/package.json",
            '{"name":"nested-dep","version":"1.0.0","type":"module","main":"a.js"}',
        )
        self.write(
            "node_modules/nested-pkg/node_modules/nested-dep/a.js",
            "export const marker = 'a';",
        )
        self.write(
            "node_modules/nested-pkg/node_modules/nested-dep/b.js",
            "export const marker = 'b';",
        )
        entry = self.write(
            "main.js",
            "import { marker } from './node_modules/nested-pkg/index.js';"
            "\nDeno.writeTextFileSync(new URL('./out.txt', import.meta.url), marker);",
        )
        output = self.root / "out.txt"
        runtime = libdeno.Runtime(self.root)

        self.assertEqual(runtime.run(entry, allow_all_permissions=True), 0)
        self.assertEqual(output.read_text(encoding="utf-8"), "a")
        self.write(
            "node_modules/nested-pkg/node_modules/nested-dep/package.json",
            '{"name":"nested-dep","version":"1.0.0","type":"module","main":"b.js"}',
        )
        self.assertIsNone(runtime.refresh())
        self.assertEqual(runtime.run(entry, allow_all_permissions=True), 0)
        self.assertEqual(output.read_text(encoding="utf-8"), "b")

    def test_entry_preparation_does_not_stall_other_threads(self):
        # A stalled filesystem cannot be simulated portably, so this covers the
        # reachable part: preparation-only calls (missing entry) keep working
        # from another thread while a run is in flight. Whether the stat itself
        # runs detached is verified by code review, not by this test.
        script = self.write(
            "hold-preparation.js",
            "const until = Date.now() + 1000; while (Date.now() < until) {}",
        )
        missing = self.root / "missing.js"
        runtime = libdeno.Runtime(self.root)
        results = []
        errors = []

        def execute():
            try:
                results.append(
                    runtime.run(script, allow_all_permissions=True, execution_deadline=5.0)
                )
            except BaseException as error:  # surface native errors after join
                errors.append(error)

        thread = threading.Thread(target=execute)
        thread.start()
        checked = 0
        try:
            deadline = time.monotonic() + 2.0
            while thread.is_alive() and time.monotonic() < deadline:
                with self.assertRaises(libdeno.EntryError):
                    libdeno.run(missing, allow_all_permissions=True)
                checked += 1
                time.sleep(0.01)
        finally:
            thread.join(timeout=10.0)

        self.assertFalse(thread.is_alive())
        self.assertFalse(errors, errors)
        self.assertEqual(results, [0])
        self.assertGreater(checked, 0)

    def test_parallel_python_threads_and_gil_progress(self):
        script = self.write(
            "peer.js",
            """
const own = Deno.args[0];
const peer = Deno.args[1];
Deno.writeTextFileSync(own, "ready");
let found = false;
const waitUntil = Date.now() + 2000;
while (Date.now() < waitUntil) {
  try { Deno.statSync(peer); found = true; break; } catch (_) {}
}
if (!found) Deno.exit(1);
const holdUntil = Date.now() + 500;
while (Date.now() < holdUntil) {}
Deno.exit(0);
""".strip(),
        )
        markers = [self.root / "thread-1.ready", self.root / "thread-2.ready"]
        runtime = libdeno.Runtime(self.root)
        barrier = threading.Barrier(3)
        results = [None, None]
        errors = []

        def execute(index):
            try:
                barrier.wait()
                results[index] = runtime.run(
                    script,
                    allow_all_permissions=True,
                    args=(str(markers[index]), str(markers[1 - index])),
                    execution_deadline=3.0,
                )
            except BaseException as error:  # surface native errors after join
                errors.append(error)

        threads = [threading.Thread(target=execute, args=(i,)) for i in range(2)]
        for thread in threads:
            thread.start()
        barrier.wait()

        marker_deadline = time.monotonic() + 2.0
        while time.monotonic() < marker_deadline and not all(path.exists() for path in markers):
            time.sleep(0.01)

        probe_seen = threading.Event()
        stop_probe = threading.Event()

        def probe():
            while not stop_probe.is_set():
                probe_seen.set()
                time.sleep(0)

        probe_thread = threading.Thread(target=probe)
        probe_thread.start()
        try:
            self.assertTrue(all(path.exists() for path in markers))
            self.assertTrue(probe_seen.wait(1.0), "another Python thread could not acquire the GIL")
            self.assertTrue(any(thread.is_alive() for thread in threads))
        finally:
            stop_probe.set()
            probe_thread.join(timeout=2.0)
            for thread in threads:
                thread.join(timeout=5.0)

        self.assertFalse(any(thread.is_alive() for thread in threads))
        self.assertFalse(errors, errors)
        self.assertEqual(results, [0, 0])


if __name__ == "__main__":
    unittest.main()
