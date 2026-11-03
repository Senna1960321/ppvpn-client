#!/usr/bin/env python3
"""Install the maintained iOS host into Flutter's generated AppDelegate."""
from pathlib import Path
import re
import sys


def install(delegate: Path) -> None:
    code = delegate.read_text()
    runner = Path(__file__).resolve().parents[1] / "runner/ios/Runner"
    sources = [runner / name for name in ("PushInbox.swift", "PPVPNPlugin.swift")]
    plugin = "\n".join(path.read_text() for path in sources)
    imports = list(dict.fromkeys(line for line in plugin.splitlines() if line.startswith("import ")))
    body = "\n".join(line for line in plugin.splitlines() if not line.startswith("import "))
    code, registered = re.subn(
        r"^([ \t]*)GeneratedPluginRegistrant\.register\(with: ([^)]+)\)\n",
        lambda m: m.group(0) + f'{m[1]}PPVPNPlugin.register(with: {m[2]}.registrar(forPlugin: "PPVPNPlugin")!)\n',
        code, count=1, flags=re.M,
    )
    if not registered:
        raise SystemExit(f"{delegate}: no GeneratedPluginRegistrant.register to follow")
    code, installed = re.subn(
        r"^([ \t]*)return super\.application\(application, didFinishLaunchingWithOptions: launchOptions\)",
        lambda m: f"{m[1]}PPVPNPush.install()\n" + m.group(0),
        code, count=1, flags=re.M,
    )
    if not installed:
        raise SystemExit(f"{delegate}: no didFinishLaunching entry for the notification receiver")
    missing = [line for line in imports if line not in code.splitlines()]
    delegate.write_text("\n".join(missing) + "\n" + code + "\n" + body + "\n")


if __name__ == "__main__":
    install(Path(sys.argv[1]))
