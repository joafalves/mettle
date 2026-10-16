//! Core helper documentation beside compiler-owned intrinsic resolution.

use mettle_syntax::language::Item;

macro_rules! intrinsics {
    ($($kind:ident => ($name:literal, $form:literal, $description:literal, $example:literal, $parameters:expr)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Intrinsic { $($kind),* }
        pub const BUILTINS: &[Item] = &[$(Item { name: $name, form: $form, description: $description, example: $example, parameters: $parameters }),*];
        #[must_use]
        pub fn intrinsic(name: &str) -> Option<Intrinsic> {
            match name { $($name => Some(Intrinsic::$kind),)* _ => None }
        }
    };
}

intrinsics! {
    Environment => ("env", "env(name)", "Read a string from the execution environment, including the selected .env/profile files. The name must be a string literal; an absent variable fails. This does not mark the value secret: use senv for credentials. Named arguments and option blocks are not accepted.", "flow main = env(\"DEMO_VALUE\")", &[("name", "String literal naming the environment variable.")]),
    SecretEnvironment => ("senv", "senv(name)", "Read an environment variable and mark its value sensitive, combining env and secret. Derived values retain sensitivity and reports redact them. The name must be a string literal; an absent variable fails. This is redaction metadata, not encryption.", "flow main = senv(\"DEMO_TOKEN\")", &[("name", "String literal naming the secret environment variable.")]),
    Secret => ("secret", "secret(value)", "Mark a value sensitive without changing its underlying kind. Sensitivity follows supported transformations and redacts report/diagnostic output. This is not encryption or a separate string type. The helper accepts one positional argument and no option block.", "flow main = secret(\"demo-token\")", &[("value", "Value to mark sensitive; its underlying kind is preserved.")]),
    Echo => ("echo", "echo(value)", "Record a flow-scoped message, retaining sensitive-value redaction and logical parallel/retry labels. Use it as a standalone statement, not a result expression. Normal human reports display messages; quiet/raw modes suppress their presentation, and machine-readable reports retain captured events. Load capture is bounded and may report omissions.", "flow main { echo(\"Starting\")\n 42 }", &[("value", "Value to record in the execution report, with secrets redacted.")]),
}

#[must_use]
pub fn item(name: &str) -> Option<&'static Item> {
    BUILTINS.iter().find(|item| item.name == name)
}

/// Built-in framings. Each item's parameters, after the positional one, are
/// exactly the named arguments the compiler accepts for it.
pub const FRAMINGS: &[Item] = &[
    Item {
        name: "delimited",
        form: "delimited(delimiter, mediaType: string, maxFrameBytes: integer, requireFinalDelimiter: boolean)",
        description: "Frames end with a delimiter that is not part of the body. Reading searches across chunk boundaries and consumes the delimiter; encoding appends it and rejects a body that contains it. By default the input must end with a delimiter, so a truncated final frame is an error.",
        example: "framing lines = delimited(\"\\n\", mediaType: json.mediaType)\nflow main = lines.encode({ body: { ok: true } })",
        parameters: &[
            (
                "delimiter",
                "Nonempty string literal; its UTF-8 bytes separate frames.",
            ),
            (
                "mediaType",
                "Optional media type assigned to every frame body: a string literal, a built-in constant such as json.mediaType, or a codec's name.mediaType.",
            ),
            (
                "maxFrameBytes",
                "Optional bound for one frame including its delimiter.",
            ),
            (
                "requireFinalDelimiter",
                "Optional boolean, true by default. When false, bytes after the last delimiter at the end of input form a final frame.",
            ),
        ],
    },
    Item {
        name: "lengthPrefixed",
        form: "lengthPrefixed(prefix, mediaType: string, maxFrameBytes: integer)",
        description: "Each body follows its unsigned byte length. Reading checks the declared length against maxFrameBytes before reading the body; encoding writes the prefix and rejects a body whose length does not fit it. Input ending inside a prefix or body is a truncation error.",
        example: "framing packets = lengthPrefixed(u32be, mediaType: json.mediaType)\nflow main = packets.encode({ body: { ok: true } })",
        parameters: &[
            (
                "prefix",
                "Length encoding written before each body, named without quotes; the supported names are listed under Prefixes.",
            ),
            (
                "mediaType",
                "Optional media type assigned to every frame body: a string literal, a built-in constant such as json.mediaType, or a codec's name.mediaType.",
            ),
            (
                "maxFrameBytes",
                "Optional bound for one frame including its prefix.",
            ),
        ],
    },
    Item {
        name: "fixed",
        form: "fixed(size, mediaType: string)",
        description: "Every body has exactly `size` bytes, with no separator. The size also bounds each frame. Encoding rejects a body of any other length; input ending inside a frame is a truncation error.",
        example: "framing cells = fixed(4)\nflow main = cells.encode({ bodyBytes: text.encode(\"abcd\") })",
        parameters: &[
            (
                "size",
                "Positive integer literal: the byte length of every frame.",
            ),
            (
                "mediaType",
                "Optional media type assigned to every frame body: a string literal, a built-in constant such as json.mediaType, or a codec's name.mediaType.",
            ),
        ],
    },
];

/// Operations on a declared framing, and on a custom framing's read input.
pub const FRAMING_METHODS: &[Item] = &[
    Item {
        name: "frames",
        form: "name.frames(bytesOrSource) -> stream of { body, bodyBytes, mediaType }",
        description: "Read complete frames lazily from complete bytes or one owned byte source such as response.chunks or fs.stream(path). The stream is single-consumer and reads only as it is iterated; stopping iteration releases the source. Each body is decoded by its media type with a codec registered in the calling file, then the built-in JSON/text/bytes rules; a frame without a media type decodes as bytes. Waiting for bytes is bounded by the source itself, such as an HTTP request timeout. After an error the stream ends.",
        example: "framing lines = delimited(\"\\n\", mediaType: json.mediaType)\nflow main {\n for frame in lines.frames(text.encode(\"{\\\"id\\\":1}\\n\")) { echo(frame.body.id) }\n \"done\"\n}",
        parameters: &[(
            "bytesOrSource",
            "Complete bytes or an owned, not yet consumed byte source.",
        )],
    },
    Item {
        name: "encode",
        form: "name.encode(frame) -> bytes",
        description: "Encode one frame object into complete bytes. Supply `body` to encode it with the frame's media type, or already encoded `bodyBytes`; not both. A frame mediaType must match the framing's own mediaType when it declares one; a custom framing needs one unless the body is bytes. The framing then adds its boundary, and the complete result is bounded by maxFrameBytes.",
        example: "framing lines = delimited(\"\\n\", mediaType: json.mediaType)\nflow main = lines.encode({ body: { ok: true } })",
        parameters: &[(
            "frame",
            "Object with body or bodyBytes, and optionally mediaType.",
        )],
    },
    Item {
        name: "readUntil",
        form: "input.readUntil(delimiter, maxBytes: integer) -> bytes or null",
        description: "Inside a custom framing's read method, return the bytes before the next delimiter and consume the delimiter. Returns null at a clean end of input; input ending inside a frame is a truncation error. The result is bounded by maxBytes and by the framing's remaining maxFrameBytes.",
        example: "framing records {\n read(input) {\n  body = input.readUntil(\"\\n\")\n  if (body == null) { return null }\n  return { bodyBytes: body, mediaType: json.mediaType }\n }\n write(frame) { bytes.concat([frame.bodyBytes, text.encode(\"\\n\")]) }\n}\nflow main = records.encode({ body: { ok: true }, mediaType: json.mediaType })",
        parameters: &[
            ("delimiter", "Nonempty string or bytes that ends the value."),
            (
                "maxBytes",
                "Optional positive bound for the bytes before the delimiter.",
            ),
        ],
    },
    Item {
        name: "readExactly",
        form: "input.readExactly(count) -> bytes",
        description: "Inside a custom framing's read method, return exactly `count` bytes. Input ending first is a truncation error, and the count is bounded by the framing's remaining maxFrameBytes. Use it for fixed headers and bodies of a known length.",
        example: "framing pairs {\n read(input) { return { bodyBytes: input.readExactly(2) } }\n write(frame) { frame.bodyBytes }\n}\nflow main = pairs.encode({ bodyBytes: text.encode(\"ab\") })",
        parameters: &[("count", "Nonnegative integer number of bytes to read.")],
    },
];

/// Operations available on a custom framing's `read(input)` parameter.
#[must_use]
pub fn is_frame_reader_method(name: &str) -> bool {
    framing_method(name).is_some_and(|item| item.form.starts_with("input."))
}

/// A named argument accepted by some built-in framing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramingOption {
    MediaType,
    MaxFrameBytes,
    RequireFinalDelimiter,
}

#[must_use]
pub fn framing(name: &str) -> Option<&'static Item> {
    FRAMINGS.iter().find(|item| item.name == name)
}

#[must_use]
pub fn framing_method(name: &str) -> Option<&'static Item> {
    FRAMING_METHODS.iter().find(|item| item.name == name)
}

/// Resolve a named argument that `framing` documents as accepted.
#[must_use]
pub fn framing_option(framing_name: &str, name: &str) -> Option<FramingOption> {
    let option = match name {
        "mediaType" => FramingOption::MediaType,
        "maxFrameBytes" => FramingOption::MaxFrameBytes,
        "requireFinalDelimiter" => FramingOption::RequireFinalDelimiter,
        _ => return None,
    };
    framing(framing_name)?
        .parameters
        .iter()
        .skip(1)
        .any(|(parameter, _)| *parameter == name)
        .then_some(option)
}

/// Shared bounds rendered from runtime constants rather than repeated prose.
#[must_use]
pub fn framing_bounds(name: &str) -> String {
    use mettle_capability::framing::{
        DEFAULT_MAX_FRAME_BYTES, DEFAULT_READ_UNTIL_BYTES, LengthPrefix, MAX_FRAME_BYTES_LIMIT,
    };
    let mut output = match name {
        "readUntil" => format!("maxBytes defaults to {DEFAULT_READ_UNTIL_BYTES} bytes."),
        "readExactly" | "fixed" => format!("Frames are limited to {MAX_FRAME_BYTES_LIMIT} bytes."),
        _ => format!(
            "maxFrameBytes defaults to {DEFAULT_MAX_FRAME_BYTES} bytes and may be at most {MAX_FRAME_BYTES_LIMIT} bytes."
        ),
    };
    if name == "lengthPrefixed" {
        output.push_str("\n\nPrefixes: ");
        let prefixes = LengthPrefix::ALL
            .iter()
            .map(|prefix| {
                format!(
                    "`{}` ({} byte{})",
                    prefix.name(),
                    prefix.width(),
                    if prefix.width() == 1 { "" } else { "s" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&prefixes);
        output.push_str(". `be` is big-endian (network order) and `le` little-endian.");
    }
    output
}

fn framing_reference(name: &str) -> Option<String> {
    use std::fmt::Write as _;
    if name == "framing" {
        let mut output = mettle_syntax::language::reference(name)?;
        output.push_str("\n## Built-in framings\n");
        for item in FRAMINGS {
            let _ = writeln!(output, "\n- `{}` — `{}`", item.name, item.form);
        }
        output.push_str("\n## Operations\n");
        for item in FRAMING_METHODS {
            let _ = writeln!(output, "\n- `{}` — `{}`", item.name, item.form);
        }
        return Some(output);
    }
    let name = name.strip_prefix("framing.")?;
    let item = framing(name).or_else(|| framing_method(name))?;
    let mut output = item.reference();
    let _ = write!(output, "\n## Bounds\n\n{}\n", framing_bounds(name));
    Some(output)
}

/// Reference names may use the language namespace to avoid capability collisions.
#[must_use]
pub fn reference(name: &str) -> Option<String> {
    let name = name.strip_prefix("language.").unwrap_or(name);
    if name == "language" {
        let mut output = mettle_syntax::language::reference(name)?;
        output.push_str("\n## Core helpers\n");
        for item in BUILTINS {
            use std::fmt::Write as _;
            let _ = writeln!(output, "\n- `{}` — `{}`", item.name, item.form);
        }
        return Some(output);
    }
    item(name)
        .map(|item| item.reference())
        .or_else(|| framing_reference(name))
        .or_else(|| mettle_syntax::language::reference(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn core_helpers_have_documented_compilable_examples() {
        for item in BUILTINS {
            assert!(intrinsic(item.name).is_some());
            let program = mettle_syntax::parse(item.example).unwrap();
            crate::compile(&program).unwrap();
            assert!(!item.parameters.is_empty());
        }
    }

    #[test]
    fn framing_references_compile_and_render_shared_bounds() {
        use mettle_capability::codecs::{BYTES_DESCRIPTOR, JSON_DESCRIPTOR, TEXT_DESCRIPTOR};
        use mettle_capability::framing::{DEFAULT_MAX_FRAME_BYTES, LengthPrefix};
        let capabilities = [JSON_DESCRIPTOR, TEXT_DESCRIPTOR, BYTES_DESCRIPTOR];
        for item in FRAMINGS.iter().chain(FRAMING_METHODS) {
            let program = mettle_syntax::parse(item.example)
                .unwrap_or_else(|error| panic!("{}: {error:?}", item.name));
            crate::compile_with_capabilities(&program, &capabilities)
                .unwrap_or_else(|errors| panic!("{}: {errors:?}", item.name));
            let reference = reference(&format!("framing.{}", item.name)).unwrap();
            assert!(reference.contains("## Bounds"), "{}", item.name);
        }
        for item in FRAMINGS {
            for (parameter, _) in &item.parameters[1..] {
                assert!(
                    framing_option(item.name, parameter).is_some(),
                    "{parameter}"
                );
            }
        }
        assert!(framing_option("fixed", "maxFrameBytes").is_none());
        let prefixed = reference("framing.lengthPrefixed").unwrap();
        for prefix in LengthPrefix::ALL {
            assert!(prefixed.contains(&format!("`{}`", prefix.name())));
        }
        assert!(
            reference("framing.delimited")
                .unwrap()
                .contains(&DEFAULT_MAX_FRAME_BYTES.to_string())
        );
        let overview = reference("framing").unwrap();
        assert!(overview.contains("lengthPrefixed") && overview.contains("readExactly"));
    }
}
