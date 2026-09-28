#!/usr/bin/env python3
"""Exercise byte/value source loops and bounded incremental HTTP decoding."""

from __future__ import annotations

import importlib.util
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
        source = (ROOT / "examples/http/incremental.mettle").read_text().replace(
            "http://127.0.0.1:8080", base)
        with tempfile.TemporaryDirectory(prefix="mettle-incremental-") as directory:
            program = Path(directory) / "incremental.mettle"
            program.write_text(source)
            call("test", str(program), "--jobs", "2")

            program.write_text(f'''test "invalid event encoding fails" {{
    response = http.get("{base}/content/events-invalid", stream: true, headers: {{ Authorization: "Bearer local-test-token" }})
    for event in response.body {{ assert(event.data is string) }}
}}
''')
            result = call("test", str(program), success=False)
            assert "UTF-8" in result.stdout + result.stderr, result.stdout + result.stderr

            program.write_text(f'''test "one source has one consumer" {{
    response = http.get("{base}/content/events", stream: true, headers: {{ Authorization: "Bearer local-test-token" }})
    for event in response.body {{ break }}
    for event in response.body {{ assert(event.data is string) }}
}}
''')
            result = call("test", str(program), success=False)
            assert "already consumed" in result.stdout + result.stderr or "closed" in result.stdout + result.stderr, result.stdout + result.stderr

            program.write_text('''flow main {
    for chunk in source { yield "abc" } { chunk }
}
''')
            result = call("check", str(program), success=False)
            assert "cannot collect results" in result.stdout + result.stderr, result.stdout + result.stderr

            program.write_text('''test "line boundaries cross yields" {
    input = source { yield "ca"
                     yield "fé\\nnext" }
    for index, line in text.lines(input) {
        if (index == 0) { assert(line == "café") }
        if (index == 1) { assert(line == "next") }
    }
}
''')
            call("test", str(program))

            program.write_text('''codec invalid {
    mediaType: "application/x-invalid-records"
    encode(value) { text.encode(value as string) }
    decode(input) { stream { for line in text.lines(input) { yield line } } }
    decodeChunk(state, bytes) { [] }
}
use codec invalid
flow main = 1
''')
            result = call("check", str(program), success=False)
            assert "expected mediaType, aliases, encode, or decode" in result.stdout + result.stderr, result.stdout + result.stderr
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    print("Incremental byte/value sources, SSE, codecs, and ownership: OK")


if __name__ == "__main__":
    main()
