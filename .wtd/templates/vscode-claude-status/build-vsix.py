#!/usr/bin/env python3
"""Package this extension into a .vsix (a zip with the VSCode extension layout) using only the
stdlib — no vsce/npm needed. install.sh runs it and installs the result; run it by hand to test:

    python build-vsix.py [--version X.Y.Z] [--out <file.vsix>]

The version is derived: package.json's version with its patch raised by the number of commits that
touched this folder, so it moves whenever the source does. Don't bump package.json by hand, and
don't commit the .vsix (it's gitignored). Writes claude-status-<version>.vsix next to this script,
replacing any older build. The release pipeline passes --version (the release's X.Y.Z) and --out."""
import argparse
import json
import os
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))

ap = argparse.ArgumentParser()
ap.add_argument("--version", help="explicit X.Y.Z instead of the git-derived version")
ap.add_argument("--out", help="output .vsix path")
args = ap.parse_args()

# Gate: the webviews' inline <script> blocks must parse. A syntax error there (e.g. an unescaped
# apostrophe in a concatenated HTML string) ships a panel that renders but is silently dead.
if subprocess.call(["node", os.path.join(HERE, "check-webview.js")]) != 0:
    sys.exit("ABORT: webview script parse check failed — not building the vsix")
if subprocess.call(["node", os.path.join(HERE, "check-wslpath.js")]) != 0:
    sys.exit("ABORT: WSL path/command checks failed — not building the vsix")

pkg = json.load(open(os.path.join(HERE, "package.json"), encoding="utf-8"))


def derived_version(base):
    try:
        n = int(subprocess.check_output(["git", "rev-list", "--count", "HEAD", "--", "."], cwd=HERE,
                                        stderr=subprocess.DEVNULL, text=True).strip())
    except (OSError, subprocess.CalledProcessError, ValueError):
        return base
    major, minor, patch = base.split(".")
    return f"{major}.{minor}.{int(patch) + n}"


VERSION = args.version or derived_version(pkg["version"])
pkg["version"] = VERSION
NAME, PUBLISHER = pkg["name"], pkg["publisher"]

CONTENT_TYPES = (
    '<?xml version="1.0" encoding="utf-8"?>\n'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="json" ContentType="application/json"/>'
    '<Default Extension="js" ContentType="application/javascript"/>'
    '<Default Extension="md" ContentType="text/markdown"/>'
    '<Default Extension="vsixmanifest" ContentType="text/xml"/>'
    '<Default Extension="txt" ContentType="text/plain"/>'
    '<Default Extension="css" ContentType="text/css"/>'
    '<Default Extension="html" ContentType="text/html"/>'
    '<Default Extension="ttf" ContentType="font/ttf"/></Types>'
)
MANIFEST = (
    '<?xml version="1.0" encoding="utf-8"?>\n'
    '<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">'
    "<Metadata>"
    f'<Identity Language="en-US" Id="{NAME}" Version="{VERSION}" Publisher="{PUBLISHER}"/>'
    f"<DisplayName>{pkg['displayName']}</DisplayName>"
    "<Description>Worktree status + dev workflow summary (cross-platform).</Description>"
    "</Metadata>"
    '<Installation><InstallationTarget Id="Microsoft.VisualStudio.Code"/></Installation>'
    '<Assets><Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true"/></Assets>'
    "</PackageManifest>"
)

if args.out:
    out = os.path.abspath(args.out)
else:
    out = os.path.join(HERE, f"{NAME}-{VERSION}.vsix")
    for f in os.listdir(HERE):
        if f.startswith(f"{NAME}-") and f.endswith(".vsix"):
            os.remove(os.path.join(HERE, f))
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("[Content_Types].xml", CONTENT_TYPES)
    z.writestr("extension.vsixmanifest", MANIFEST)
    z.writestr("extension/package.json", json.dumps(pkg, indent=2, ensure_ascii=False) + "\n")
    for f in ("extension.js", "gitview.js", "gitparse.js", "changespanel.js", "changes.html", "settings.js", "wslpath.js", "README.md", "LICENSE"):
        z.write(os.path.join(HERE, f), f"extension/{f}")
    # bundled codicons (toolbar / row icons) + their licenses
    for f in sorted(os.listdir(os.path.join(HERE, "media"))):
        z.write(os.path.join(HERE, "media", f), f"extension/media/{f}")

print(f"wrote {out}")
