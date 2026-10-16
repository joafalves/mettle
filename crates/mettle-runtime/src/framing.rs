//! Compile-validated framings owned by one execution entry. Built-in framings
//! run in the capability crate; custom ones run their pure read/write flows.

use std::sync::Arc;

use mettle_capability::framing::{
    BuiltinFramer, ContentFramer, DEFAULT_READ_UNTIL_BYTES, FrameReader, FramerBinding,
};
use mettle_capability::{CapabilityError, CapabilityFuture, IoContext, Object, Span, Value};
use mettle_compiler::{ExecutionPlan, FramingKind};

use crate::{ActiveContext, Runtime};

struct Custom {
    name: String,
    max_frame_bytes: usize,
    read_flow: usize,
    write_flow: usize,
    runtime: Runtime,
    plan: Arc<ExecutionPlan>,
}

pub(super) fn install(
    engine: &Runtime,
    plan: &Arc<ExecutionPlan>,
    io: &IoContext,
) -> Result<(), CapabilityError> {
    if plan.framers.is_empty() {
        return Ok(());
    }
    let bindings = plan
        .framers
        .iter()
        .map(|framer| FramerBinding {
            name: framer.name.clone(),
            framer: match &framer.framing {
                FramingKind::Builtin(framing) => Arc::new(BuiltinFramer {
                    framing: framing.clone(),
                    media_type: framer.media_type.clone(),
                    max_frame_bytes: framer.max_frame_bytes,
                }) as Arc<dyn ContentFramer>,
                FramingKind::Custom {
                    read_flow,
                    write_flow,
                } => Arc::new(Custom {
                    name: framer.name.clone(),
                    max_frame_bytes: framer.max_frame_bytes,
                    read_flow: *read_flow,
                    write_flow: *write_flow,
                    runtime: engine.clone(),
                    plan: plan.clone(),
                }),
            },
        })
        .collect();
    io.install_framers(bindings)
}

impl Custom {
    async fn execute(
        &self,
        flow: usize,
        input: Value,
        span: Span,
        io: &IoContext,
        allow_byte_source: bool,
    ) -> Result<Value, CapabilityError> {
        let mut secrets = Vec::new();
        input.visit_sensitive_strings(&mut |secret| {
            if !secret.is_empty() && !secrets.iter().any(|known| known == secret) {
                secrets.push(secret.to_owned());
            }
        });
        let sensitive = input.contains_sensitive();
        let mut executor = super::Executor {
            runtime: &self.runtime,
            producer: None,
            breaking: false,
            plan: &self.plan,
            capabilities: &self.runtime.capabilities,
            clock: self.runtime.clock.as_ref(),
            observer: self.runtime.observer.as_ref(),
            environment: &self.runtime.environment,
            io,
            inside_workload: true,
            workload_id: None,
            flow_stack: Vec::new(),
            scope_path: Vec::new(),
            next_scope_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            secrets,
            test_assertions: None,
            collect_test_expressions: false,
        };
        let result = executor
            .execute_flow(flow, vec![input], ActiveContext::default())
            .await
            .map_err(|error| {
                CapabilityError::new(
                    format!("framing `{}` failed: {}", self.name, error.message),
                    error.span,
                )
            })?;
        if result.contains_source()
            && !(allow_byte_source && matches!(result.revealed(), Value::Source(_)))
        {
            return Err(CapabilityError::new(
                "framing method cannot return a live source or reader",
                span,
            ));
        }
        Ok(if sensitive {
            result.sensitive()
        } else {
            result
        })
    }
}

impl ContentFramer for Custom {
    fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    fn fixed_media_type(&self) -> Option<&str> {
        None
    }

    fn read<'a>(
        &'a self,
        reader: Arc<FrameReader>,
        sensitive: bool,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            // No extra deadline: read waits on the source, whose own timeout
            // bounds idle streams, and pure bodies cannot loop unboundedly.
            let input = Value::FrameReader(reader);
            let input = if sensitive { input.sensitive() } else { input };
            self.execute(self.read_flow, input, span, context, false)
                .await
        })
    }

    fn write<'a>(
        &'a self,
        frame: Value,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(
                mettle_capability::DEFAULT_IO_TIMEOUT,
                self.execute(self.write_flow, frame, span, context, true),
            )
            .await
            .map_err(|_| {
                CapabilityError::new(format!("framing `{}` write timed out", self.name), span)
            })?
        })
    }
}

/// Execute `input.readUntil(...)` or `input.readExactly(...)` in a custom read.
pub(super) async fn reader_call(
    reader: &Arc<FrameReader>,
    method: &str,
    arguments: Vec<Value>,
    named: Object,
    span: Span,
) -> Result<Value, CapabilityError> {
    match method {
        "readUntil" => {
            if arguments.len() != 1 || named.keys().any(|key| key != "maxBytes") {
                return Err(CapabilityError::new(
                    "readUntil expects a delimiter and an optional maxBytes",
                    span,
                ));
            }
            let delimiter = match arguments[0].revealed() {
                Value::String(value) => value.as_bytes(),
                Value::Bytes(value) => value,
                _ => {
                    return Err(CapabilityError::new(
                        "readUntil delimiter must be a string or bytes",
                        span,
                    ));
                }
            };
            let max_bytes = match named.get("maxBytes").map(Value::revealed) {
                Some(Value::Integer(value)) if *value > 0 => {
                    usize::try_from(*value).unwrap_or(usize::MAX)
                }
                None => DEFAULT_READ_UNTIL_BYTES,
                _ => {
                    return Err(CapabilityError::new(
                        "maxBytes must be a positive integer",
                        span,
                    ));
                }
            };
            Ok(reader
                .read_until(delimiter, max_bytes, false, span)
                .await?
                .map_or(Value::Null, Value::Bytes))
        }
        "readExactly" => {
            if arguments.len() != 1 || !named.is_empty() {
                return Err(CapabilityError::new(
                    "readExactly expects one byte count",
                    span,
                ));
            }
            let count = match arguments[0].revealed() {
                Value::Integer(count) => usize::try_from(*count).ok(),
                _ => None,
            }
            .ok_or_else(|| {
                CapabilityError::new("readExactly count must be a nonnegative integer", span)
            })?;
            Ok(Value::Bytes(reader.read_exactly(count, span).await?))
        }
        _ => Err(CapabilityError::new(
            format!("framing input has no operation `{method}`"),
            span,
        )),
    }
}
