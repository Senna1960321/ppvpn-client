"""upload-downloads.sh against a fake `aws` that keeps the bucket in a directory."""

import hashlib
import json
import os
import pathlib
import stat
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("upload-downloads.sh")
PLATFORMS = {
    "macos-arm64": "PPVPN-0.6.0-macos-arm64.zip",
    "macos-x64": "PPVPN-0.6.0-macos-x64.zip",
    "windows-x64": "PPVPN-0.6.0-windows-x64-setup.exe",
    "linux-x64-deb": "PPVPN-0.6.0-linux-x64.deb",
    "linux-x64-rpm": "PPVPN-0.6.0-linux-x64.rpm",
}
RELEASES = "clients/desktop/native/releases/0.6.0/117"
POINTERS = "clients/desktop/native/channels/stable"

# s3api --endpoint-url <url> <operation> --bucket <b> --key <k> [options] [output file]
FAKE_AWS = r'''#!/usr/bin/env python3
import os, pathlib, shutil, sys

args = sys.argv[1:]
assert args[0] == "s3api" and args[1] == "--endpoint-url", args
operation, rest = args[3], args[4:]
options, positional = {}, []
while rest:
    if rest[0].startswith("--"):
        options[rest[0]] = rest[1]
        rest = rest[2:]
    else:
        positional.append(rest[0])
        rest = rest[1:]
root = pathlib.Path(os.environ["FAKE_R2"])
with open(root / ".calls", "a") as calls:
    calls.write(operation + " " + options.get("--key", "") + "\n")
if os.environ.get("FAKE_R2_DENY"):
    sys.exit("An error occurred (403) when calling the operation: Forbidden")
assert options["--bucket"] == "clients-bucket", options
obj = root / "objects" / options["--key"]
meta = root / "meta" / options["--key"]
if operation == "head-object":
    if not obj.exists():
        sys.stderr.write("\nAn error occurred (404) when calling the HeadObject operation: Not Found\n")
        sys.exit(254)
    if options.get("--query") == "Metadata.sha256":
        print(meta.read_text() if meta.exists() else "None")
elif operation == "get-object":
    shutil.copyfile(obj, positional[0])
elif operation == "put-object":
    obj.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(options["--body"], obj)
    (root / "cache" / options["--key"]).parent.mkdir(parents=True, exist_ok=True)
    (root / "cache" / options["--key"]).write_text(options["--cache-control"])
    content_type = root / "content-type" / options["--key"]
    content_type.parent.mkdir(parents=True, exist_ok=True)
    content_type.write_text(options["--content-type"])
    if "--metadata" in options:
        meta.parent.mkdir(parents=True, exist_ok=True)
        meta.write_text(options["--metadata"].split("=", 1)[1])
    elif meta.exists():
        meta.unlink()
else:
    sys.exit("unexpected operation " + operation)
'''


class UploadDownloads(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.tmp.name)
        self.release, self.bucket, bin_dir = root / "release", root / "r2", root / "bin"
        for directory in (self.release, self.bucket, bin_dir):
            directory.mkdir()
        aws = bin_dir / "aws"
        aws.write_text(FAKE_AWS)
        aws.chmod(aws.stat().st_mode | stat.S_IXUSR)
        self.env = dict(
            os.environ,
            PATH=f"{bin_dir}:{os.environ['PATH']}",
            FAKE_R2=str(self.bucket),
            VERSION="0.6.0",
            BUILD="117",
            CHANNEL="stable",
            R2_ACCOUNT_ID="0" * 32,
            R2_BUCKET="clients-bucket",
            AWS_ACCESS_KEY_ID="id",
            AWS_SECRET_ACCESS_KEY="secret",
        )
        for platform, name in PLATFORMS.items():
            content = f"installer of {platform}".encode()
            (self.release / name).write_bytes(content)
            self.write_meta(platform, file=name, length=len(content), sha256=hashlib.sha256(content).hexdigest())
        # What a release also holds and this upload leaves alone.
        (self.release / "SHA256SUMS").write_text("sums")
        (self.release / "ppvpn-cli-0.6.0-linux-x86_64.tar.gz").write_text("cli")

    def tearDown(self):
        self.tmp.cleanup()

    def write_meta(self, name, **changes):
        path = self.release / f"release-meta-{name}.json"
        meta = json.loads(path.read_text()) if path.exists() else {
            "schema": 1,
            "platform": name,
            "channel": "stable",
            "version": "0.6.0",
            "build": "0.6.0.117",
            "ed_signature": None if name.startswith("linux-") else "c2lnbmF0dXJl",
            "min_os": "12.0",
            "published_at": "2026-10-12T19:11:52Z",
        }
        meta.update(changes)
        path.write_text(json.dumps(meta, indent=2))

    def run_script(self, *args, **env):
        return subprocess.run(
            ["bash", str(SCRIPT), *args, str(self.release)],
            env={**self.env, **env}, capture_output=True, text=True,
        )

    def objects(self):
        root = self.bucket / "objects"
        return sorted(str(p.relative_to(root)) for p in root.rglob("*") if p.is_file()) if root.exists() else []

    def calls(self, operation):
        log = self.bucket / ".calls"
        return [line.split(" ", 1)[1] for line in log.read_text().splitlines() if line.startswith(operation + " ")] if log.exists() else []

    def test_plan_names_the_keys_without_the_bucket(self):
        env = {key: "" for key in ("R2_BUCKET", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")}
        result = self.run_script("--plan", **env)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 15)
        self.assertIn(f"release  {RELEASES}/PPVPN-0.6.0-windows-x64-setup.exe", lines)
        self.assertIn(f"release  {RELEASES}/release-meta-linux-x64-rpm.json", lines)
        self.assertIn(f"pointer  {POINTERS}/release-meta-macos-arm64.json", lines)
        self.assertEqual(self.calls("head-object"), [])

    def test_upload_puts_the_release_objects_then_the_pointers(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        expected = sorted(
            [f"{RELEASES}/{name}" for name in PLATFORMS.values()]
            + [f"{RELEASES}/release-meta-{p}.json" for p in PLATFORMS]
            + [f"{POINTERS}/release-meta-{p}.json" for p in PLATFORMS]
        )
        self.assertEqual(self.objects(), expected)
        puts = self.calls("put-object")
        self.assertTrue(all(key.startswith(RELEASES) for key in puts[:10]), puts)
        self.assertTrue(all(key.startswith(POINTERS) for key in puts[10:]), puts)
        for platform, name in PLATFORMS.items():
            source = json.loads((self.release / f"release-meta-{platform}.json").read_text())
            pointer = json.loads((self.bucket / "objects" / POINTERS / f"release-meta-{platform}.json").read_text())
            # What the backend reads: the release-meta and the installer's key.
            self.assertEqual(pointer, {**source, "path": f"{RELEASES}/{name}"})
            copy = json.loads((self.bucket / "objects" / RELEASES / f"release-meta-{platform}.json").read_text())
            self.assertEqual(copy, source)
            self.assertEqual(
                (self.bucket / "objects" / RELEASES / name).read_bytes(), (self.release / name).read_bytes()
            )
        for platform in ("macos-arm64", "macos-x64"):
            self.assertEqual((self.bucket / "content-type" / RELEASES / PLATFORMS[platform]).read_text(),
                             "application/zip")
        cache = self.bucket / "cache"
        self.assertEqual((cache / RELEASES / PLATFORMS["macos-x64"]).read_text(), "public, max-age=31536000, immutable")
        self.assertEqual((cache / POINTERS / "release-meta-macos-x64.json").read_text(), "no-cache, no-store, must-revalidate")
        self.assertIn("uploaded 10 release objects", result.stdout)

    def test_a_second_run_keeps_what_is_there(self):
        self.assertEqual(self.run_script().returncode, 0)
        (self.bucket / ".calls").unlink()
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls("put-object"), [f"{POINTERS}/release-meta-{p}.json" for p in PLATFORMS])
        self.assertIn("uploaded 0 release objects to clients-bucket, kept 10 already there", result.stdout)

    def test_a_release_object_with_other_content_is_refused_before_anything_is_written(self):
        # Uploaded some other way (no sha256 metadata): the content itself is compared.
        other = self.bucket / "objects" / RELEASES / PLATFORMS["linux-x64-rpm"]
        other.parent.mkdir(parents=True)
        other.write_text("another build's package")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a release object is never replaced", result.stderr)
        self.assertEqual(self.calls("put-object"), [])

    def test_check_reads_only_and_says_where_the_pointers_are(self):
        result = self.run_script("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"would create  {POINTERS}/release-meta-windows-x64.json", result.stdout)

        self.assertEqual(self.run_script().returncode, 0)
        (self.bucket / ".calls").unlink()
        result = self.run_script("--check", BUILD="118")
        # The files are of build 117: the release-meta decides.
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("build is 0.6.0.117, not 0.6.0.118", result.stderr)

        result = self.run_script("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("release objects already there with the same content: 10", result.stdout)
        self.assertIn(
            f"would move  {POINTERS}/release-meta-windows-x64.json  from 0.6.0 (build 0.6.0.117), "
            f"{RELEASES}/{PLATFORMS['windows-x64']} to 0.6.0 (build 0.6.0.117)",
            result.stdout,
        )
        self.assertIn("check only: wrote nothing", result.stdout)
        self.assertEqual(self.calls("put-object"), [])

    def test_missing_keys_stop_the_check_and_the_upload(self):
        for args in (("--check",), ()):
            for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "R2_BUCKET"):
                with self.subTest(args=args, name=name):
                    result = self.run_script(*args, **{name: ""})
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"{name} (", result.stderr)
                    self.assertEqual(self.calls("head-object"), [])
                    self.assertEqual(self.objects(), [])

    def test_no_access_stops_the_run(self):
        result = self.run_script(FAKE_R2_DENY="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot read", result.stderr)
        self.assertIn("403", result.stderr)
        self.assertEqual(self.objects(), [])

    def test_files_that_do_not_match_their_release_meta_are_refused(self):
        cases = {
            "length": lambda: self.write_meta("macos-x64", length=1),
            "SHA-256": lambda: self.write_meta("windows-x64", sha256="0" * 64),
            "ed_signature is missing": lambda: self.write_meta("windows-x64", ed_signature=None),
            "channel is dev": lambda: self.write_meta("linux-x64-deb", channel="dev"),
            "version is 0.5.0": lambda: self.write_meta("linux-x64-rpm", version="0.5.0"),
            "min_os is missing": lambda: self.write_meta("macos-arm64", min_os=""),
            "published_at": lambda: self.write_meta("macos-arm64", published_at="yesterday-ish"),
            "the release-meta is of macos-x64": lambda: self.write_meta("macos-arm64", platform="macos-x64"),
            "unexpected file name": lambda: self.write_meta("macos-arm64", file="../x.zip"),
            "release-meta-linux-x64-deb.json is missing": lambda: (self.release / "release-meta-linux-x64-deb.json").unlink(),
        }
        originals = {p.name: p.read_text() for p in self.release.glob("release-meta-*.json")}
        for message, change in cases.items():
            with self.subTest(message):
                change()
                result = self.run_script()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(self.objects(), [])
                for name, text in originals.items():
                    (self.release / name).write_text(text)

    def add_packages(self, signed=False):
        """The first-install packages of a release from 0.6.2 on, in both macOS metas;
        signed: with their EdDSA signatures, as releases from #548 on."""
        names = {}
        for platform in ("macos-arm64", "macos-x64"):
            name = PLATFORMS[platform].replace(".zip", ".pkg")
            content = f"package of {platform}".encode()
            (self.release / name).write_bytes(content)
            extra = {"install_ed_signature": f"signature of {name}"} if signed else {}
            self.write_meta(platform, install_file=name, install_length=len(content),
                            install_sha256=hashlib.sha256(content).hexdigest(), **extra)
            names[platform] = name
        return names

    def test_the_macos_packages_go_up_with_their_install_path(self):
        names = self.add_packages()
        plan = self.run_script("--plan")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        self.assertIn(f"release  {RELEASES}/{names['macos-x64']}", plan.stdout.splitlines())
        self.assertEqual(len(plan.stdout.splitlines()), 17)

        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        for platform, name in names.items():
            source = json.loads((self.release / f"release-meta-{platform}.json").read_text())
            pointer = json.loads((self.bucket / "objects" / POINTERS / f"release-meta-{platform}.json").read_text())
            # The ZIP stays in file/path (the Sparkle feeds); the package is install_*.
            self.assertEqual(pointer, {
                **source,
                "path": f"{RELEASES}/{PLATFORMS[platform]}",
                "install_path": f"{RELEASES}/{name}",
            })
            self.assertEqual((self.bucket / "objects" / RELEASES / name).read_bytes(), (self.release / name).read_bytes())
        for platform in ("windows-x64", "linux-x64-deb", "linux-x64-rpm"):
            pointer = json.loads((self.bucket / "objects" / POINTERS / f"release-meta-{platform}.json").read_text())
            self.assertFalse([key for key in pointer if key.startswith("install_")], platform)
        puts = self.calls("put-object")
        self.assertTrue(all(key.startswith(RELEASES) for key in puts[:12]), puts)
        self.assertTrue(all(key.startswith(POINTERS) for key in puts[12:]), puts)

    def test_the_pointers_leave_out_the_packages_eddsa_signatures(self):
        names = self.add_packages(signed=True)
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        for platform, name in names.items():
            source = json.loads((self.release / f"release-meta-{platform}.json").read_text())
            pointer = json.loads((self.bucket / "objects" / POINTERS / f"release-meta-{platform}.json").read_text())
            # The backend sees the three fields it knows; the release object keeps the signature.
            del source["install_ed_signature"]
            self.assertEqual(pointer, {
                **source,
                "path": f"{RELEASES}/{PLATFORMS[platform]}",
                "install_path": f"{RELEASES}/{name}",
            })
            uploaded = json.loads((self.bucket / "objects" / RELEASES / f"release-meta-{platform}.json").read_text())
            self.assertEqual(uploaded["install_ed_signature"], f"signature of {name}")

    def test_an_empty_package_signature_is_refused(self):
        self.add_packages(signed=True)
        self.write_meta("macos-x64", install_ed_signature="")
        for args in (("--plan",), ()):
            result = self.run_script(*args)
            self.assertNotEqual(result.returncode, 0, args)
            self.assertIn("macos-x64: install_ed_signature is empty", result.stderr)
            self.assertEqual(self.objects(), [])

    def test_install_fields_are_all_or_none_and_macos_only(self):
        names = self.add_packages()
        package = self.release / names["macos-arm64"]
        cases = {
            # Partial: the backend would answer 503 for the whole pointer.
            "must be install_file, install_length and install_sha256 together":
                lambda: self.write_meta("macos-arm64", install_sha256=None) or self.drop("macos-arm64", "install_sha256"),
            "install_path is set by this upload": lambda: self.write_meta("macos-arm64", install_path=f"{RELEASES}/x.pkg"),
            "install_length must be a number": lambda: self.write_meta("macos-x64", install_length=str(package.stat().st_size)),
            "install_sha256 is not 64 hex": lambda: self.write_meta("macos-arm64", install_sha256="0" * 63),
            "does not have the release-meta's install_sha256": lambda: self.write_meta("macos-arm64", install_sha256="0" * 64),
            "does not have the release-meta's install_length": lambda: self.write_meta("macos-arm64", install_length=1),
            "unexpected install_file": lambda: self.write_meta("macos-arm64", install_file="../x.pkg"),
            "PPVPN-0.6.0-macos-arm64.zip.pkg is missing": lambda: self.write_meta("macos-arm64", install_file="PPVPN-0.6.0-macos-arm64.zip.pkg"),
            # macOS only.
            "windows-x64: install_* fields are for macOS only":
                lambda: self.write_meta("windows-x64", install_file=names["macos-arm64"], install_length=package.stat().st_size,
                                        install_sha256=hashlib.sha256(package.read_bytes()).hexdigest()),
            "linux-x64-deb: install_* fields are for macOS only": lambda: self.write_meta("linux-x64-deb", install_file="x.pkg"),
        }
        originals = {p.name: p.read_text() for p in self.release.glob("release-meta-*.json")}
        for message, change in cases.items():
            with self.subTest(message):
                change()
                for args in (("--plan",), ()):
                    result = self.run_script(*args)
                    self.assertNotEqual(result.returncode, 0, args)
                    self.assertIn(message, result.stderr)
                    self.assertEqual(self.objects(), [])
                for name, text in originals.items():
                    (self.release / name).write_text(text)

    def drop(self, name, key):
        path = self.release / f"release-meta-{name}.json"
        meta = json.loads(path.read_text())
        del meta[key]
        path.write_text(json.dumps(meta, indent=2))

    def test_only_a_numbered_stable_build_is_published(self):
        for env, message in (
            ({"CHANNEL": "dev"}, "only the stable channel"),
            ({"BUILD": "117a"}, "BUILD is not a number"),
            ({"VERSION": "0.6"}, "VERSION is not x.y.z"),
        ):
            with self.subTest(message):
                result = self.run_script(**env)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(self.objects(), [])


if __name__ == "__main__":
    unittest.main()
