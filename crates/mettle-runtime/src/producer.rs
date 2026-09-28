//! Pull-driven language byte producers. The reader owns the execution future.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use crate::{ActiveContext, ExecutionScope, Runtime, RuntimeError};
use mettle_capability::{
    ByteReader, ByteSource, CHUNK_BYTES, CapabilityError, ChunkFuture, IoContext, ReaderFuture,
    SourceFactory, Span, Value, ValueFuture, ValueReader, ValueReaderFuture, ValueSource,
    ValueSourceFactory,
};
use mettle_compiler::{ExecutionPlan, Instruction};

#[derive(Default)]
pub(super) struct Output {
    chunk: Mutex<Option<Arc<[u8]>>>,
    acknowledged: AtomicBool,
    produced: AtomicUsize,
}

#[derive(Clone)]
pub(super) enum Emitter {
    Bytes(Arc<Output>),
    Values(Arc<ValueOutput>),
}

impl Emitter {
    pub async fn emit(&self, value: Value, span: Span) -> Result<(), CapabilityError> {
        match self {
            Self::Bytes(output) => output.emit(value, span).await,
            Self::Values(output) => output.emit(value, span).await,
        }
    }
}

#[derive(Default)]
pub(super) struct ValueOutput {
    value: Mutex<Option<Value>>,
    acknowledged: AtomicBool,
    produced: AtomicUsize,
}

impl ValueOutput {
    async fn emit(&self, value: Value, span: Span) -> Result<(), CapabilityError> {
        if value.contains_source() || value_size(&value, CHUNK_BYTES).is_none() {
            return Err(CapabilityError::new(
                "stream yield requires one complete value no larger than 64 KiB",
                span,
            ));
        }
        if self
            .produced
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(64)
        {
            tokio::task::yield_now().await;
        }
        self.acknowledged.store(false, Ordering::Release);
        *self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
        std::future::poll_fn(|_| {
            if self.acknowledged.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        Ok(())
    }
}

fn value_size(value: &Value, max: usize) -> Option<usize> {
    fn visit(value: &Value, remaining: &mut usize, depth: usize) -> Option<()> {
        if depth > 127 {
            return None;
        }
        let size = match value.revealed() {
            Value::Null | Value::Boolean(_) => 4,
            Value::Integer(_) | Value::Float(_) | Value::Duration(_) => 32,
            Value::String(text) => text.len(),
            Value::Bytes(bytes) => bytes.len(),
            Value::Array(values) => {
                *remaining = remaining.checked_sub(values.len())?;
                for value in values {
                    visit(value, remaining, depth + 1)?;
                }
                return Some(());
            }
            Value::Object(fields) => {
                *remaining = remaining.checked_sub(fields.len())?;
                for (key, value) in fields {
                    *remaining = remaining.checked_sub(key.len())?;
                    visit(value, remaining, depth + 1)?;
                }
                return Some(());
            }
            Value::Source(_) | Value::Stream(_) | Value::Deferred(_) | Value::Sensitive(_) => {
                return None;
            }
        };
        *remaining = remaining.checked_sub(size)?;
        Some(())
    }
    let mut remaining = max;
    visit(value, &mut remaining, 0)?;
    Some(max - remaining)
}

impl Output {
    pub async fn emit(&self, value: Value, span: Span) -> Result<(), CapabilityError> {
        let bytes: &[u8] = match value.revealed() {
            Value::String(text) => text.as_bytes(),
            Value::Bytes(bytes) => bytes,
            _ => return Err(CapabilityError::new("yield requires string or bytes", span)),
        };
        // Empty yields do not transfer bytes or suspend. Cooperative turns make
        // even a dense finite producer responsive to cancellation/deadlines.
        if self
            .produced
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(64)
        {
            tokio::task::yield_now().await;
        }
        for chunk in bytes.chunks(CHUNK_BYTES) {
            self.acknowledged.store(false, Ordering::Release);
            *self.chunk.lock().expect("producer chunk") = Some(Arc::from(chunk));
            std::future::poll_fn(|_| {
                if self.acknowledged.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Factory {
    runtime: Runtime,
    plan: ExecutionPlan,
    instructions: Vec<Instruction>,
    locals: Vec<Option<Value>>,
    context: ActiveContext,
    io: IoContext,
    flow_stack: Vec<String>,
    scope_path: Vec<ExecutionScope>,
    secrets: Vec<String>,
    inside_workload: bool,
    workload_id: Option<u64>,
    next_scope_id: Arc<AtomicU64>,
    span: Span,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create(
    runtime: Runtime,
    plan: ExecutionPlan,
    instructions: Vec<Instruction>,
    local_count: usize,
    mut locals: Vec<Option<Value>>,
    context: ActiveContext,
    io: &IoContext,
    flow_stack: Vec<String>,
    scope_path: Vec<ExecutionScope>,
    secrets: Vec<String>,
    inside_workload: bool,
    workload_id: Option<u64>,
    next_scope_id: Arc<AtomicU64>,
    span: Span,
) -> Value {
    locals.resize(local_count, None);
    let factory = Factory {
        runtime,
        plan,
        instructions,
        locals,
        context,
        io: io.clone(),
        flow_stack,
        scope_path,
        secrets,
        inside_workload,
        workload_id,
        next_scope_id,
        span,
    };
    // A producer may read secrets later, so conservatively protect its content.
    Value::Source(Arc::new(ByteSource::new(Box::new(factory), span, io))).sensitive()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create_values(
    runtime: Runtime,
    plan: ExecutionPlan,
    instructions: Vec<Instruction>,
    local_count: usize,
    mut locals: Vec<Option<Value>>,
    context: ActiveContext,
    io: &IoContext,
    flow_stack: Vec<String>,
    scope_path: Vec<ExecutionScope>,
    secrets: Vec<String>,
    inside_workload: bool,
    workload_id: Option<u64>,
    next_scope_id: Arc<AtomicU64>,
    span: Span,
) -> Value {
    locals.resize(local_count, None);
    let factory = Factory {
        runtime,
        plan,
        instructions,
        locals,
        context,
        io: io.clone(),
        flow_stack,
        scope_path,
        secrets,
        inside_workload,
        workload_id,
        next_scope_id,
        span,
    };
    Value::Stream(Arc::new(ValueSource::new(
        Box::new(ValueFactory(factory)),
        span,
        io,
    )))
    .sensitive()
}

impl SourceFactory for Factory {
    fn open(&self, context: &IoContext) -> ReaderFuture<'_> {
        let context = context.clone();
        let runtime = self.runtime.clone();
        let plan = self.plan.clone();
        let instructions = self.instructions.clone();
        let mut locals = self.locals.clone();
        let active = self.context.clone();
        let io = self.io.clone();
        let flow_stack = self.flow_stack.clone();
        let scope_path = self.scope_path.clone();
        let secrets = self.secrets.clone();
        let inside_workload = self.inside_workload;
        let workload_id = self.workload_id;
        let next_scope_id = self.next_scope_id.clone();
        let span = self.span;
        Box::pin(async move {
            let permit = context.reader_permit(span).await?;
            let output = Arc::new(Output::default());
            let producer_output = output.clone();
            let future = Box::pin(async move {
                let mut executor = super::Executor {
                    runtime: &runtime,
                    producer: Some(Emitter::Bytes(producer_output)),
                    breaking: false,
                    plan: &plan,
                    capabilities: &runtime.capabilities,
                    clock: runtime.clock.as_ref(),
                    observer: runtime.observer.as_ref(),
                    environment: &runtime.environment,
                    io: &io,
                    flow_stack,
                    scope_path,
                    secrets,
                    inside_workload,
                    workload_id,
                    next_scope_id,
                    test_assertions: None,
                    collect_test_expressions: false,
                };
                executor
                    .run_instructions(&instructions, &mut locals, &active, &mut Vec::new(), false)
                    .await
                    .map(|_| ())
            });
            Ok(Box::new(Reader {
                future: Some(future),
                output,
                _permit: permit,
                deadline: tokio::time::Instant::now() + mettle_capability::DEFAULT_IO_TIMEOUT,
                total: 0,
                span,
            }) as Box<dyn ByteReader>)
        })
    }
}

type Production = Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send>>;
struct Reader {
    future: Option<Production>,
    output: Arc<Output>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
    total: usize,
    span: Span,
}

impl ByteReader for Reader {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            self.output.acknowledged.store(true, Ordering::Release);
            let result = tokio::time::timeout_at(
                self.deadline,
                std::future::poll_fn(|cx| {
                    let Some(future) = self.future.as_mut() else {
                        return Poll::Ready(Ok(None));
                    };
                    match future.as_mut().poll(cx) {
                        Poll::Ready(result) => {
                            self.future = None;
                            Poll::Ready(result.map(|()| None).map_err(|error| {
                                let mut result = CapabilityError::new(error.message, error.span);
                                result.terminal = error.terminal;
                                result
                            }))
                        }
                        Poll::Pending => {
                            match self.output.chunk.lock().expect("producer chunk").take() {
                                Some(chunk) => Poll::Ready(Ok(Some(chunk))),
                                None => Poll::Pending,
                            }
                        }
                    }
                }),
            )
            .await;
            let Ok(result) = result else {
                self.future = None;
                return Err(CapabilityError::new("source producer timed out", self.span));
            };
            if let Ok(Some(chunk)) = &result {
                self.total += chunk.len();
                if self.total as u64 > mettle_capability::DEFAULT_TRANSFER_BYTES {
                    self.future = None;
                    return Err(CapabilityError::new(
                        "source producer exceeds its byte limit",
                        self.span,
                    ));
                }
            }
            result
        })
    }
}

struct ValueFactory(Factory);

impl ValueSourceFactory for ValueFactory {
    fn open(&self, context: &IoContext) -> ValueReaderFuture<'_> {
        let context = context.clone();
        let Factory {
            runtime,
            plan,
            instructions,
            mut locals,
            context: active,
            io,
            flow_stack,
            scope_path,
            secrets,
            inside_workload,
            workload_id,
            next_scope_id,
            span,
        } = self.0.clone();
        Box::pin(async move {
            let permit = context.reader_permit(span).await?;
            let output = Arc::new(ValueOutput::default());
            let producer_output = output.clone();
            let future = Box::pin(async move {
                let mut executor = super::Executor {
                    runtime: &runtime,
                    producer: Some(Emitter::Values(producer_output)),
                    breaking: false,
                    plan: &plan,
                    capabilities: &runtime.capabilities,
                    clock: runtime.clock.as_ref(),
                    observer: runtime.observer.as_ref(),
                    environment: &runtime.environment,
                    io: &io,
                    flow_stack,
                    scope_path,
                    secrets,
                    inside_workload,
                    workload_id,
                    next_scope_id,
                    test_assertions: None,
                    collect_test_expressions: false,
                };
                executor
                    .run_instructions(&instructions, &mut locals, &active, &mut Vec::new(), false)
                    .await
                    .map(|_| ())
            });
            Ok(Box::new(ValueStreamReader {
                future: Some(future),
                output,
                _permit: permit,
                deadline: tokio::time::Instant::now() + mettle_capability::DEFAULT_IO_TIMEOUT,
                total: 0,
                span,
            }) as Box<dyn ValueReader>)
        })
    }
}

struct ValueStreamReader {
    future: Option<Production>,
    output: Arc<ValueOutput>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
    total: usize,
    span: Span,
}

impl ValueReader for ValueStreamReader {
    fn next_value(&mut self) -> ValueFuture<'_> {
        Box::pin(async move {
            self.output.acknowledged.store(true, Ordering::Release);
            let result = tokio::time::timeout_at(
                self.deadline,
                std::future::poll_fn(|cx| {
                    let Some(future) = self.future.as_mut() else {
                        return Poll::Ready(Ok(None));
                    };
                    match future.as_mut().poll(cx) {
                        Poll::Ready(result) => {
                            self.future = None;
                            Poll::Ready(result.map(|()| None).map_err(|error| {
                                let mut result = CapabilityError::new(error.message, error.span);
                                result.terminal = error.terminal;
                                result
                            }))
                        }
                        Poll::Pending => match self
                            .output
                            .value
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            Some(value) => Poll::Ready(Ok(Some(value))),
                            None => Poll::Pending,
                        },
                    }
                }),
            )
            .await;
            let Ok(result) = result else {
                self.future = None;
                return Err(CapabilityError::new(
                    "value stream producer timed out",
                    self.span,
                ));
            };
            if let Ok(Some(value)) = &result {
                self.total = self
                    .total
                    .saturating_add(value_size(value, CHUNK_BYTES).unwrap_or(CHUNK_BYTES));
                if self.total as u64 > mettle_capability::DEFAULT_TRANSFER_BYTES {
                    self.future = None;
                    return Err(CapabilityError::new(
                        "value stream exceeds its transfer limit",
                        self.span,
                    ));
                }
            }
            result
        })
    }
}
