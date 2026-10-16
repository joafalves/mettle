#!/usr/bin/env python3
"""Exercise file-scoped custom codecs against the local HTTP fixture."""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug" / ("mettle.exe" if os.name == "nt" else "mettle")


def call(*arguments: str, success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run([str(BINARY), *arguments], cwd=ROOT, capture_output=True,
                            text=True, timeout=20)
    assert (result.returncode == 0) == success, (arguments, result.stdout, result.stderr)
    return result


def main() -> None:
    subprocess.run(["cargo", "build", "--locked", "--quiet", "-p", "mettle-cli"], cwd=ROOT, check=True)
    spec = importlib.util.spec_from_file_location("fixture", ROOT / "util/test-server/http_fixture.py")
    assert spec is not None and spec.loader is not None
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    server = fixture.FixtureServer(("127.0.0.1", 0))
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    try:
        base = f"http://127.0.0.1:{server.server_address[1]}"
        source = (ROOT / "examples/http/custom-codec.mettle").read_text().replace(
            "http://127.0.0.1:8080", base)
        with tempfile.TemporaryDirectory(prefix="mettle-codec-") as directory:
            folder = Path(directory)
            program = folder / "custom-codec.mettle"
            program.write_text(source)
            assert json.loads(call("run", str(program), "--raw").stdout) == {
                "local": {"celsius": 22}, "received": {"celsius": 23}, "sent": "24"}
            call("test", str(program))
            assert json.loads(call("run", str(program), "streaming", "--raw").stdout) == {
                "celsius": 23}

            inline = folder / "inline-codec.mettle"
            inline.write_text(source.replace("codec temperature {", "use codec temperature {", 1)
                              .replace("\nuse codec temperature\n", "\n", 1))
            assert json.loads(call("run", str(inline), "--raw").stdout) == {
                "local": {"celsius": 22}, "received": {"celsius": 23}, "sent": "24"
            }

            # A sibling project file sees the declaration but not its registration.
            project = folder / "project"
            project.mkdir()
            (project / "mettle.toml").write_text('name = "custom-codec-acceptance"\n')
            (project / "codec.mettle").write_text("namespace sensors\n" + source.split("use codec temperature", 1)[0])
            (project / "main.mettle").write_text(f'''use namespace sensors
use codec temperature
flow main {{
    local = temperature.decode(temperature.encode(21))
    assert(local.celsius == 21)
    response = http.get("{base}/content/temperature", headers: {{ Authorization: "Bearer local-test-token" }})
    response.body.celsius
}}
''')
            assert call("run", str(project / "main.mettle"), "--raw").stdout.strip() == "23"
            (project / "other.mettle").write_text(f'''flow unregistered {{
    response = http.get("{base}/content/temperature", headers: {{ Authorization: "Bearer local-test-token" }})
    response.body is bytes
}}
''')
            assert call("run", str(project / "other.mettle"), "unregistered", "--raw").stdout.strip() == "true"

            (folder / "bad.mettle").write_text(f'''codec broken {{
    mediaType: "application/vnd.example.temperature"
    encode(value) {{ "not bytes" }}
    decode(bytes) {{ text.decode(bytes) as number }}
}}
use codec broken
flow main = http.post("{base}/method", body: 23, mediaType: broken.mediaType)
''')
            failure = call("run", str(folder / "bad.mettle"), success=False)
            assert "encode must return bytes" in failure.stderr, failure.stderr

            (folder / "invalid.mettle").write_text(f'''{source.split("use codec temperature", 1)[0]}
use codec temperature
flow main = http.get("{base}/content/temperature-invalid", headers: {{ Authorization: "Bearer local-test-token" }}).body
''')
            failure = call("run", str(folder / "invalid.mettle"), success=False)
            assert "codec `temperature` conversion failed" in failure.stderr, failure.stderr
            assert "not-a-number" not in failure.stderr, failure.stderr

            (folder / "secret.mettle").write_text(f'''codec broken {{
    mediaType: "application/vnd.example.temperature"
    encode(value) {{ fail("${{value}}") }}
    decode(bytes) {{ bytes }}
}}
use codec broken
flow main = http.post("{base}/method", body: secret("local-secret-token"), mediaType: broken.mediaType)
''')
            failure = call("run", str(folder / "secret.mettle"), success=False)
            assert "local-secret-token" not in failure.stderr, failure.stderr
            assert "REDACTED" in failure.stderr or "redacted" in failure.stderr.lower(), failure.stderr

            (folder / "retry.mettle").write_text('''codec broken {
    mediaType: "application/x-retry-test"
    encode(value) { fail("conversion rejected") }
    decode(bytes) { bytes }
}
use codec broken
flow main = retry(attempts: 2) { broken.encode(23) }
''')
            failure = call("run", str(folder / "retry.mettle"), success=False)
            assert "retry" in failure.stderr.lower(), failure.stderr
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    print("Custom codecs, explicit project registration, HTTP, bounds, and redaction: OK")


if __name__ == "__main__":
    main()
