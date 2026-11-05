"""Builds the Chrome Web Store upload: dist/Orexa-Downloader-Extension-<version>.zip

- Only the files the extension uses (no tests, screenshots or README).
- The development-only "key" is removed from the manifest (the Web Store gives its own ID).
- manifest.json sits at the top of the zip, with forward-slash paths.

Run:  python package-extension.py
"""
import json
import re
import sys
import zipfile
from pathlib import Path

root = Path(__file__).resolve().parent
src = root / "extension"
dist = root / "dist"

manifest = json.loads((src / "manifest.json").read_text(encoding="utf-8"))
version = manifest["version"]
manifest.pop("key", None)  # the Web Store assigns the ID

# Everything the extension loads at run time.
files = ["background.js", "content-media.js", "content-youtube.js", "dialog.html", "dialog.css", "dialog.js",
         "dialog-logic.js", "popup.html", "popup.css", "popup.js"]
files += sorted(str(p.relative_to(src)).replace("\\", "/") for p in (src / "icons").glob("*"))
files += sorted(str(p.relative_to(src)).replace("\\", "/") for p in (src / "fonts").glob("*.ttf"))

problems = []

# Every file the manifest points at must be in the package.
referenced = set()
referenced.add(manifest["background"]["service_worker"])
referenced.add(manifest["action"]["default_popup"])
referenced.update(manifest["action"]["default_icon"].values())
referenced.update(manifest["icons"].values())
for cs in manifest.get("content_scripts", []):
    referenced.update(cs.get("js", []))
    referenced.update(cs.get("css", []))
for r in sorted(referenced):
    if r not in files:
        problems.append(f"manifest points at {r}, which is not in the package")

# Every file the pages load (script/link/src/url) must exist too.
for page in ("popup.html", "dialog.html"):
    text = (src / page).read_text(encoding="utf-8")
    for m in re.finditer(r'(?:src|href)="([^"#]+)"', text):
        ref = m.group(1)
        if not re.match(r"^(https?:|data:|chrome-extension:)", ref) and ref not in files:
            problems.append(f"{page} loads {ref}, which is not in the package")
for css in ("dialog.css", "popup.css"):
    for m in re.finditer(r'url\("?([^")]+)"?\)', (src / css).read_text(encoding="utf-8")):
        ref = m.group(1)
        if not ref.startswith(("data:", "http")) and ref not in files:
            problems.append(f"{css} loads {ref}, which is not in the package")

for f in files:
    if not (src / f).is_file():
        problems.append(f"missing file: {f}")

if problems:
    print("Not built:")
    for p in problems:
        print("  -", p)
    sys.exit(1)

dist.mkdir(exist_ok=True)
out = dist / f"Orexa-Downloader-Extension-{version}.zip"
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    z.writestr("manifest.json", json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")
    for f in files:
        z.write(src / f, f)

print(f"Built {out}  ({out.stat().st_size / 1024:.0f} KB, {len(files) + 1} files)")
