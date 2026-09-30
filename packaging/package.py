#!/usr/bin/env python3
"""Stage the built ssx binaries into a release archive.

Used by .github/workflows/release.yml on both Linux and Windows runners, and runnable locally:

    python3 packaging/package.py --os linux --version 0.1.0 --variant dynamic \
        --target-dir target --out dist

Produces, in ``--out``:

* ``ssx-<version>-<os>-<arch>-<variant>.tar.gz`` (Linux) or ``.zip`` (Windows)
* the same name plus ``.sha256`` (``sha256sum``-compatible)

The archive contains ``bin/`` (the five programs), ``LICENSE``, ``README.md``, the third-party
notices, an installer script, and ``MANIFEST.json`` (sizes, SHA-256 of every binary, build
variant, and on Linux the shared libraries each binary needs).

Archives are reproducible: member order, timestamps, owners and permissions are normalised
(``SOURCE_DATE_EPOCH`` if set, else the Unix epoch).

It refuses to package an incomplete build and, with ``--forbid-lib``, refuses a binary that
still links a library it must not (this is how the "static" variants prove FFmpeg is embedded).
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

#: The programs a release ships, in the order they are listed in the manifest.
BINARIES = ["ssx", "ssx-app", "ssx-overlay", "ssx-editor-ui", "ssx-settings-ui"]

ROOT = Path(__file__).resolve().parent.parent


class PackagingError(Exception):
    """Something is wrong with the build or the arguments; the message says what to do."""


def exe_name(name: str, os_name: str) -> str:
    return f"{name}.exe" if os_name == "windows" else name


def machine_arch(machine: str | None = None) -> str:
    """Normalised CPU architecture name used in archive names."""
    m = (machine or platform.machine()).lower()
    return {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(m, m)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def find_binaries(bin_dir: Path, os_name: str) -> dict[str, Path]:
    """Locates every shipped program in ``bin_dir`` or raises with the complete list of misses."""
    found: dict[str, Path] = {}
    missing: list[str] = []
    for name in BINARIES:
        p = bin_dir / exe_name(name, os_name)
        if p.is_file() and p.stat().st_size > 0:
            found[name] = p
        else:
            missing.append(p.name)
    if missing:
        raise PackagingError(
            f"missing or empty binaries in {bin_dir}: {', '.join(missing)}. "
            "Build them first: cargo build --release --locked "
            + " ".join(f"-p {'ssx-cli' if n == 'ssx' else n}" for n in BINARIES)
        )
    return found


#: ``libfoo.so.1 => /path (0x...)`` and the dynamic-loader line without ``=>``.
_LDD_LINE = re.compile(r"^\s*(\S+)\s*(?:=>\s*(\S+|not found))?")


def parse_ldd(output: str) -> list[str]:
    """Library names from ``ldd`` output, sorted, excluding the kernel vDSO and the loader."""
    libs = set()
    for line in output.splitlines():
        m = _LDD_LINE.match(line)
        if not m:
            continue
        name = m.group(1)
        # Every Linux shared-library soname contains ".so"; this also ignores ldd's prose lines
        # ("statically linked", "not a dynamic executable") and the absolute-path loader line.
        if ".so" not in name or "linux-vdso" in name or name.startswith("/"):
            continue
        libs.add(name)
    return sorted(libs)


def shared_libraries(binary: Path) -> list[str] | None:
    """Shared libraries a Linux binary needs, or ``None`` if ``ldd`` is unavailable."""
    if shutil.which("ldd") is None:
        return None
    try:
        out = subprocess.run(
            ["ldd", str(binary)], capture_output=True, text=True, timeout=60, check=False
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    return parse_ldd(out)


def check_forbidden(libs_by_binary: dict[str, list[str] | None], forbidden: list[str]) -> None:
    """Raises if any binary links a library whose name starts with a forbidden prefix."""
    bad = []
    for binary, libs in libs_by_binary.items():
        if libs is None:
            # Cannot prove anything without ldd: fail closed, the caller asked for a guarantee.
            raise PackagingError(
                f"--forbid-lib was given but the dependencies of {binary} cannot be inspected "
                "(is `ldd` installed?)"
            )
        for lib in libs:
            if any(lib.startswith(prefix) for prefix in forbidden):
                bad.append(f"{binary} -> {lib}")
    if bad:
        raise PackagingError(
            "binaries still link libraries that must be embedded:\n  " + "\n  ".join(bad)
        )


def collect_notices(root: Path) -> list[tuple[Path, str]]:
    """``(source, archive path)`` for LICENSE, README and every bundled third-party notice."""
    items: list[tuple[Path, str]] = []
    for name in ("LICENSE", "README.md"):
        p = root / name
        if not p.is_file():
            raise PackagingError(f"{p} is missing")
        items.append((p, name))
    for p in sorted(root.glob("crates/*/THIRD_PARTY*.md")):
        items.append((p, f"licenses/{p.parent.name}-{p.name}"))
    return items


def installer_for(os_name: str, root: Path) -> tuple[Path, str]:
    name = "install.sh" if os_name == "linux" else "install.ps1"
    p = root / "packaging" / name
    if not p.is_file():
        raise PackagingError(f"{p} is missing")
    return p, name


def epoch() -> int:
    return int(os.environ.get("SOURCE_DATE_EPOCH", "0"))


def build_manifest(
    version: str,
    os_name: str,
    arch: str,
    variant: str,
    binaries: dict[str, Path],
    libs: dict[str, list[str] | None],
) -> dict:
    return {
        "name": "ssx",
        "version": version,
        "os": os_name,
        "arch": arch,
        "variant": variant,
        "binaries": {
            name: {
                "file": p.name,
                "bytes": p.stat().st_size,
                "sha256": sha256_file(p),
                **({"needs": libs[name]} if libs.get(name) is not None else {}),
            }
            for name, p in binaries.items()
        },
    }


def write_tar_gz(dest: Path, top: str, files: list[tuple[str, bytes, int]]) -> None:
    """Writes a reproducible tar.gz. ``files`` are ``(path, data, mode)`` under ``top/``."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path, data, mode in sorted(files):
            info = tarfile.TarInfo(f"{top}/{path}")
            info.size = len(data)
            info.mode = mode
            info.mtime = epoch()
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            tar.addfile(info, io.BytesIO(data))
    # gzip with a fixed header timestamp, or identical input would produce different bytes.
    with dest.open("wb") as out, gzip.GzipFile(
        filename="", mode="wb", fileobj=out, mtime=epoch(), compresslevel=9
    ) as gz:
        gz.write(raw.getvalue())


def write_zip(dest: Path, top: str, files: list[tuple[str, bytes, int]]) -> None:
    """Writes a reproducible zip (fixed timestamps, sorted members)."""
    # The zip format cannot represent dates before 1980.
    stamp = max(epoch(), 315532800)
    import time

    date_time = time.gmtime(stamp)[:6]
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for path, data, mode in sorted(files):
            info = zipfile.ZipInfo(f"{top}/{path}", date_time=date_time)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (mode & 0xFFFF) << 16
            z.writestr(info, data)


def package(args: argparse.Namespace) -> Path:
    os_name = args.os
    arch = args.arch or machine_arch()
    target_dir = Path(args.target_dir)
    # `cargo build --target T` puts output in target/T/<profile>; without --target, target/<profile>.
    bin_dir = target_dir / args.target / args.profile if args.target else target_dir / args.profile
    binaries = find_binaries(bin_dir, os_name)

    libs: dict[str, list[str] | None] = {}
    if os_name == "linux":
        libs = {name: shared_libraries(p) for name, p in binaries.items()}
    if args.forbid_lib:
        check_forbidden(libs, args.forbid_lib)

    manifest = build_manifest(args.version, os_name, arch, args.variant, binaries, libs)

    files: list[tuple[str, bytes, int]] = []
    for name, p in binaries.items():
        files.append((f"bin/{p.name}", p.read_bytes(), 0o755))
    for src, rel in collect_notices(ROOT):
        files.append((rel, src.read_bytes(), 0o644))
    installer, installer_name = installer_for(os_name, ROOT)
    files.append((installer_name, installer.read_bytes(), 0o755))
    if os_name == "linux":
        uninstall = ROOT / "packaging" / "uninstall.sh"
        if not uninstall.is_file():
            raise PackagingError(f"{uninstall} is missing")
        files.append(("uninstall.sh", uninstall.read_bytes(), 0o755))
    files.append(("MANIFEST.json", (json.dumps(manifest, indent=2) + "\n").encode(), 0o644))

    top = f"ssx-{args.version}-{os_name}-{arch}-{args.variant}"
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    archive = out / (top + (".zip" if os_name == "windows" else ".tar.gz"))
    (write_zip if os_name == "windows" else write_tar_gz)(archive, top, files)
    (out / (archive.name + ".sha256")).write_text(f"{sha256_file(archive)}  {archive.name}\n")
    return archive


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--os", choices=["linux", "windows"], required=True)
    ap.add_argument("--arch", help="architecture name for the archive (default: this machine's)")
    ap.add_argument("--version", required=True, help="version string, e.g. 0.1.0 (no leading v)")
    ap.add_argument("--variant", required=True, help="build flavour, e.g. dynamic, static, lite, full")
    ap.add_argument("--target-dir", default="target", help="cargo target directory")
    ap.add_argument("--target", help="cargo --target triple, if the build used one")
    ap.add_argument("--profile", default="release")
    ap.add_argument("--out", default="dist")
    ap.add_argument(
        "--forbid-lib",
        action="append",
        default=[],
        metavar="PREFIX",
        help="Linux: fail if any binary links a shared library starting with PREFIX (repeatable)",
    )
    args = ap.parse_args(argv)
    if not re.fullmatch(r"[0-9][0-9A-Za-z.+~_-]*", args.version):
        ap.error(f"--version {args.version!r} must not start with 'v' or contain odd characters")
    if not re.fullmatch(r"[0-9A-Za-z_-]+", args.variant):
        ap.error("--variant may only contain letters, digits, '-' and '_'")
    return args


def main(argv: list[str] | None = None) -> int:
    try:
        archive = package(parse_args(argv))
    except PackagingError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
