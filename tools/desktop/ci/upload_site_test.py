"""The site uploader preserves old archives and serves ZIP updates as ZIPs."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("upload-site.sh")
FAKE_AWS = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
op = args[3]
opts = dict(zip(args[4::2], args[5::2]))
root = pathlib.Path(os.environ["FAKE_R2"])
if op == "list-objects-v2":
    # The old DMG is in the bucket but is not part of this publication.
    print("desktop/stable/0.6.0/1/PPVPN.dmg" if opts["--prefix"] == "desktop/" else "None")
elif op == "head-bucket":
    pass
elif op == "put-object":
    with (root / "puts.jsonl").open("a") as out:
        out.write(json.dumps(opts) + "\n")
else:
    sys.exit("unexpected operation: " + op)
'''


class UploadSite(unittest.TestCase):
    def test_archives_upload_before_pointer_with_their_content_types(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            site = root / "site"
            names = {
                "desktop/stable/0.7.5/123/PPVPN-macos-arm64.zip": "application/zip",
                "desktop/stable/0.7.5/123/PPVPN-macos-x64.zip": "application/zip",
                "desktop/stable/0.7.5/123/PPVPN-macos-arm64.pkg": "application/octet-stream",
                "desktop/stable/0.6.2/2/PPVPN.dmg": "application/x-apple-diskimage",
                "desktop/channels/stable.json": "application/json",
            }
            for name in names:
                target = site / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(name)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            aws = bin_dir / "aws"
            aws.write_text(FAKE_AWS)
            aws.chmod(0o755)
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", FAKE_R2=str(root),
                       R2_ACCOUNT_ID="0" * 32, R2_BUCKET="test", AWS_ACCESS_KEY_ID="test",
                       AWS_SECRET_ACCESS_KEY="test")
            result = subprocess.run(["bash", str(SCRIPT), str(site)], env=env,
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            puts = [json.loads(line) for line in (root / "puts.jsonl").read_text().splitlines()]
            self.assertEqual(puts[-1]["--key"], "desktop/channels/stable.json")
            self.assertEqual({p["--key"]: p["--content-type"] for p in puts}, names)
            self.assertTrue(all(p["--cache-control"] == "public, max-age=31536000, immutable"
                                for p in puts[:-1]))
            self.assertEqual(puts[-1]["--cache-control"], "no-cache")
            self.assertNotIn("desktop/stable/0.6.0/1/PPVPN.dmg", [p["--key"] for p in puts])
