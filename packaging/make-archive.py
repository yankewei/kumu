#!/usr/bin/env python3
"""Lays out Kumu.xcarchive and ExportOptions.plist for cloud Developer ID signing.

Run from the repository root by packaging/release.sh, which exports:
KUMU_RELEASE_DIR, KUMU_VERSION, KUMU_TEAM_ID, KUMU_DEVELOPMENT_IDENTITY.
"""

import datetime
import os
import plistlib
import shutil
from pathlib import Path

root = Path(os.environ["KUMU_RELEASE_DIR"])
archive = root / "Kumu.xcarchive"
app = archive / "Products/Applications/Kumu.app"
(app / "Contents/MacOS").mkdir(parents=True)
(app / "Contents/Resources").mkdir()
shutil.copy2("target/release/kumu", app / "Contents/MacOS/kumu")
shutil.copy2("assets/AppIcon.icns", app / "Contents/Resources/AppIcon.icns")

version = os.environ["KUMU_VERSION"]
team = os.environ["KUMU_TEAM_ID"]
with open("packaging/Info.plist", "rb") as f:
    info = plistlib.load(f)
info["CFBundleShortVersionString"] = version
info["CFBundleVersion"] = version
with (app / "Contents/Info.plist").open("wb") as f:
    plistlib.dump(info, f)

with (archive / "Info.plist").open("wb") as f:
    plistlib.dump(
        {
            "ArchiveVersion": 2,
            "CreationDate": datetime.datetime.now(datetime.timezone.utc),
            "Name": "Kumu",
            "SchemeName": "Kumu",
            "ApplicationProperties": {
                "ApplicationPath": "Applications/Kumu.app",
                "CFBundleIdentifier": info["CFBundleIdentifier"],
                "CFBundleShortVersionString": version,
                "CFBundleVersion": version,
                "SigningIdentity": os.environ["KUMU_DEVELOPMENT_IDENTITY"],
                "Team": team,
            },
        },
        f,
    )

with (root / "ExportOptions.plist").open("wb") as f:
    plistlib.dump(
        {
            "method": "developer-id",
            "signingStyle": "automatic",
            "teamID": team,
            "destination": "export",
        },
        f,
    )
