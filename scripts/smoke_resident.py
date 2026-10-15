"""Non-capturing packaged macOS smoke; never reads screen/clipboard content."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

app = Path(sys.argv[1]).resolve()
exe = app / "Contents/MacOS/clipocr"
root = Path.home() / "Library/Application Support/ca.clipocr.desktop"
address = root / "cli.sock"

for args, expected in [(["--help"], 0), (["--capture-ocr-selftest"], 6), (["--capture-ocr", "--engine=remote"], 6)]:
    result = subprocess.run([str(exe), *args], capture_output=True, timeout=5, check=False)
    assert result.returncode == expected, (args, result.returncode)
print("Release CLI help, dev-only flag rejection, and extra-argument rejection: PASS")

probe = socket.socket(socket.AF_UNIX)
try:
    probe.connect(str(address))
except OSError:
    pass
else:
    raise RuntimeError("An existing resident is running; quit it before this smoke test")
finally:
    probe.close()

process = subprocess.Popen([str(exe)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    deadline = time.monotonic() + 15
    while True:
        assert process.poll() is None, "Packaged app exited before ready"
        connection = socket.socket(socket.AF_UNIX)
        try:
            connection.connect(str(address))
            break
        except OSError:
            connection.close()
            assert time.monotonic() < deadline, "Resident readiness timeout"
            time.sleep(0.05)
    with connection:
        connection.settimeout(5)
        connection.sendall(b"--quit\n")  # Intentionally invalid; NEVER captures or quits.
        connection.shutdown(socket.SHUT_WR)
        result = bytearray()
        while chunk := connection.recv(4096):
            result.extend(chunk)
        out = json.loads(result)
        assert out["status"] == "failed" and out["reason"] == "unavailable"
        assert "text" not in out
    assert process.poll() is None
    for name in [root, root / "capture-tmp"]:
        metadata = name.lstat()
        assert metadata.st_uid == os.geteuid() and metadata.st_mode & 0o7777 == 0o700
    assert not list((root / "capture-tmp").iterdir())
    duplicate = subprocess.run([str(exe)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, check=False)
    assert duplicate.returncode == 0, "Duplicate GUI launch should delegate and exit"
    assert process.poll() is None
    print("Packaged tray/plugin startup, private socket rejection, 0700 storage, empty capture directory, duplicate-instance handling: PASS")
finally:
    # This smoke never requests a capture. Terminate ONLY the child it launched.
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
print("Smoke-owned resident stopped; no screenshots or clipboard contents were accessed")
