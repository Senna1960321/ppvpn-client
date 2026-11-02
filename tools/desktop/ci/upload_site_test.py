"""Run the real site uploader against a fake S3 bucket; no network is used."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("upload-site.sh")
APK = "android/stable/0.4.0/1234/PPVPN-0.4.0-1234-android.apk"
FEED = "android/stable/latest.json"
POINTER = "desktop/channels/stable.json"
AWS = r'''#!/usr/bin/env python3
import hashlib, json, os, pathlib, shutil, sys
args = sys.argv[1:]
assert args[:2] == ["s3api", "--endpoint-url"], args
operation, args = args[3], args[4:]
options, positional = {}, []
while args:
    if args[0].startswith("--"):
        options[args[0]], args = args[1], args[2:]
    else:
        positional.append(args.pop(0))
root = pathlib.Path(os.environ["FAKE_SITE_BUCKET"])
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"operation": operation, **options}) + "\n")
objects = root / "objects"
key = options.get("--key", "")
if operation == "list-objects-v2":
    if os.environ.get("FAKE_SITE_LIST_DENY") == options["--prefix"]:
        sys.exit("Listing forbidden")
    names = sorted(str(p.relative_to(objects)) for p in objects.rglob("*")
                   if p.is_file() and str(p.relative_to(objects)).startswith(options["--prefix"]))
    print("\t".join(names) or "None")
elif operation == "head-bucket":
    pass
elif operation == "head-object":
    print(hashlib.sha256((objects / key).read_bytes()).hexdigest())
elif operation == "get-object":
    shutil.copyfile(objects / key, positional[0])
elif operation == "put-object":
    dest = objects / key
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(options["--body"], dest)
else:
    sys.exit("Unexpected S3 operation: " + operation)
'''


class UploadSite(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.site, self.bucket, bins = root / "site", root / "bucket", root / "bin"
        for path in (self.site, self.bucket / "objects", bins):
            path.mkdir(parents=True)
        for relative, data in ((APK, b"verified APK bytes"), (FEED, b"APK pointer"),
                               (POINTER, b"desktop pointer")):
            path = self.site / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        (bins / "aws").write_text(AWS)
        (bins / "aws").chmod(0o755)
        self.env = {**os.environ, "PATH": f"{bins}:{os.environ['PATH']}", "FAKE_SITE_BUCKET": str(self.bucket),
                    "R2_ACCOUNT_ID": "0" * 32, "R2_BUCKET": "site", "AWS_ACCESS_KEY_ID": "fixture-id",
                    "AWS_SECRET_ACCESS_KEY": "fixture-key", "AWS_DEFAULT_REGION": "auto"}

    def run_script(self, *args, **env):
        return subprocess.run(["bash", str(SCRIPT), *args, str(self.site)],
                              env={**self.env, **env}, text=True, capture_output=True)

    def calls(self, operation):
        log = self.bucket / "calls.jsonl"
        return [c for c in map(json.loads, log.read_text().splitlines()) if c["operation"] == operation] if log.exists() else []

    def existing(self, body):
        path = self.bucket / "objects" / APK
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)
        return path

    def test_plan_puts_immutable_apk_before_mutable_pointers_without_credentials(self):
        result = self.run_script("--plan", AWS_ACCESS_KEY_ID="", AWS_SECRET_ACCESS_KEY="")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertTrue(lines[0].startswith("1  public, max-age=31536000, immutable"))
        self.assertTrue(lines[0].endswith(APK))
        self.assertTrue(all(line.startswith("6  no-cache") for line in lines[1:]))
        self.assertEqual(self.calls("put-object"), [])

    def test_upload_orders_apk_before_feed_and_sets_apk_content_type(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        puts = self.calls("put-object")
        self.assertEqual([p["--key"] for p in puts], [APK, FEED, POINTER])
        self.assertEqual(puts[0]["--content-type"], "application/vnd.android.package-archive")
        self.assertEqual(puts[0]["--cache-control"], "public, max-age=31536000, immutable")
        self.assertTrue(all(p["--cache-control"] == "no-cache" for p in puts[1:]))

    def test_existing_apk_with_different_bytes_refuses_all_uploads(self):
        path = self.existing(b"different APK already published")
        for args in ((), ("--check",)):
            with self.subTest(args=args):
                result = self.run_script(*args)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("immutable file is never replaced", result.stderr)
                self.assertEqual(self.calls("put-object"), [])
                self.assertEqual(path.read_bytes(), b"different APK already published")

    def test_existing_identical_apk_is_kept(self):
        self.existing((self.site / APK).read_bytes())
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([p["--key"] for p in self.calls("put-object")], [FEED, POINTER])
        self.assertIn("keep    " + APK, result.stdout)

    def test_failed_android_listing_refuses_uploads_even_when_bucket_is_readable(self):
        result = self.run_script(FAKE_SITE_LIST_DENY="android/")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("android/", result.stderr)
        self.assertEqual(self.calls("put-object"), [])


if __name__ == "__main__":
    unittest.main()
