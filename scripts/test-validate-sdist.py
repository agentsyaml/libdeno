import io
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from zipfile import ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from validate_sdist import REQUIRED, validate, validate_wheel_readmes

README = (Path(__file__).resolve().parents[1] / "python/README-pypi.md").read_text()


class SdistValidationTests(unittest.TestCase):
    def make_archive(self, directory, version="1.2.3", missing=None, metadata_version=None, cargo_version=None, description=None, python_readme=None, readme_target="README-pypi.md"):
        path = Path(directory) / f"libdeno-{version}.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            files = REQUIRED - ({missing} if missing else set())
            files |= {"PKG-INFO"}
            for name in files:
                value = metadata_version or version
                config = "project" if name == "pyproject.toml" else "package"
                declared = cargo_version if name in {"Cargo.toml", "python/Cargo.toml"} else version
                declared = declared or version
                content = f"[{config}]\nversion = \"{declared}\"\n" if name in {"pyproject.toml", "Cargo.toml", "python/Cargo.toml"} else "file"
                if name == "pyproject.toml":
                    content += f'readme = "{readme_target}"\n'
                if name == "PKG-INFO":
                    content = f"Metadata-Version: 2.1\nName: libdeno\nVersion: {value}\n\n{description or README}"
                elif name == "README-pypi.md":
                    content = python_readme or README
                info = tarfile.TarInfo(f"libdeno-{version}/{name}")
                info.size = len(content.encode())
                archive.addfile(info, io.BytesIO(content.encode()))
        return path

    def make_wheel(self, directory, description=README):
        path = Path(directory) / "libdeno-1.2.3-py3-none-any.whl"
        with ZipFile(path, "w") as wheel:
            wheel.writestr("libdeno-1.2.3.dist-info/METADATA", f"Metadata-Version: 2.1\nName: libdeno\nVersion: 1.2.3\n\n{description}")
        return path

    def test_accepts_complete_matching_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            validate([self.make_archive(directory)], "1.2.3")

    def test_rejects_wrong_version_and_missing_cargo_file(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, missing="Cargo.lock")
            with self.assertRaisesRegex(ValueError, "missing required files"):
                validate([archive], "1.2.3")
            with self.assertRaisesRegex(ValueError, "filename"):
                validate([archive], "2.0.0")

    def test_requires_exactly_one_archive(self):
        with self.assertRaisesRegex(ValueError, "exactly one"):
            validate([], "1.2.3")

    def test_rejects_mismatched_metadata_version(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, metadata_version="9.9.9")
            with self.assertRaisesRegex(ValueError, "expected sdist version"):
                validate([archive], "1.2.3")

    def test_rejects_mismatched_cargo_version(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, cargo_version="9.9.9")
            with self.assertRaisesRegex(ValueError, "Cargo.toml: expected version"):
                validate([archive], "1.2.3")

    def test_rejects_sdist_readme_mismatch(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, description="Wrong description.\n")
            with self.assertRaisesRegex(ValueError, "README-pypi.md"):
                validate([archive], "1.2.3")

    def test_rejects_readme_not_matching_public_python_readme(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, description="Wrong description.\n", python_readme="Wrong description.\n")
            with self.assertRaisesRegex(ValueError, "does not match python/README-pypi.md"):
                validate([archive], "1.2.3")

    def test_rejects_wrong_pyproject_readme_target(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory, readme_target="README.md")
            with self.assertRaisesRegex(ValueError, "must select README-pypi.md"):
                validate([archive], "1.2.3")

    def test_wheel_metadata_matches_sdist_readme(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.make_archive(directory)
            validate_wheel_readmes(archive, [self.make_wheel(directory)])
            validate_wheel_readmes(archive, [self.make_wheel(directory, README.replace("\n", "\r\n"))])
            with self.assertRaisesRegex(ValueError, "does not match Python README"):
                validate_wheel_readmes(archive, [self.make_wheel(directory, "Wrong description.\n")])


if __name__ == "__main__":
    unittest.main()
