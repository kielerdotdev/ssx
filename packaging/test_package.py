#!/usr/bin/env python3
"""Tests for packaging/package.py and packaging/install.sh. Run: python3 packaging/test_package.py"""

from __future__ import annotations

import gzip
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import package  # noqa: E402

PROGRAMS = package.BINARIES


def fake_build(root: Path, os_name: str, *, profile: str = "release", target: str | None = None) -> Path:
    """A target dir containing a fake build of every program."""
    bin_dir = root / target / profile if target else root / profile
    bin_dir.mkdir(parents=True)
    for name in PROGRAMS:
        (bin_dir / package.exe_name(name, os_name)).write_bytes(f"#!/bin/sh\necho {name}\n".encode())
    return root


class ArgsMixin:
    def args(self, tmp: Path, **kw):
        base = ["--version", "1.2.3", "--variant", "dynamic", "--target-dir", str(tmp / "target"),
                "--out", str(tmp / "dist"), "--arch", "x86_64"]
        for k, v in kw.items():
            base += [f"--{k.replace('_', '-')}", str(v)]
        return base


class ParsingTests(unittest.TestCase):
    def test_ldd_output_is_parsed(self):
        out = """\
\tlinux-vdso.so.1 (0x00007ffd)
\tlibavcodec.so.60 => /lib/x86_64-linux-gnu/libavcodec.so.60 (0x0000)
\tlibm.so.6 => /lib/x86_64-linux-gnu/libm.so.6 (0x0000)
\tlibmissing.so.1 => not found
\t/lib64/ld-linux-x86-64.so.2 (0x0000)
"""
        self.assertEqual(package.parse_ldd(out), ["libavcodec.so.60", "libm.so.6", "libmissing.so.1"])

    def test_prose_lines_are_not_libraries(self):
        self.assertEqual(package.parse_ldd("\tstatically linked\n"), [])
        self.assertEqual(package.parse_ldd("\tnot a dynamic executable\n"), [])
        self.assertEqual(package.parse_ldd(""), [])

    def test_architecture_names_are_normalised(self):
        self.assertEqual(package.machine_arch("AMD64"), "x86_64")
        self.assertEqual(package.machine_arch("arm64"), "aarch64")
        self.assertEqual(package.machine_arch("riscv64"), "riscv64")

    def test_bad_versions_and_variants_are_rejected(self):
        for argv in (
            ["--os", "linux", "--version", "v1.0.0", "--variant", "a"],
            ["--os", "linux", "--version", "1.0/x", "--variant", "a"],
            ["--os", "linux", "--version", "1.0.0", "--variant", "a b"],
            ["--os", "linux", "--version", "1.0.0", "--variant", "../x"],
        ):
            with self.subTest(argv=argv):
                with open(os.devnull, "w") as devnull, self.assertRaises(SystemExit):
                    old, sys.stderr = sys.stderr, devnull
                    try:
                        package.parse_args(argv)
                    finally:
                        sys.stderr = old


class ForbiddenLibTests(unittest.TestCase):
    def test_passes_when_nothing_forbidden_is_linked(self):
        package.check_forbidden({"ssx": ["libc.so.6", "libva.so.2"]}, ["libavcodec", "libx264"])

    def test_fails_naming_each_offender(self):
        with self.assertRaises(package.PackagingError) as cm:
            package.check_forbidden(
                {"ssx": ["libavcodec.so.60"], "ssx-app": ["libc.so.6", "libx264.so.164"]},
                ["libavcodec", "libx264"],
            )
        msg = str(cm.exception)
        self.assertIn("ssx -> libavcodec.so.60", msg)
        self.assertIn("ssx-app -> libx264.so.164", msg)
        self.assertNotIn("libc.so.6", msg)

    def test_fails_closed_when_dependencies_cannot_be_inspected(self):
        with self.assertRaises(package.PackagingError) as cm:
            package.check_forbidden({"ssx": None}, ["libavcodec"])
        self.assertIn("ldd", str(cm.exception))


class PackageTests(ArgsMixin, unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def run_package(self, os_name: str, **kw) -> Path:
        argv = ["--os", os_name] + self.args(self.tmp, **kw)
        return package.package(package.parse_args(argv))

    def test_linux_archive_layout_manifest_and_checksum(self):
        fake_build(self.tmp / "target", "linux")
        archive = self.run_package("linux")
        self.assertEqual(archive.name, "ssx-1.2.3-linux-x86_64-dynamic.tar.gz")
        with tarfile.open(archive) as tar:
            names = tar.getnames()
            top = "ssx-1.2.3-linux-x86_64-dynamic"
            for n in PROGRAMS:
                self.assertIn(f"{top}/bin/{n}", names)
                self.assertEqual(tar.getmember(f"{top}/bin/{n}").mode & 0o777, 0o755)
            for extra in ("LICENSE", "README.md", "install.sh", "uninstall.sh", "MANIFEST.json"):
                self.assertIn(f"{top}/{extra}", names)
            self.assertEqual(tar.getmember(f"{top}/install.sh").mode & 0o777, 0o755)
            manifest = json.load(tar.extractfile(f"{top}/MANIFEST.json"))
        self.assertEqual((manifest["version"], manifest["os"], manifest["variant"]), ("1.2.3", "linux", "dynamic"))
        self.assertEqual(sorted(manifest["binaries"]), sorted(PROGRAMS))
        self.assertEqual(len(manifest["binaries"]["ssx"]["sha256"]), 64)
        line = (archive.parent / (archive.name + ".sha256")).read_text()
        self.assertEqual(line, f"{package.sha256_file(archive)}  {archive.name}\n")

    def test_windows_archive_is_a_zip_with_exe_files(self):
        fake_build(self.tmp / "target", "windows")
        argv = ["--os", "windows", "--version", "1.2.3", "--variant", "lite", "--target-dir",
                str(self.tmp / "target"), "--out", str(self.tmp / "dist"), "--arch", "x86_64"]
        archive = package.package(package.parse_args(argv))
        self.assertEqual(archive.name, "ssx-1.2.3-windows-x86_64-lite.zip")
        with zipfile.ZipFile(archive) as z:
            names = z.namelist()
            top = "ssx-1.2.3-windows-x86_64-lite"
            for n in PROGRAMS:
                self.assertIn(f"{top}/bin/{n}.exe", names)
            self.assertIn(f"{top}/install.ps1", names)
            self.assertNotIn(f"{top}/install.sh", names)
            self.assertIsNone(z.testzip())

    def test_cross_target_directory_layout_is_found(self):
        fake_build(self.tmp / "target", "windows", target="x86_64-pc-windows-msvc")
        argv = ["--os", "windows", "--version", "1.0.0", "--variant", "full", "--target-dir",
                str(self.tmp / "target"), "--target", "x86_64-pc-windows-msvc",
                "--out", str(self.tmp / "dist"), "--arch", "x86_64"]
        self.assertTrue(package.package(package.parse_args(argv)).is_file())

    def test_archives_are_reproducible(self):
        fake_build(self.tmp / "target", "linux")
        first = self.run_package("linux").read_bytes()
        second = self.run_package("linux", out=self.tmp / "dist2").read_bytes()
        self.assertEqual(first, second, "same inputs must give byte-identical archives")
        os.environ["SOURCE_DATE_EPOCH"] = "1700000000"
        try:
            third = self.run_package("linux", out=self.tmp / "dist3").read_bytes()
        finally:
            del os.environ["SOURCE_DATE_EPOCH"]
        self.assertNotEqual(first, third, "SOURCE_DATE_EPOCH must be honoured")

    def test_incomplete_builds_are_refused_with_the_full_list(self):
        root = fake_build(self.tmp / "target", "linux")
        (root / "release" / "ssx-overlay").unlink()
        (root / "release" / "ssx-app").write_bytes(b"")  # empty counts as missing
        with self.assertRaises(package.PackagingError) as cm:
            self.run_package("linux")
        msg = str(cm.exception)
        self.assertIn("ssx-overlay", msg)
        self.assertIn("ssx-app", msg)
        self.assertNotIn("ssx-editor-ui,", msg)

    def test_forbid_lib_blocks_packaging(self):
        fake_build(self.tmp / "target", "linux")
        # Fake binaries are shell scripts, so `ldd` reports "not a dynamic executable": nothing
        # forbidden is linked and packaging must succeed...
        self.run_package("linux", forbid_lib="libavcodec")
        # ...while an unknown-dependencies situation fails closed.
        real = package.shutil.which
        package.shutil.which = lambda _name: None
        try:
            with self.assertRaises(package.PackagingError):
                self.run_package("linux", forbid_lib="libavcodec", out=self.tmp / "d2")
        finally:
            package.shutil.which = real

    def test_real_binary_dependencies_are_recorded_on_linux(self):
        if shutil.which("ldd") is None or not sys.platform.startswith("linux"):
            self.skipTest("needs Linux ldd")
        build = self.tmp / "target" / "release"
        build.mkdir(parents=True)
        for name in PROGRAMS:
            shutil.copy("/bin/ls", build / name)  # a real dynamic ELF
        archive = self.run_package("linux")
        with tarfile.open(archive) as tar:
            manifest = json.load(tar.extractfile("ssx-1.2.3-linux-x86_64-dynamic/MANIFEST.json"))
        needs = manifest["binaries"]["ssx"]["needs"]
        self.assertTrue(any(lib.startswith("libc.so") for lib in needs), needs)


@unittest.skipUnless(sys.platform != "win32" and shutil.which("sh"), "needs a POSIX shell")
class InstallScriptTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        # Unpack a fake archive exactly as a user would.
        fake_build(self.tmp / "target", "linux")
        archive = package.package(package.parse_args(
            ["--os", "linux", "--version", "9.9.9", "--variant", "dynamic", "--arch", "x86_64",
             "--target-dir", str(self.tmp / "target"), "--out", str(self.tmp / "dist")]))
        with tarfile.open(archive) as tar:
            tar.extractall(self.tmp / "unpacked")
        self.unpacked = next((self.tmp / "unpacked").iterdir())
        self.home = self.tmp / "home"
        self.home.mkdir()

    def sh(self, script: str, *args: str, env_extra=None):
        env = {"PATH": os.environ["PATH"], "HOME": str(self.home)}
        env.update(env_extra or {})
        return subprocess.run(["sh", str(self.unpacked / script), *args], capture_output=True,
                              text=True, env=env, timeout=60)

    def test_installs_into_home_local_and_uninstalls(self):
        r = self.sh("install.sh")
        self.assertEqual(r.returncode, 0, r.stderr)
        for n in PROGRAMS:
            f = self.home / ".local" / "bin" / n
            self.assertTrue(f.is_file() and os.access(f, os.X_OK), f)
        self.assertIn("Installed. Next steps", r.stdout)
        r = self.sh("uninstall.sh")
        self.assertEqual(r.returncode, 0, r.stderr)
        for n in PROGRAMS:
            self.assertFalse((self.home / ".local" / "bin" / n).exists())

    def test_dry_run_changes_nothing(self):
        r = self.sh("install.sh", "--dry-run", "--integrate")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("would run:", r.stdout)
        self.assertIn("daemon autostart enable", r.stdout)
        self.assertFalse((self.home / ".local").exists(), "dry run must not create anything")

    def test_custom_prefix_with_spaces(self):
        prefix = self.tmp / "my prefix"
        r = self.sh("install.sh", "--prefix", str(prefix))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue((prefix / "bin" / "ssx").is_file())

    def test_integrate_runs_the_real_subcommands(self):
        # Make the installed `ssx` a recorder so we can see exactly what install.sh invokes.
        r = self.sh("install.sh")
        self.assertEqual(r.returncode, 0, r.stderr)
        log = self.tmp / "calls.log"
        ssx = self.home / ".local" / "bin" / "ssx"
        ssx.write_text(f'#!/bin/sh\necho "$@" >> "{log}"\n')
        ssx.chmod(0o755)
        r = self.sh("install.sh", "--integrate")
        self.assertEqual(r.returncode, 0, r.stderr)
        # install(1) replaced our recorder with the packaged fake, so re-plant it and call again.
        ssx.write_text(f'#!/bin/sh\necho "$@" >> "{log}"\n')
        ssx.chmod(0o755)
        r = self.sh("uninstall.sh")
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = log.read_text().splitlines()
        self.assertEqual(calls, ["daemon stop", "daemon autostart disable", "shell uninstall", "hotkeys uninstall"])

    def test_uninstall_survives_failing_ssx_subcommands(self):
        self.assertEqual(self.sh("install.sh").returncode, 0)
        ssx = self.home / ".local" / "bin" / "ssx"
        ssx.write_text("#!/bin/sh\nexit 7\n")
        ssx.chmod(0o755)
        r = self.sh("uninstall.sh")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(ssx.exists(), "files are removed even when the subcommands fail")

    def test_settings_are_never_touched(self):
        cfg = self.home / ".config" / "ssx"
        data = self.home / ".local" / "share" / "ssx"
        cfg.mkdir(parents=True)
        data.mkdir(parents=True)
        (cfg / "settings.toml").write_text("keep = true\n")
        (data / "history.sqlite3").write_text("keep")
        self.assertEqual(self.sh("install.sh").returncode, 0)
        self.assertEqual(self.sh("uninstall.sh").returncode, 0)
        self.assertEqual((cfg / "settings.toml").read_text(), "keep = true\n")
        self.assertEqual((data / "history.sqlite3").read_text(), "keep")

    def test_without_home_and_prefix_it_refuses(self):
        env = {"PATH": os.environ["PATH"]}
        r = subprocess.run(["sh", str(self.unpacked / "install.sh")], capture_output=True, text=True, env=env)
        self.assertEqual(r.returncode, 2)
        self.assertIn("--prefix", r.stderr)

    def test_unknown_option_and_missing_value_are_usage_errors(self):
        self.assertEqual(self.sh("install.sh", "--bogus").returncode, 2)
        self.assertEqual(self.sh("install.sh", "--prefix").returncode, 2)

    def test_runs_under_dash(self):
        dash = shutil.which("dash")
        if not dash:
            self.skipTest("dash not installed")
        env = {"PATH": os.environ["PATH"], "HOME": str(self.home)}
        r = subprocess.run([dash, str(self.unpacked / "install.sh")], capture_output=True, text=True, env=env)
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
