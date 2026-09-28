//! Lazy, bounded UTF-8 line decoding from an owned byte source.

use std::sync::Arc;

use crate::{
    ByteReader, ByteSource, CapabilityError, IoContext, Span, Value, ValueFuture, ValueReader,
    ValueReaderFuture, ValueSource, ValueSourceFactory,
};

const DEFAULT_LINE_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// # Errors
/// Requires one byte source and a positive `maxLineBytes` no larger than 10 MiB.
pub fn invoke(
    value: &Value,
    max_line: Option<i64>,
    span: Span,
    context: &IoContext,
) -> Result<Value, CapabilityError> {
    let Value::Source(source) = value.revealed() else {
        return Err(CapabilityError::new(
            "text.lines requires a byte source",
            span,
        ));
    };
    let max_line = match max_line {
        None => DEFAULT_LINE_BYTES,
        Some(value) if value > 0 => usize::try_from(value)
            .map_err(|_| CapabilityError::new("maxLineBytes is too large", span))?,
        _ => return Err(CapabilityError::new("maxLineBytes must be positive", span)),
    };
    if max_line > MAX_LINE_BYTES {
        return Err(CapabilityError::new("maxLineBytes exceeds 10 MiB", span));
    }
    let stream = Value::Stream(Arc::new(ValueSource::new(
        Box::new(Factory {
            source: source.clone(),
            span,
            max_line,
        }),
        span,
        context,
    )));
    Ok(if value.contains_sensitive() {
        stream.sensitive()
    } else {
        stream
    })
}

struct Factory {
    source: Arc<ByteSource>,
    span: Span,
    max_line: usize,
}

impl ValueSourceFactory for Factory {
    fn open(&self, context: &IoContext) -> ValueReaderFuture<'_> {
        let context = context.clone();
        Box::pin(async move {
            let bytes = self.source.open(&context).await?;
            Ok(Box::new(Reader {
                bytes,
                chunk: Arc::from([]),
                position: 0,
                line: Vec::new(),
                max_line: self.max_line,
                span: self.span,
                done: false,
            }) as Box<dyn ValueReader>)
        })
    }
}

struct Reader {
    bytes: Box<dyn ByteReader>,
    chunk: Arc<[u8]>,
    position: usize,
    line: Vec<u8>,
    max_line: usize,
    span: Span,
    done: bool,
}

impl Reader {
    fn take_line(&mut self) -> Result<Value, CapabilityError> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| CapabilityError::new("text.lines encountered invalid UTF-8", self.span))?
            .to_owned();
        self.line.clear();
        Ok(Value::String(line))
    }
}

impl ValueReader for Reader {
    fn next_value(&mut self) -> ValueFuture<'_> {
        Box::pin(async move {
            if self.done {
                return Ok(None);
            }
            loop {
                if self.position < self.chunk.len() {
                    let byte = self.chunk[self.position];
                    self.position += 1;
                    if byte == b'\n' {
                        return self.take_line().map(Some);
                    }
                    if self.line.len() >= self.max_line {
                        return Err(CapabilityError::new(
                            "text.lines line exceeds maxLineBytes",
                            self.span,
                        ));
                    }
                    self.line.push(byte);
                    continue;
                }
                if let Some(chunk) = self.bytes.next_chunk().await? {
                    self.chunk = chunk;
                    self.position = 0;
                } else {
                    self.done = true;
                    return if self.line.is_empty() {
                        Ok(None)
                    } else {
                        self.take_line().map(Some)
                    };
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChunkFuture;

    struct Chunks(Vec<Arc<[u8]>>);
    impl ByteReader for Chunks {
        fn next_chunk(&mut self) -> ChunkFuture<'_> {
            Box::pin(async {
                Ok(if self.0.is_empty() {
                    None
                } else {
                    Some(self.0.remove(0))
                })
            })
        }
    }

    #[tokio::test]
    async fn lines_cross_chunks_and_reject_invalid_utf8() {
        let mut reader = Reader {
            bytes: Box::new(Chunks(vec![
                Arc::from(b"caf\xc3".as_slice()),
                Arc::from(b"\xa9\r\nlast".as_slice()),
            ])),
            chunk: Arc::from([]),
            position: 0,
            line: Vec::new(),
            max_line: 64,
            span: Span::default(),
            done: false,
        };
        assert_eq!(
            reader.next_value().await.unwrap(),
            Some(Value::String("café".into()))
        );
        assert_eq!(
            reader.next_value().await.unwrap(),
            Some(Value::String("last".into()))
        );
        assert_eq!(reader.next_value().await.unwrap(), None);
        let mut invalid = Reader {
            bytes: Box::new(Chunks(vec![Arc::from(b"bad\xff\n".as_slice())])),
            chunk: Arc::from([]),
            position: 0,
            line: Vec::new(),
            max_line: 64,
            span: Span::default(),
            done: false,
        };
        assert!(invalid.next_value().await.is_err());
    }
}
