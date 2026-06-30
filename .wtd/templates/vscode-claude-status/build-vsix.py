#!/usr/bin/env python3
"""Package this extension into a .vsix (a zip with the VSCode extension layout) using only the
stdlib — no vsce/npm needed. Run after editing extension.js / package.json:

    python build-vsix.py

It reads the version from package.json, emits the two metadata files, and writes
claude-status-<version>.vsix next to this script. install.sh installs that file via
`code --install-extension`."""
import json
import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
pkg = json.load(open(os.path.join(HERE, "package.json"), encoding="utf-8"))
VERSION = pkg["version"]
NAME, PUBLISHER = pkg["name"], pkg["publisher"]

CONTENT_TYPES = (
    '<?xml version="1.0" encoding="utf-8"?>\n'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="json" ContentType="application/json"/>'
    '<Default Extension="js" ContentType="application/javascript"/>'
    '<Default Extension="md" ContentType="text/markdown"/>'
    '<Default Extension="vsixmanifest" ContentType="text/xml"/>'
    '<Default Extension="txt" ContentType="text/plain"/></Types>'
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

out = os.path.join(HERE, f"{NAME}-{VERSION}.vsix")
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("[Content_Types].xml", CONTENT_TYPES)
    z.writestr("extension.vsixmanifest", MANIFEST)
    for f in ("package.json", "extension.js", "README.md", "LICENSE"):
        z.write(os.path.join(HERE, f), f"extension/{f}")

print(f"wrote {out}")
