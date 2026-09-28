//! Bounded, pull-driven Server-Sent Events decoding for streamed HTTP bodies.

use std::sync::Arc;

use mettle_capability::{
    ByteReader, CapabilityError, IoContext, Object, Span, Value, ValueFuture, ValueReader,
    ValueReaderFuture, ValueSource, ValueSourceFactory,
};

use crate::response_stream::ResponseStream;

const MAX_LINE: usize = 64 * 1024;
const MAX_EVENT: usize = 1024 * 1024;

pub(super) fn source(response: &Arc<ResponseStream>, context: &IoContext) -> Value {
    let value = Value::Stream(Arc::new(ValueSource::new(
        Box::new(Factory(response.clone())),
        response.span(),
        context,
    )));
    if response.sensitive() {
        value.sensitive()
    } else {
        value
    }
}

struct Factory(Arc<ResponseStream>);
impl ValueSourceFactory for Factory {
    fn open(&self, context: &IoContext) -> ValueReaderFuture<'_> {
        let context = context.clone();
        Box::pin(async move {
            let source = self.0.chunks(&context);
            let Value::Source(source) = source.revealed() else {
                unreachable!()
            };
            let reader = source.open(&context).await?;
            Ok(Box::new(Reader::new(reader, self.0.span())) as Box<dyn ValueReader>)
        })
    }
}

struct Reader {
    bytes: Box<dyn ByteReader>,
    chunk: Arc<[u8]>,
    position: usize,
    line: Vec<u8>,
    data: String,
    event: String,
    id: String,
    retry: Option<i64>,
    after_cr: bool,
    first_line: bool,
    span: Span,
}

impl Reader {
    fn new(bytes: Box<dyn ByteReader>, span: Span) -> Self {
        Self {
            bytes,
            chunk: Arc::from([]),
            position: 0,
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            id: String::new(),
            retry: None,
            after_cr: false,
            first_line: true,
            span,
        }
    }

    fn feed(&mut self, byte: u8) -> Result<Option<Value>, CapabilityError> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if byte == b'\r' || byte == b'\n' {
            self.after_cr = byte == b'\r';
            return self.finish_line();
        }
        if self.line.len() >= MAX_LINE {
            return Err(CapabilityError::new("SSE line exceeds 64 KiB", self.span));
        }
        self.line.push(byte);
        Ok(None)
    }

    fn finish_line(&mut self) -> Result<Option<Value>, CapabilityError> {
        let mut line = std::str::from_utf8(&self.line)
            .map_err(|_| CapabilityError::new("SSE content is not valid UTF-8", self.span))?;
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            self.line.clear();
            if self.data.is_empty() {
                self.event.clear();
                return Ok(None);
            }
            self.data.pop(); // Last appended LF is framing, not payload.
            let data = std::mem::take(&mut self.data);
            let event = std::mem::take(&mut self.event);
            return Ok(Some(Value::Object(Object::from([
                ("data".into(), Value::String(data)),
                (
                    "event".into(),
                    Value::String(if event.is_empty() {
                        "message".into()
                    } else {
                        event
                    }),
                ),
                ("id".into(), Value::String(self.id.clone())),
                (
                    "retry".into(),
                    self.retry.map_or(Value::Null, Value::Integer),
                ),
            ]))));
        }
        if !line.starts_with(':') {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match name {
                "data" => {
                    if value.len() + 1 > MAX_EVENT.saturating_sub(self.data.len()) {
                        return Err(CapabilityError::new("SSE event exceeds 1 MiB", self.span));
                    }
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => self.event = value.to_owned(),
                "id" if !value.contains('\0') => self.id = value.to_owned(),
                "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                    if let Ok(retry) = value.parse::<i64>() {
                        self.retry = Some(retry);
                    }
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(None)
    }
}

impl ValueReader for Reader {
    fn next_value(&mut self) -> ValueFuture<'_> {
        Box::pin(async move {
            loop {
                if self.position < self.chunk.len() {
                    let byte = self.chunk[self.position];
                    self.position += 1;
                    if let Some(event) = self.feed(byte)? {
                        return Ok(Some(event));
                    }
                    continue;
                }
                let Some(chunk) = self.bytes.next_chunk().await? else {
                    // An unterminated event is not dispatched. Invalid final UTF-8
                    // is still a malformed representation rather than clean EOF.
                    std::str::from_utf8(&self.line).map_err(|_| {
                        CapabilityError::new("SSE content ends with invalid UTF-8", self.span)
                    })?;
                    return Ok(None);
                };
                self.chunk = chunk;
                self.position = 0;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mettle_capability::ChunkFuture;

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
    async fn events_cross_chunks_and_track_fields_without_reconnecting() {
        let mut reader = Reader::new(
            Box::new(Chunks(vec![
                Arc::from(b"\xef\xbb".as_slice()),
                Arc::from(b"\xbf: comment\r\nid: 7\rretry: 42\rdata: caf\xc3".as_slice()),
                Arc::from(b"\xa9\r\ndata: ready\r\n\r\ndata: last\n\n".as_slice()),
            ])),
            Span::default(),
        );
        let first = reader.next_value().await.unwrap().unwrap();
        assert_eq!(
            first.as_object().unwrap()["data"],
            Value::String("café\nready".into())
        );
        assert_eq!(first.as_object().unwrap()["id"], Value::String("7".into()));
        assert_eq!(first.as_object().unwrap()["retry"], Value::Integer(42));
        let second = reader.next_value().await.unwrap().unwrap();
        assert_eq!(
            second.as_object().unwrap()["data"],
            Value::String("last".into())
        );
        assert!(reader.next_value().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn incomplete_event_is_discarded_and_invalid_utf8_fails() {
        let mut reader = Reader::new(
            Box::new(Chunks(vec![Arc::from(b"data: incomplete\n".as_slice())])),
            Span::default(),
        );
        assert!(reader.next_value().await.unwrap().is_none());
        let mut reader = Reader::new(
            Box::new(Chunks(vec![Arc::from(b"data: bad \xff\n\n".as_slice())])),
            Span::default(),
        );
        assert!(reader.next_value().await.is_err());
    }
}
