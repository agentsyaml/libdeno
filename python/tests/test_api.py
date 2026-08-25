"""Synchronous Python alpha API tests.

The binding intentionally does not expose capture, async, subprocess, or
permission-hook APIs.
"""

import json
import os
import sys
import threading
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import libdeno


class ApiTests(unittest.TestCase):
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

    def test_permissions_are_fail_closed_but_allow_all_runs(self):
        entry = self.write("ok.js", "const answer = 40 + 2; if (answer !== 42) Deno.exit(1);")

        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run(entry)
        self.assertEqual(libdeno.run(entry, allow_all_permissions=True), 0)

    def test_features_preserve_omitted_vs_explicit_empty(self):
        enabled = self.write(
            "kv-enabled.js",
            "if (typeof Deno.openKv !== 'function') Deno.exit(1);",
        )
        disabled = self.write(
            "kv-disabled.js",
            "if (typeof Deno.openKv === 'function') Deno.exit(1);",
        )

        self.assertEqual(libdeno.run(enabled, allow_all_permissions=True), 0)
        self.assertEqual(
            libdeno.run(disabled, allow_all_permissions=True, features=()),
            0,
        )

        runtime = libdeno.Runtime(self.root)
        self.assertEqual(runtime.run(enabled, allow_all_permissions=True), 0)
        self.assertEqual(
            runtime.run(disabled, allow_all_permissions=True, features=[]),
            0,
        )

    def test_scoped_read_succeeds_and_is_denied_outside_scope(self):
        allowed = self.root / "allowed"
        allowed.mkdir()
        inside = allowed / "inside.txt"
        outside = self.root / "outside.txt"
        inside.write_text("inside", encoding="utf-8")
        outside.write_text("outside", encoding="utf-8")
        permission = (f"--allow-read={allowed}",)

        inside_entry = self.write(
            "allowed/inside.js",
            f"if (Deno.readTextFileSync({json.dumps(str(inside))}) !== 'inside') Deno.exit(1);",
        )
        outside_entry = self.write(
            "allowed/outside.js",
            f"Deno.readTextFileSync({json.dumps(str(outside))});",
        )

        self.assertEqual(
            libdeno.run(inside_entry, permissions=permission, cwd=self.root),
            0,
        )
        with self.assertRaises(libdeno.DenoPermissionError):
            libdeno.run(outside_entry, permissions=permission, cwd=self.root)

    def test_runtime_reuses_resolver_and_does_not_leak_permissions(self):
        allowed = self.root / "allowed"
        other = self.root / "other"
        allowed.mkdir()
        other.mkdir()
        data = allowed / "data.txt"
        data.write_text("ok", encoding="utf-8")
        entry = self.write(
            "allowed/read.js",
            f"if (Deno.readTextFileSync({json.dumps(str(data))}) !== 'ok') Deno.exit(1);",
        )
        runtime = libdeno.Runtime(self.root)
        allowed_permission = (f"--allow-read={allowed}",)

        self.assertEqual(runtime.run(entry, permissions=allowed_permission), 0)
        self.assertEqual(runtime.run(entry, permissions=allowed_permission), 0)
        with self.assertRaises(libdeno.DenoPermissionError):
            runtime.run(entry, permissions=(f"--allow-read={other}",))

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

    def test_deadline_and_javascript_errors_have_distinct_exceptions(self):
        loop = self.write("loop.js", "while (true) {}")
        for deadline in (-1.0, float("nan"), float("inf")):
            with self.subTest(deadline=deadline):
                with self.assertRaises(libdeno.ConfigurationError):
                    libdeno.run(
                        loop,
                        allow_all_permissions=True,
                        execution_deadline=deadline,
                    )

        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run(
                loop,
                allow_all_permissions=True,
                max_heap_bytes=(8 << 20) - 1,
            )

        with self.assertRaises(libdeno.DenoTimeoutError):
            libdeno.run(loop, allow_all_permissions=True, execution_deadline=0.05)

        failure = self.write("failure.js", "throw new Error('phase 1 failure');")
        with self.assertRaises(libdeno.DenoRuntimeError):
            libdeno.run(failure, allow_all_permissions=True)

        permission_marker = self.write(
            "permission-marker.js",
            "throw new Error('Requires read access');",
        )
        with self.assertRaises(libdeno.DenoRuntimeError):
            libdeno.run(permission_marker, allow_all_permissions=True)

    def test_invalid_heap_size_numbers_raise_configuration_error(self):
        entry = self.write("heap.js", "while (true) {}")
        runtime = libdeno.Runtime(self.root)

        for max_heap_bytes in (-1, sys.maxsize * 2 + 2):
            with self.subTest(binding="run", max_heap_bytes=max_heap_bytes):
                with self.assertRaises(libdeno.ConfigurationError):
                    libdeno.run(
                        entry,
                        allow_all_permissions=True,
                        max_heap_bytes=max_heap_bytes,
                    )
            with self.subTest(binding="Runtime.run", max_heap_bytes=max_heap_bytes):
                with self.assertRaises(libdeno.ConfigurationError):
                    runtime.run(
                        entry,
                        allow_all_permissions=True,
                        max_heap_bytes=max_heap_bytes,
                    )

    def test_pathlike_and_non_ascii_paths(self):
        directory = self.root / "非ASCII-目录"
        entry = directory / "入口.js"
        directory.mkdir()
        entry.write_text("const ok = true;", encoding="utf-8")

        self.assertEqual(
            libdeno.run(Path(entry), cwd=Path(directory), allow_all_permissions=True),
            0,
        )
        runtime = libdeno.Runtime(os.fsencode(str(directory)))
        self.assertEqual(
            runtime.run(os.fsencode(str(entry)), allow_all_permissions=True),
            0,
        )

    def test_bytes_and_custom_fspath_paths(self):
        directory = self.root / "bytes-paths"
        entry = directory / "entry.js"
        directory.mkdir()
        entry.write_text("const ok = true;", encoding="utf-8")

        self.assertEqual(
            libdeno.run(
                os.fsencode(str(entry)),
                cwd=os.fsencode(str(directory)),
                allow_all_permissions=True,
            ),
            0,
        )

        class StringPathLike:
            def __init__(self, path):
                self.path = path

            def __fspath__(self):
                return str(self.path)

        class BytesPathLike:
            def __init__(self, path):
                self.path = path

            def __fspath__(self):
                return os.fsencode(str(self.path))

        self.assertEqual(
            libdeno.run(
                StringPathLike(entry),
                cwd=StringPathLike(directory),
                allow_all_permissions=True,
            ),
            0,
        )
        self.assertEqual(
            libdeno.run(
                BytesPathLike(entry),
                cwd=BytesPathLike(directory),
                allow_all_permissions=True,
            ),
            0,
        )

        class InvalidPathLike:
            def __fspath__(self):
                return 123

        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run(InvalidPathLike(), allow_all_permissions=True)
        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run(entry, cwd=InvalidPathLike(), allow_all_permissions=True)

    def test_none_cwd_uses_the_process_current_directory(self):
        entry = self.write("default-cwd.js", "const ok = true;")
        original_cwd = os.getcwd()
        os.chdir(self.root)
        try:
            self.assertEqual(
                libdeno.run("default-cwd.js", cwd=None, allow_all_permissions=True),
                0,
            )
            self.assertEqual(
                libdeno.Runtime(cwd=None).run(
                    "default-cwd.js", allow_all_permissions=True
                ),
                0,
            )
            self.assertEqual(
                libdeno.Runtime().run("default-cwd.js", allow_all_permissions=True),
                0,
            )
        finally:
            os.chdir(original_cwd)

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
