#!/usr/bin/env python3
"""Fixture checks for Homebrew Formula generation."""

from __future__ import annotations

import hashlib
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GENERATOR = ROOT / "scripts" / "generate_homebrew_formula.py"
TAG = "v9.8.7"
VERSION = TAG[1:]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def fixture(directory: Path) -> dict[str, str]:
    digests: dict[str, str] = {}
    for target in TARGETS:
        name = f"tessivum-{VERSION}-{target}.tar.gz"
        archive = directory / name
        archive.write_bytes(f"fixture archive for {target}\n".encode())
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (directory / f"{name}.sha256").write_text(f"{digest}  {name}\n", encoding="utf-8")
        digests[target] = digest
    return digests


def generate(directory: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(GENERATOR), TAG, str(directory), str(directory / "tessivum.rb")],
        capture_output=True,
        text=True,
    )


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        digests = fixture(directory)
        result = generate(directory)
        assert result.returncode == 0, result.stderr
        formula = (directory / "tessivum.rb").read_text(encoding="utf-8")
        assert "@" not in formula
        assert "on_macos do" in formula and "on_linux do" in formula
        assert formula.count("Hardware::CPU.intel?") == 2
        assert formula.count("Hardware::CPU.arm?") == 2
        assert formula.count("url ") == 4
        assert formula.count("sha256 ") == 4
        assert 'depends_on "bun"' in formula and 'depends_on "pnpm"' in formula
        assert 'desc "Rust-native AI agent harness"' in formula
        assert formula.index('depends_on "bun"') < formula.index("on_macos do")
        assert 'libexec.install Dir["*"]' in formula
        assert formula.count('bin.install_symlink libexec/"bin/tessivum"') == 2
        assert 'bin.install_symlink libexec/"bin/tessivum" => "tsv"' in formula
        assert "bin.install " not in formula
        assert "dsh" not in formula
        assert 'shell_output("#{bin}/tessivum --version"), shell_output("#{bin}/tsv --version")' in formula
        assert 'shell_output("#{bin}/tessivum --help"), shell_output("#{bin}/tsv --help")' in formula

        for target, digest in digests.items():
            assert f"tessivum-{VERSION}-{target}.tar.gz" in formula
            assert f"https://github.com/wavetao2010/tessivum/releases/download/{TAG}/tessivum-{VERSION}-{target}.tar.gz" in formula
            assert f'sha256 "{digest}"' in formula
        missing = directory / f"tessivum-{VERSION}-{TARGETS[0]}.tar.gz.sha256"
        missing.unlink()
        assert generate(directory).returncode != 0

        fixture(directory)
        (directory / f"tessivum-{VERSION}-{TARGETS[0]}.tar.gz").unlink()
        assert generate(directory).returncode != 0

        fixture(directory)
        duplicate = directory / f"tessivum-{VERSION}-{TARGETS[0]}.tar.gz.sha256"
        duplicate.write_text(duplicate.read_text(encoding="utf-8") * 2, encoding="utf-8")
        assert generate(directory).returncode != 0

        fixture(directory)
        wrong = directory / f"tessivum-{VERSION}-{TARGETS[0]}.tar.gz.sha256"
        wrong.write_text(
            f"{'0' * 64}  tessivum-{VERSION}-{TARGETS[0]}.tar.gz\n",
            encoding="utf-8",
        )
        assert generate(directory).returncode != 0


if __name__ == "__main__":
    main()
