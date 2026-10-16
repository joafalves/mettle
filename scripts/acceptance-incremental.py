#!/usr/bin/env python3
"""Exercise byte/value source loops and bounded protocol-neutral framing."""

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


def expect_failure(program: Path, source: str, message: str, *arguments: str,
                   command: str = "test") -> None:
    program.write_text(source)
    result = call(command, str(program), *arguments, success=False)
    output = result.stdout + result.stderr
    assert message in output, (message, output)


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

            expect_failure(program, f'''test "invalid event encoding fails" {{
    response = http.get("{base}/content/events-invalid", stream: true, headers: {{ Authorization: "Bearer local-test-token" }})
    for event in response.body {{ assert(event.data is string) }}
}}
''', "UTF-8")

            program.write_text(f'''test "one source has one consumer" {{
    response = http.get("{base}/content/events", stream: true, headers: {{ Authorization: "Bearer local-test-token" }})
    for event in response.body {{ break }}
    for event in response.body {{ assert(event.data is string) }}
}}
''')
            result = call("test", str(program), success=False)
            assert "already consumed" in result.stdout + result.stderr or "closed" in result.stdout + result.stderr, result.stdout + result.stderr

            expect_failure(program, '''flow main {
    for chunk in source { yield "abc" } { chunk }
}
''', "cannot collect results", command="check")

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

            expect_failure(program, source + '''\ntest "a framing rejects a different media type" {
    recordLines.encode({ mediaType: json.mediaType, body: { text: "wrong" } })
}
''', "conflicts with the framing's mediaType", "a framing rejects a different media type")

            expect_failure(program, source + '''\ntest "delimited framing rejects an unescaped delimiter" {
    recordLines.encode({ bodyBytes: text.encode("one\\ntwo") })
}
''', "body contains the framing delimiter", "delimited framing rejects an unescaped delimiter")

            expect_failure(program, '''framing tiny = delimited("\\n", mediaType: text.mediaType, maxFrameBytes: 3)
test "frame bound includes delimiter" {
    tiny.encode({ body: "abc" })
}
''', "maxFrameBytes")

            expect_failure(program, '''framing mixed {
    maxFrameBytes: 32
    read(input) { null }
    write(frame) { frame.bodyBytes }
}
test "custom framing requires an explicit media type" {
    mixed.encode({ body: { ok: true } })
}
''', "requires mediaType")

            # `framing` and `layout` are contextual, so ordinary names still work.
            program.write_text('''framing lines = delimited("\\n", mediaType: text.mediaType)
flow main {
    framing = { layout: "still a name" }
    lines = { frames: "a local shadows the framing" }
    echo(lines.frames)
    framing.layout
}
''')
            result = call("run", str(program))
            assert "still a name" in result.stdout, result.stdout

            for invalid, message in [
                ('framing a = lengthPrefixed(u24)', "length prefix must be one of u8"),
                ('framing a = fixed(4, maxFrameBytes: 8)', "does not accept `maxFrameBytes`"),
                ('framing a = delimited("\\n", maxFrameBytes: 1)', "leaves no room for a body"),
                ('framing a = delimited("\\n", maxFrameBytes: 99999999)', "no greater than"),
                ('framing a = chunked("\\n")', "unknown built-in framing"),
                ('framing a = delimited("\\n", mediaType: missing.mediaType)', "neither a codec"),
                ('framing a = delimited("\\n")\nframing a = fixed(2)', "declared more than once"),
                ('flow main = framing.frames(text.encode("a"))', "capability `framing` is not registered"),
            ]:
                expect_failure(program, invalid + "\nflow other = 1\n", message, command="check")

            project = Path(directory) / "project"
            project.mkdir()
            (project / "mettle.toml").write_text('name = "framing-acceptance"\n')
            (project / "codec.mettle").write_text('''namespace records
codec note {
    mediaType: "application/x-project-note"
    encode(value) { text.encode(value.message) }
    decode(bytes) { { message: text.decode(bytes) } }
}
''')
            (project / "framing.mettle").write_text('''namespace framings
use namespace records
framing lines = delimited("\\n", mediaType: note.mediaType, maxFrameBytes: 64)
''')
            (project / "main.mettle").write_text('''use namespace records
use namespace framings
use codec note
test "cross-file framing and codec" {
    for frame in lines.frames(source { yield "hello\\n" }) {
        assert(frame.body.message == "hello")
    }
}
''')
            call("test", str(project / "main.mettle"))

            expect_failure(program, source + '''
test "truncated framed payload" {
    input = source {
        yield json.encode({ mediaType: json.mediaType, length: 4 })
        yield "\\n"
        yield "ab"
    }
    for frame in mixedEvents.frames(input) { assert(false) }
}
''', "truncated frame: expected 4 bytes", "truncated framed payload")

            expect_failure(program, source + '''
test "truncated framed header" {
    for frame in recordLines.frames(source { yield "partial" }) { assert(false) }
}
''', "truncated frame: input ended before the delimiter", "truncated framed header")

            program.write_text('''framing lenient = delimited("\\n", mediaType: text.mediaType, requireFinalDelimiter: false)
test "an unterminated final frame is accepted when allowed" {
    for index, frame in lenient.frames(source { yield "a\\nb" }) {
        if (index == 0) { assert(frame.body == "a") }
        if (index == 1) { assert(frame.body == "b") }
    }
}
''')
            call("test", str(program))

            program.write_text(f'''framing packets = lengthPrefixed(u16be, mediaType: json.mediaType, maxFrameBytes: 64)
framing raw = lengthPrefixed(u16be)
framing pairs = fixed(2, mediaType: text.mediaType)
test "length-prefixed HTTP frames ignore chunk boundaries" {{
    response = http.get("{base}/content/length-prefixed", stream: true,
        headers: {{ Authorization: "Bearer local-test-token" }})
    for index, frame in raw.frames(response.chunks) {{
        if (index == 0) {{ assert(json.decode(frame.bodyBytes).id == 1) }}
        if (index == 1) {{ assert(text.decode(frame.body) == "multi\\nline") }}
        if (index == 2) {{ assert(frame.bodyBytes == text.encode("")) }}
    }}
}}
test "length-prefixed and fixed framings round trip" {{
    encoded = packets.encode({{ body: {{ ok: true }} }})
    assert(bytes.length(encoded) == 13) // 2-byte prefix + {{"ok":true}}
    for frame in packets.frames(encoded) {{ assert(frame.body.ok == true) }}
    for index, frame in pairs.frames(text.encode("abcd")) {{
        if (index == 1) {{ assert(frame.body == "cd") }}
    }}
}}
test "stopping early releases a framed stream" {{
    response = http.get("{base}/content/length-prefixed", stream: true,
        headers: {{ Authorization: "Bearer local-test-token" }})
    for frame in raw.frames(response.chunks) {{ break }}
    response.close()
}}
''')
            call("test", str(program))

            expect_failure(program, '''framing packets = lengthPrefixed(u8)
test "a prefix bounds the body length" {
    packets.encode({ bodyBytes: bytes.concat([text.encode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        text.encode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        text.encode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        text.encode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")]) })
}
''', "does not fit a u8 length prefix")

            # "zz" declares a 0x7a7a-byte body, far beyond the 16-byte bound.
            expect_failure(program, '''framing packets = lengthPrefixed(u16be, maxFrameBytes: 16)
test "a declared length is bounded before reading the body" {
    for frame in packets.frames(text.encode("zzx")) { assert(false) }
}
''', "frame length 31354 exceeds maxFrameBytes")

            expect_failure(program, '''framing stalled {
    maxFrameBytes: 32
    read(input) { return { bodyBytes: text.encode("x") } }
    write(frame) { frame.bodyBytes }
}
test "reader must advance" {
    for frame in stalled.frames(source { yield "a" }) { assert(false) }
}
''', "without consuming bytes")

            expect_failure(program, '''framing premature {
    maxFrameBytes: 32
    read(input) { null }
    write(frame) { frame.bodyBytes }
}
test "null must mean the end of input" {
    for frame in premature.frames(source { yield "a" }) { assert(false) }
}
''', "before the byte source ended")

            program.write_text('''framing guarded {
    maxFrameBytes: 8192
    read(input) {
        line = input.readUntil("\\n", maxBytes: 4096)
        if (line is null) { return null }
        header = json.decode(line)
        fail("bad token ${header.token}")
    }
    write(frame) { frame.bodyBytes }
}
test "sensitive frames redact conversion failures" {
    input = secret(source {
        yield json.encode({ token: "sensitive-record" })
        yield "\\n"
    })
    for frame in guarded.frames(input) { assert(false) }
}
''')
            result = call("test", str(program), success=False)
            assert "sensitive-record" not in result.stdout + result.stderr, result.stdout + result.stderr
            assert "REDACTED" in result.stdout + result.stderr, result.stdout + result.stderr

            expect_failure(program, '''codec invalid {
    mediaType: "application/x-invalid-records"
    encode(value) { text.encode(value as string) }
    decode(bytes) { text.decode(bytes) }
    decodeChunk(state, bytes) { [] }
}
use codec invalid
flow main = 1
''', "expected mediaType, aliases, encode, or decode", command="check")

            program.write_text('''framing mixed {
    maxFrameBytes: 8192
    read(input) {
        headerBytes = input.readUntil("\\n", maxBytes: 4096)
        if (headerBytes is null) { return null }
        header = json.decode(headerBytes)
        return { mediaType: header.mediaType, bodyBytes: input.readExactly(header.length) }
    }
    write(frame) {
        header = json.encode({ mediaType: frame.mediaType, length: bytes.length(frame.bodyBytes) })
        bytes.concat([header, text.encode("\\n"), frame.bodyBytes])
    }
}

test "custom framing and per-frame media types" {
    first = mixed.encode({ mediaType: json.mediaType, body: { ok: true } })
    second = mixed.encode({ mediaType: text.mediaType, body: "done" })
    input = source { yield first
                     yield second }
    for index, frame in mixed.frames(input) {
        if (index == 0) { assert(frame.body.ok == true) }
        if (index == 1) { assert(frame.body == "done") }
    }
}
''')
            call("test", str(program))
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    print("Incremental sources, SSE, framings, codecs, and ownership: OK")


if __name__ == "__main__":
    main()
