//! Built-in codec capabilities exposed through the same compiler interface as I/O.

use std::sync::Arc;

use crate::content::BuiltinCodec;
#[path = "text_ops.rs"]
mod text_ops;
use crate::documentation::{DefaultValue, OperationDocumentation};

use crate::{
    Capability, CapabilityConstant, CapabilityDescriptor, CapabilityError, CapabilityFuture,
    DEFAULT_READ_BYTES, FieldSchema, Object, OperationReport, OperationSchema, ReportOutcome,
    SchemaType, Span, Value,
};

const DETAILS: &str = "Complete values are reusable and bounded. JSON/text processing rejects invalid JSON, UTF-8, numeric ranges, or nesting where applicable, with a source location. Sensitivity is preserved through encoding/decoding and reports do not capture payload contents.";
const MEDIA_TYPE_DESCRIPTION: &str = "The codec canonical media type as an ordinary string.";

const OPTIONS: &[FieldSchema] = &[FieldSchema::new("maxBytes", SchemaType::Integer)
    .documented("Positive byte bound on complete encoded output or input.")
    .with_default(DefaultValue::Bytes(crate::DEFAULT_READ_BYTES))];
const JSON_OPERATIONS: &[OperationSchema] = &[
    OperationSchema {
        name: "encode",
        documentation: OperationDocumentation {
            summary: "Encode an object, array, string, number, boolean, or null into reusable JSON bytes. Sending these bytes does not encode them a second time.",
            notes: &[],
            parameters: &[
                "Value to encode. Bytes, live sources, and durations require explicit conversion for JSON/text.",
            ],
            details: DETAILS,
            example: "json.encode({ ok: true })",
        },
        parameters: &[SchemaType::Json],
        parameter_names: &["value"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Bytes,
    },
    OperationSchema {
        name: "decode",
        documentation: OperationDocumentation {
            summary: "Parse JSON into a native object, array, scalar, or null. JSON is a representation, not a native kind. Invalid JSON or unsupported numeric ranges fail; live sources are not consumed implicitly.",
            notes: &[],
            parameters: &[
                "Complete encoded bytes; JSON/text also accept strings. Live sources are never collected implicitly.",
            ],
            details: DETAILS,
            example: "json.decode(\"{\\\"ok\\\":true}\")",
        },
        parameters: &[SchemaType::Encoded],
        parameter_names: &["content"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Value,
    },
];
const TEXT_OPERATIONS: &[OperationSchema] = &[
    OperationSchema {
        name: "encode",
        documentation: OperationDocumentation {
            summary: "Encode a string as reusable UTF-8 bytes. Other JSON-compatible values use compact JSON spelling: objects are not converted with an arbitrary display format.",
            notes: &[],
            parameters: &[
                "Value to encode. Bytes, live sources, and durations require explicit conversion for JSON/text.",
            ],
            details: DETAILS,
            example: "text.encode(\"Hello!\")",
        },
        parameters: &[SchemaType::Json],
        parameter_names: &["value"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Bytes,
    },
    OperationSchema {
        name: "decode",
        documentation: OperationDocumentation {
            summary: "Decode complete bytes into a UTF-8 string, or validate an existing string. Invalid UTF-8 fails rather than inserting replacement characters.",
            notes: &[],
            parameters: &[
                "Complete encoded bytes; JSON/text also accept strings. Live sources are never collected implicitly.",
            ],
            details: DETAILS,
            example: "text.decode(text.encode(\"Hello!\"))",
        },
        parameters: &[SchemaType::Encoded],
        parameter_names: &["content"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::String,
    },
    text_ops::SPLIT,
    text_ops::MATCHES,
    text_ops::FIND,
    text_ops::FIND_ALL,
    text_ops::REPLACE,
    OperationSchema {
        name: "lines",
        documentation: OperationDocumentation {
            summary: "Decode an owned byte source into a one-consumer stream of complete UTF-8 lines without collecting the file or response.",
            notes: &[
                "A line may cross byte chunks. LF and CRLF separators are removed; a final unterminated line is emitted. Invalid UTF-8 or an oversized line fails. A value stream cannot be used as an outbound byte body.",
            ],
            parameters: &[
                "Owned byte source, such as fs.stream(path) or response.chunks. Its aliases share one consumption claim.",
            ],
            details: "Lines are pulled only when a for loop consumes the returned value stream. Break or failure drops the reader and closes an unfinished HTTP response. There is no implicit collection or replay.",
            example: "for line in text.lines(fs.stream(\"./events.log\")) { echo(line) }",
        },
        parameters: &[SchemaType::Source],
        parameter_names: &["source"],
        options: &[FieldSchema::new("maxLineBytes", SchemaType::Integer)
            .documented("Positive upper bound for one line, default 64 KiB; at most 10 MiB.")
            .with_default(DefaultValue::Bytes(64 * 1024))],
        mutually_exclusive: &[],
        result: SchemaType::ValueStream,
    },
];
const BYTE_OPERATIONS: &[OperationSchema] = &[
    OperationSchema {
        name: "encode",
        documentation: OperationDocumentation {
            summary: "Return reusable bytes unchanged after checking the byte bound. This identity codec does not parse text, serialize objects, or consume live sources.",
            notes: &[],
            parameters: &[
                "Complete reusable bytes. No conversion or source collection is performed.",
            ],
            details: DETAILS,
            example: "bytes.encode(text.encode(\"Hello!\"))",
        },
        parameters: &[SchemaType::Bytes],
        parameter_names: &["value"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Bytes,
    },
    OperationSchema {
        name: "decode",
        documentation: OperationDocumentation {
            summary: "Return reusable bytes unchanged after checking the byte bound. This identity codec does not parse text, serialize objects, or consume live sources.",
            notes: &[],
            parameters: &[
                "Complete reusable bytes. No conversion or source collection is performed.",
            ],
            details: DETAILS,
            example: "bytes.decode(text.encode(\"Hello!\"))",
        },
        parameters: &[SchemaType::Bytes],
        parameter_names: &["content"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Bytes,
    },
    OperationSchema {
        name: "length",
        documentation: OperationDocumentation {
            summary: "Return the number of bytes in a complete byte value.",
            notes: &["This does not consume a live source or count characters in a string."],
            parameters: &["Complete reusable bytes to measure."],
            details: "Useful for explicit length-prefixed framing.",
            example: "bytes.length(json.encode({ ok: true }))",
        },
        parameters: &[SchemaType::Bytes],
        parameter_names: &["value"],
        options: &[],
        mutually_exclusive: &[],
        result: SchemaType::Integer,
    },
    OperationSchema {
        name: "concat",
        documentation: OperationDocumentation {
            summary: "Join a bounded array of complete byte values without text conversion.",
            notes: &["No live source is consumed implicitly."],
            parameters: &["Array of complete byte values in output order."],
            details: "Use when writing a length-prefixed frame from encoded header and body bytes.",
            example: "bytes.concat([text.encode(\"header\\n\"), json.encode({ ok: true })])",
        },
        parameters: &[SchemaType::Array],
        parameter_names: &["parts"],
        options: OPTIONS,
        mutually_exclusive: &[],
        result: SchemaType::Bytes,
    },
];

pub const JSON_DESCRIPTOR: CapabilityDescriptor = CapabilityDescriptor {
    removed_result_fields: &[],
    name: "json",
    description: "Protocol-independent JSON encoding and decoding for complete values.",
    constants: &[CapabilityConstant {
        name: "mediaType",
        description: MEDIA_TYPE_DESCRIPTION,
        value: BuiltinCodec::Json.media_type(),
    }],
    defaults: OPTIONS,
    operations: JSON_OPERATIONS,
};
pub const TEXT_DESCRIPTOR: CapabilityDescriptor = CapabilityDescriptor {
    removed_result_fields: &[],
    name: "text",
    description: "Protocol-independent UTF-8 codecs and bounded literal/regex string operations.",
    constants: &[CapabilityConstant {
        name: "mediaType",
        description: MEDIA_TYPE_DESCRIPTION,
        value: BuiltinCodec::Text.media_type(),
    }],
    defaults: OPTIONS,
    operations: TEXT_OPERATIONS,
};
pub const BYTES_DESCRIPTOR: CapabilityDescriptor = CapabilityDescriptor {
    removed_result_fields: &[],
    name: "bytes",
    description: "Identity codec for complete representation bytes; no implicit conversion.",
    constants: &[CapabilityConstant {
        name: "mediaType",
        description: MEDIA_TYPE_DESCRIPTION,
        value: BuiltinCodec::Bytes.media_type(),
    }],
    defaults: OPTIONS,
    operations: BYTE_OPERATIONS,
};

/// Convert complete bytes using their declared representation, independent of a protocol.
pub const CONTENT_DESCRIPTOR: CapabilityDescriptor = CapabilityDescriptor {
    removed_result_fields: &[],
    name: "content",
    description: "Decode complete representation bytes by media type using a file-registered codec or a built-in fallback.",
    constants: &[],
    defaults: &[],
    operations: &[OperationSchema {
        name: "decode",
        documentation: OperationDocumentation {
            summary: "Decode complete bytes using the registered codec for mediaType, then built-in JSON/text/bytes rules.",
            notes: &[
                "A media type is representation metadata, not a transport header. Unknown types remain bytes; no codec is loaded from a remote value.",
            ],
            parameters: &[
                "Complete encoded bytes; a live source must first be framed or collected explicitly.",
            ],
            details: "The calling file must register a custom codec with `use codec`. Decoding is bounded and preserves sensitivity.",
            example: "content.decode(json.encode({ ok: true }), mediaType: json.mediaType)",
        },
        parameters: &[SchemaType::Bytes],
        parameter_names: &["bytes"],
        options: &[
            FieldSchema::new("mediaType", SchemaType::String)
                .documented("Concrete media type selecting the decoder."),
            FieldSchema::new("maxBytes", SchemaType::Integer)
                .documented("Positive limit for complete input and decoded output.")
                .with_default(DefaultValue::Bytes(crate::DEFAULT_READ_BYTES)),
        ],
        mutually_exclusive: &[],
        result: SchemaType::Value,
    }],
};

#[derive(Debug)]
pub struct ContentCapability;

impl Capability for ContentCapability {
    fn name(&self) -> &'static str {
        "content"
    }
    fn operation_name(&self, _: usize) -> &'static str {
        "decode"
    }
    fn invoke(&self, _: usize, _: Vec<Value>, _: Object, span: Span) -> CapabilityFuture<'_> {
        Box::pin(async move {
            Err(CapabilityError::new(
                "content.decode requires an execution context",
                span,
            ))
        })
    }
    fn invoke_with_context<'a>(
        &'a self,
        operation: usize,
        arguments: Vec<Value>,
        options: Object,
        span: Span,
        context: &'a crate::IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            if operation != 0 {
                return Err(CapabilityError::new("unknown content operation", span));
            }
            let bytes = arguments
                .first()
                .ok_or_else(|| CapabilityError::new("content.decode requires bytes", span))?;
            if !matches!(bytes.revealed(), Value::Bytes(_)) {
                return Err(CapabilityError::new(
                    "content.decode requires complete bytes",
                    span,
                ));
            }
            let media = match options.get("mediaType").map(Value::revealed) {
                Some(Value::String(value)) => {
                    Some(crate::media_type::MediaType::parse(value, span)?)
                }
                None => {
                    return Err(CapabilityError::new(
                        "content.decode requires mediaType",
                        span,
                    ));
                }
                _ => return Err(CapabilityError::new("mediaType must be a string", span)),
            };
            let max_bytes = match options.get("maxBytes").map(Value::revealed) {
                None => usize::try_from(DEFAULT_READ_BYTES).expect("default read bound"),
                Some(Value::Integer(value)) if *value > 0 => usize::try_from(*value)
                    .map_err(|_| CapabilityError::new("maxBytes is too large", span))?,
                _ => {
                    return Err(CapabilityError::new(
                        "maxBytes must be a positive integer",
                        span,
                    ));
                }
            };
            crate::content::decode_media(bytes, media.as_ref(), max_bytes, span, context).await
        })
    }
    fn observed_result(&self, _: usize, result: &Value) -> Value {
        let value = Value::String(format!("{} value", result.type_name()));
        if result.contains_sensitive() {
            value.sensitive()
        } else {
            value
        }
    }
}

#[derive(Debug)]
pub struct CodecCapability(pub BuiltinCodec);
impl Capability for CodecCapability {
    fn name(&self) -> &'static str {
        match self.0 {
            BuiltinCodec::Json => "json",
            BuiltinCodec::Text => "text",
            BuiltinCodec::Bytes => "bytes",
        }
    }
    fn operation_name(&self, operation: usize) -> &'static str {
        match operation {
            0 => "encode",
            1 => "decode",
            2 if self.0 == BuiltinCodec::Text => "split",
            3 if self.0 == BuiltinCodec::Text => "matches",
            4 if self.0 == BuiltinCodec::Text => "find",
            5 if self.0 == BuiltinCodec::Text => "findAll",
            6 if self.0 == BuiltinCodec::Text => "replace",
            7 if self.0 == BuiltinCodec::Text => "lines",
            2 if self.0 == BuiltinCodec::Bytes => "length",
            3 if self.0 == BuiltinCodec::Bytes => "concat",
            _ => "unknown",
        }
    }
    fn invoke(
        &self,
        operation: usize,
        arguments: Vec<Value>,
        options: Object,
        span: Span,
    ) -> CapabilityFuture<'_> {
        Box::pin(async move {
            if self.0 == BuiltinCodec::Bytes && operation == 2 {
                let value = arguments
                    .first()
                    .ok_or_else(|| CapabilityError::new("bytes.length requires bytes", span))?;
                let Value::Bytes(bytes) = value.revealed() else {
                    return Err(CapabilityError::new(
                        "bytes.length requires complete bytes",
                        span,
                    ));
                };
                let length = i64::try_from(bytes.len())
                    .map_err(|_| CapabilityError::new("byte length is too large", span))?;
                let result = Value::Integer(length);
                return Ok(if value.contains_sensitive() {
                    result.sensitive()
                } else {
                    result
                });
            }
            if self.0 == BuiltinCodec::Bytes && operation == 3 {
                let value = arguments
                    .first()
                    .ok_or_else(|| CapabilityError::new("bytes.concat requires an array", span))?;
                let Value::Array(parts) = value.revealed() else {
                    return Err(CapabilityError::new("bytes.concat requires an array", span));
                };
                let max_bytes = match options.get("maxBytes").map(Value::revealed) {
                    None => usize::try_from(DEFAULT_READ_BYTES).expect("default content bound"),
                    Some(Value::Integer(value)) if *value > 0 => usize::try_from(*value)
                        .map_err(|_| CapabilityError::new("maxBytes is too large", span))?,
                    _ => {
                        return Err(CapabilityError::new(
                            "maxBytes must be a positive integer",
                            span,
                        ));
                    }
                };
                let mut output = Vec::new();
                for part in parts {
                    let Value::Bytes(bytes) = part.revealed() else {
                        return Err(CapabilityError::new(
                            "bytes.concat array must contain only bytes",
                            span,
                        ));
                    };
                    if bytes.len() > max_bytes.saturating_sub(output.len()) {
                        return Err(CapabilityError::new(
                            "bytes.concat output exceeds maxBytes",
                            span,
                        ));
                    }
                    output.extend_from_slice(bytes);
                }
                let result = Value::Bytes(Arc::from(output));
                return Ok(if value.contains_sensitive() {
                    result.sensitive()
                } else {
                    result
                });
            }
            if self.0 == BuiltinCodec::Text && operation >= 2 {
                if operation == 7 {
                    return Err(CapabilityError::new(
                        "text.lines requires an execution-owned I/O context",
                        span,
                    ));
                }
                let context = crate::IoContext::new(std::path::PathBuf::new());
                let result = text_ops::invoke(operation, arguments, options, span, &context).await;
                context.cleanup().await?;
                return result;
            }
            let value = arguments
                .first()
                .ok_or_else(|| CapabilityError::new("codec requires an input value", span))?;
            let max_bytes = match options.get("maxBytes").map(Value::revealed) {
                None => usize::try_from(DEFAULT_READ_BYTES).expect("default content bound"),
                Some(Value::Integer(value)) if *value > 0 => usize::try_from(*value)
                    .map_err(|_| CapabilityError::new("maxBytes is too large", span))?,
                _ => {
                    return Err(CapabilityError::new(
                        "maxBytes must be a positive integer",
                        span,
                    ));
                }
            };
            match operation {
                0 => self.0.encode(value, max_bytes, span),
                1 => self.0.decode(value, max_bytes, span),
                _ => Err(CapabilityError::new("unknown codec operation", span)),
            }
        })
    }
    fn invoke_with_context<'a>(
        &'a self,
        operation: usize,
        arguments: Vec<Value>,
        options: Object,
        span: Span,
        context: &'a crate::IoContext,
    ) -> CapabilityFuture<'a> {
        if self.0 == BuiltinCodec::Text && operation >= 2 {
            if operation == 7 {
                return Box::pin(async move {
                    let source = arguments.first().ok_or_else(|| {
                        CapabilityError::new("text.lines requires a byte source", span)
                    })?;
                    let max_line = match options.get("maxLineBytes") {
                        None => None,
                        Some(Value::Integer(value)) => Some(*value),
                        _ => {
                            return Err(CapabilityError::new(
                                "maxLineBytes must be a positive integer",
                                span,
                            ));
                        }
                    };
                    crate::text_lines::invoke(source, max_line, span, context)
                });
            }
            Box::pin(text_ops::invoke(
                operation, arguments, options, span, context,
            ))
        } else {
            self.invoke(operation, arguments, options, span)
        }
    }
    fn observed_result(&self, _operation: usize, result: &Value) -> Value {
        let value = Value::String(format!("{} value", result.type_name()));
        if result.contains_sensitive() {
            value.sensitive()
        } else {
            value
        }
    }
    fn report(&self, operation: usize, result: &Value) -> Option<OperationReport> {
        Some(OperationReport {
            summary: format!("{}.{}", self.name(), self.operation_name(operation)),
            outcome: self.observed_result(operation, result).to_string(),
            outcome_kind: ReportOutcome::Success,
            sections: Vec::new(),
            payload: None,
        })
    }
}
