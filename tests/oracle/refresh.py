# Modified by the featherbit authors for feathercritty; see README.md for the list of changes.
"""Regenerate MANIFEST and PATCHES.diff for the vendored differential-test oracle.

Usage: python3 tests/oracle/refresh.py <pristine crate dir>

<pristine crate dir> is the published crate unpacked by Cargo, for example
~/.cargo/registry/src/index.crates.io-*/alacritty_terminal-0.26.0. MANIFEST records the SHA-256
of every vendored file as published (line endings normalized to LF); PATCHES.diff is the
unified diff from those files to the ones in this directory. tests/oracle_pristine.rs checks
both offline. See PATCHES.md.
"""

import difflib
import hashlib
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OWN_FILES = {"MANIFEST", "PATCHES.diff", "PATCHES.md", "refresh.py"}


def vendored_files(root):
    files = ["Cargo.toml", "LICENSE-APACHE"]
    for directory, _, names in os.walk(os.path.join(root, "src")):
        for name in names:
            path = os.path.relpath(os.path.join(directory, name), root)
            files.append(path.replace(os.sep, "/"))
    return sorted(files)


def read(root, path):
    with open(os.path.join(root, path), "rb") as f:
        return f.read().replace(b"\r\n", b"\n")


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    pristine = sys.argv[1]
    files = vendored_files(pristine)
    if files != vendored_files(HERE):
        sys.exit("the vendored file list differs from the pristine crate's; copy src/ again")

    manifest = []
    diff = []
    for path in files:
        before = read(pristine, path)
        after = read(HERE, path)
        manifest.append(f"{hashlib.sha256(before).hexdigest()}  {path}\n")
        if before != after:
            diff.extend(
                difflib.unified_diff(
                    before.decode().splitlines(keepends=True),
                    after.decode().splitlines(keepends=True),
                    f"a/{path}",
                    f"b/{path}",
                )
            )

    with open(os.path.join(HERE, "MANIFEST"), "w", newline="\n") as f:
        f.writelines(manifest)
    with open(os.path.join(HERE, "PATCHES.diff"), "w", newline="\n") as f:
        f.writelines(diff)


if __name__ == "__main__":
    main()
