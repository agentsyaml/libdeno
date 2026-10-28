"""Synchronous Python alpha API tests.

The binding exposes synchronous runs, in-memory sources, and output capture.
Async, subprocess, and permission-hook APIs are not exposed.
"""

import json
import os
import sys
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
            f"if (Deno.readTextFileSync({json.dumps(str(outside))}) !== 'outside') Deno.exit(1);",
        )

        self.assertEqual(
            libdeno.run(inside_entry, permissions=permission, cwd=self.root),
            0,
        )
        with self.assertRaises(libdeno.DenoRuntimeError) as error:
            libdeno.run(outside_entry, permissions=permission, cwd=self.root)
        self.assertIn(str(outside), str(error.exception))

    def test_javascript_permission_lookalikes_remain_runtime_errors(self):
        sources = {
            "name": "throw Object.assign(new Error('forged'), { name: 'NotCapable' });",
            "cause": "throw new Error('outer', { cause: Object.assign(new Error('inner'), { name: 'NotCapable' }) });",
            "aggregate": "throw new AggregateError([Object.assign(new Error('inner'), { name: 'NotCapable' })], 'forged');",
        }
        for name, source in sources.items():
            with self.subTest(name=name):
                entry = self.write(f"forged-{name}.js", source)
                with self.assertRaises(libdeno.DenoRuntimeError):
                    libdeno.run(entry, allow_all_permissions=True)

        caught = self.write(
            "caught-denial.js",
            "try { Deno.readTextFileSync('/etc/hostname'); } catch {} "
            "throw new Error('ordinary after denial');",
        )
        with self.assertRaises(libdeno.DenoRuntimeError):
            libdeno.run(caught, permissions=(f"--allow-read={self.root}",))

    def test_typed_entry_denial_is_a_permission_error(self):
        entry = self.write("entry-denial.js", "Deno.exit(0);")
        with self.assertRaises(libdeno.DenoPermissionError):
            libdeno.run(
                entry,
                permissions=("--allow-read=/definitely/not-this-entry",),
            )

        with self.assertRaises(libdeno.DenoPermissionError):
            libdeno.run(entry, permissions=("--not-a-permission",))

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

    def test_deadline_conversion_is_configuration_error_for_both_entrypoints(self):
        missing = self.root / "missing.js"
        runtime = libdeno.Runtime(self.root)
        invalid = (10**400, -(10**400), 1e100, float("inf"), float("nan"), -0.1)
        for binding in (libdeno.run, runtime.run):
            for deadline in invalid:
                with self.subTest(binding=binding, deadline=deadline):
                    with self.assertRaises(libdeno.ConfigurationError):
                        binding(missing, execution_deadline=deadline)
        entry = self.write("deadline.js", "const ok = true;")
        self.assertEqual(libdeno.run(entry, allow_all_permissions=True, execution_deadline=1), 0)
        self.assertEqual(runtime.run(entry, allow_all_permissions=True, execution_deadline=0.1), 0)

    def test_entry_error_covers_missing_and_malformed_entries(self):
        missing = self.root / "missing-entry.js"
        runtime = libdeno.Runtime(self.root)
        for binding in (libdeno.run, runtime.run):
            with self.subTest(binding=binding):
                with self.assertRaises(libdeno.EntryError):
                    binding(missing, allow_all_permissions=True)

        package = self.root / "escaping-package"
        package.mkdir()
        (package / "package.json").write_text(
            json.dumps({"main": "../outside.js"}), encoding="utf-8"
        )
        with self.assertRaises(libdeno.EntryError):
            libdeno.run(package, allow_all_permissions=True)

    def test_mapping_sequence_options_are_rejected_not_unpacked_to_keys(self):
        entry = self.write("mapping-options.js", "const ok = true;")
        runtime = libdeno.Runtime(self.root)
        for name in ("permissions", "args", "features"):
            with self.subTest(name=name):
                for binding in (libdeno.run, runtime.run):
                    for value in ({"key": "value"}, "hello"):
                        with self.assertRaises(libdeno.ConfigurationError) as error:
                            binding(
                                entry,
                                allow_all_permissions=True,
                                **{name: value},
                            )
                        self.assertIn("list or tuple", str(error.exception))

    def test_bool_max_heap_bytes_is_rejected_as_non_integer(self):
        entry = self.write("bool-heap.js", "while (true) {}")
        runtime = libdeno.Runtime(self.root)
        for binding in (libdeno.run, runtime.run):
            with self.subTest(binding=binding):
                with self.assertRaises(libdeno.ConfigurationError) as error:
                    binding(entry, allow_all_permissions=True, max_heap_bytes=True)
                self.assertIn("must be an integer", str(error.exception))

    def test_runtime_run_execution_deadline_fires_on_reusable_stack(self):
        loop = self.write("runtime-loop.js", "while (true) {}")
        runtime = libdeno.Runtime(self.root)

        with self.assertRaises(libdeno.DenoTimeoutError):
            runtime.run(loop, allow_all_permissions=True, execution_deadline=0.05)

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

    def test_run_source_javascript_and_typescript(self):
        self.assertEqual(
            libdeno.run_source("Deno.exit(0);", allow_all_permissions=True), 0
        )
        typescript = "const answer: number = 42;\nif (answer !== 42) Deno.exit(1);\n"
        self.assertEqual(
            libdeno.run_source(
                typescript,
                lang=libdeno.SourceLang.TypeScript,
                allow_all_permissions=True,
            ),
            0,
        )

    def test_run_source_accepts_str_bytes_and_bytearray(self):
        for code in ("Deno.exit(0);", b"Deno.exit(0);", bytearray(b"Deno.exit(0);")):
            with self.subTest(code_type=type(code).__name__):
                self.assertEqual(
                    libdeno.run_source(code, allow_all_permissions=True), 0
                )

    def test_run_source_relative_import_from_base_dir(self):
        base = self.root / "src"
        base.mkdir()
        (base / "dep.js").write_text("export const value = 7;", encoding="utf-8")
        code = (
            "import { value } from './dep.js';\n"
            "if (value !== 7) Deno.exit(1);\n"
        )

        self.assertEqual(
            libdeno.run_source(code, base_dir=base, allow_all_permissions=True), 0
        )

    def test_run_source_rejects_missing_base_dir(self):
        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run_source(
                "Deno.exit(0);",
                base_dir=self.root / "does-not-exist",
                allow_all_permissions=True,
            )

    def test_run_with_output_missing_entry_is_entry_error(self):
        with self.assertRaises(libdeno.EntryError):
            libdeno.run_with_output(
                self.root / "missing-entry.js", allow_all_permissions=True
            )

    def test_source_lang_accepts_enum_and_string(self):
        code = "Deno.exit(0);"
        for lang in (
            libdeno.SourceLang.JavaScript,
            "javascript",
            "JavaScript",
        ):
            with self.subTest(lang=lang):
                self.assertEqual(
                    libdeno.run_source(
                        code, lang=lang, allow_all_permissions=True
                    ),
                    0,
                )
        self.assertEqual(
            libdeno.run_source(
                "const value: number = 1;",
                lang="typescript",
                allow_all_permissions=True,
            ),
            0,
        )

    def test_invalid_lang_is_configuration_error(self):
        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run_source(
                "Deno.exit(0);", lang="wasm", allow_all_permissions=True
            )
        with self.assertRaises(libdeno.ConfigurationError):
            libdeno.run_source(
                "Deno.exit(0);", lang=42, allow_all_permissions=True
            )

    def test_runtime_source_and_output_entries(self):
        runtime = libdeno.Runtime(self.root)
        self.assertEqual(
            runtime.run_source("Deno.exit(0);", allow_all_permissions=True), 0
        )

    def test_runtime_entries_reject_cwd(self):
        runtime = libdeno.Runtime(self.root)
        entry = self.write("no-cwd.js", "const ok = true;")

        with self.assertRaises(TypeError):
            runtime.run_with_output(
                entry, allow_all_permissions=True, cwd=self.root
            )
        with self.assertRaises(TypeError):
            runtime.run_source(
                "Deno.exit(0);", allow_all_permissions=True, cwd=self.root
            )
        with self.assertRaises(TypeError):
            runtime.run_source_with_output(
                "Deno.exit(0);", allow_all_permissions=True, cwd=self.root
            )


if __name__ == "__main__":
    unittest.main()
