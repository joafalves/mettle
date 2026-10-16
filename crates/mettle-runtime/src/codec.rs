//! Execute compile-validated complete-value codecs inside their owning entry.

use std::sync::Arc;

use mettle_capability::{
    CapabilityError, CapabilityFuture, CodecBinding, ContentCodec, IoContext, Span, Value,
};
use mettle_compiler::ExecutionPlan;

use crate::{ActiveContext, Runtime};

const MAX_CODEC_BYTES: usize = 10 * 1024 * 1024;

struct Handler {
    name: String,
    runtime: Runtime,
    plan: Arc<ExecutionPlan>,
    encode_flow: usize,
    decode_flow: usize,
}

#[derive(Clone, Copy)]
enum Mode {
    Encode,
    Decode,
}

pub(super) fn install(
    engine: &Runtime,
    plan: &Arc<ExecutionPlan>,
    io: &IoContext,
) -> Result<(), CapabilityError> {
    if plan.codecs.is_empty() {
        return Ok(());
    }
    let mut bindings = Vec::new();
    for codec in &plan.codecs {
        let handler: Arc<dyn ContentCodec> = Arc::new(Handler {
            name: codec.name.clone(),
            runtime: engine.clone(),
            plan: plan.clone(),
            encode_flow: codec.encode_flow,
            decode_flow: codec.decode_flow,
        });
        for media_type in &codec.media_types {
            bindings.push(CodecBinding {
                source: codec.source,
                media_type: media_type.clone(),
                codec: handler.clone(),
            });
        }
    }
    io.install_codecs(bindings)
}

impl Handler {
    async fn execute(
        &self,
        flow: usize,
        input: Value,
        max_bytes: usize,
        span: Span,
        io: &IoContext,
        mode: Mode,
    ) -> Result<Value, CapabilityError> {
        let max_bytes = max_bytes.min(MAX_CODEC_BYTES);
        let mut secrets = Vec::new();
        input.visit_sensitive_strings(&mut |secret| {
            if !secret.is_empty() && !secrets.iter().any(|known| known == secret) {
                secrets.push(secret.to_owned());
            }
        });
        let sensitive = input.contains_sensitive();
        if bounded_size(&input, max_bytes).is_err() {
            return Err(CapabilityError::new(
                format!(
                    "codec `{}` input exceeds the {max_bytes} byte complete-value limit",
                    self.name
                ),
                span,
            ));
        }
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
            // Conversion is part of its calling operation, not a second user
            // action or retained payload-bearing observer event.
            inside_workload: true,
            workload_id: None,
            flow_stack: Vec::new(),
            scope_path: Vec::new(),
            next_scope_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            secrets,
            test_assertions: None,
            collect_test_expressions: false,
        };
        let result = tokio::time::timeout(
            mettle_capability::DEFAULT_IO_TIMEOUT,
            executor.execute_flow(flow, vec![input], ActiveContext::default()),
        )
        .await
        .map_err(|_| {
            CapabilityError::new(format!("codec `{}` conversion timed out", self.name), span)
        })?
        .map_err(|error| {
            CapabilityError::new(
                format!("codec `{}` conversion failed: {}", self.name, error.message),
                error.span,
            )
        })?;
        if result.contains_source() {
            return Err(CapabilityError::new(
                format!(
                    "codec `{}` cannot return a live source or response",
                    self.name
                ),
                span,
            ));
        }
        if matches!(mode, Mode::Encode) && !matches!(result.revealed(), Value::Bytes(_)) {
            return Err(CapabilityError::new(
                format!("codec `{}` encode must return bytes", self.name),
                span,
            ));
        }
        if bounded_size(&result, max_bytes).is_err() {
            return Err(CapabilityError::new(
                format!(
                    "codec `{}` output exceeds the {max_bytes} byte complete-value limit",
                    self.name
                ),
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

impl ContentCodec for Handler {
    fn encode<'a>(
        &'a self,
        value: Value,
        max_bytes: usize,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            self.execute(
                self.encode_flow,
                value,
                max_bytes,
                span,
                context,
                Mode::Encode,
            )
            .await
        })
    }
    fn decode<'a>(
        &'a self,
        bytes: Value,
        max_bytes: usize,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            if !matches!(bytes.revealed(), Value::Bytes(_)) {
                return Err(CapabilityError::new(
                    "codec decode expects complete bytes",
                    span,
                ));
            }
            if bounded_size(&bytes, max_bytes.min(MAX_CODEC_BYTES)).is_err() {
                return Err(CapabilityError::new(
                    format!(
                        "codec `{}` input exceeds the bounded complete-value limit",
                        self.name
                    ),
                    span,
                ));
            }
            self.execute(
                self.decode_flow,
                bytes,
                max_bytes,
                span,
                context,
                Mode::Decode,
            )
            .await
        })
    }
}

fn bounded_size(value: &Value, max: usize) -> Result<usize, ()> {
    fn visit(value: &Value, remaining: &mut usize, depth: usize) -> Result<(), ()> {
        if depth > 127 {
            return Err(());
        }
        let size = match value.revealed() {
            Value::Null | Value::Boolean(_) => 4,
            Value::Integer(_) | Value::Float(_) | Value::Duration(_) => 32,
            Value::String(value) => value.len(),
            Value::Bytes(value) => value.len(),
            Value::Array(values) => {
                *remaining = remaining.checked_sub(values.len()).ok_or(())?;
                for value in values {
                    visit(value, remaining, depth + 1)?;
                }
                return Ok(());
            }
            Value::Object(values) => {
                *remaining = remaining.checked_sub(values.len()).ok_or(())?;
                for (key, value) in values {
                    *remaining = remaining.checked_sub(key.len()).ok_or(())?;
                    visit(value, remaining, depth + 1)?;
                }
                return Ok(());
            }
            Value::Source(_)
            | Value::Stream(_)
            | Value::FrameReader(_)
            | Value::Deferred(_)
            | Value::Sensitive(_) => {
                return Err(());
            }
        };
        *remaining = remaining.checked_sub(size).ok_or(())?;
        Ok(())
    }
    let mut remaining = max;
    visit(value, &mut remaining, 0)?;
    Ok(max - remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_codec_budget_counts_nested_content_and_rejects_excess() {
        let value = Value::Object(mettle_capability::Object::from([(
            "temperature".to_owned(),
            Value::Array(vec![Value::String("23".to_owned())]),
        )]));
        assert!(bounded_size(&value, 128).is_ok());
        assert!(bounded_size(&value, 8).is_err());
        assert!(bounded_size(&value.clone().sensitive(), 128).is_ok());
    }
}
