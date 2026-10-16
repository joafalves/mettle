//! Protocol-independent framing: complete frames read from owned byte sources
//! and encoded back to bytes. Frame boundaries are chosen by a framing, never
//! inferred from transport chunks.

use std::fmt;
use std::sync::Arc;

use crate::media_type::MediaType;
use crate::{
    ByteReader, ByteSource, CapabilityError, CapabilityFuture, IoContext, Span, Value, ValueFuture,
    ValueReader, ValueReaderFuture, ValueSource, ValueSourceFactory,
};

/// Complete-frame bound used when a framing does not declare `maxFrameBytes`.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Largest accepted `maxFrameBytes`, matching the complete-conversion bound.
#[allow(clippy::cast_possible_truncation)]
pub const MAX_FRAME_BYTES_LIMIT: usize = crate::DEFAULT_READ_BYTES as usize;
/// Default `maxBytes` for a custom framing's `readUntil`.
pub const DEFAULT_READ_UNTIL_BYTES: usize = 64 * 1024;

/// Unsigned integer encoding of a frame's body length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LengthPrefix {
    U8,
    U16Be,
    U16Le,
    U32Be,
    U32Le,
    U64Be,
    U64Le,
}

impl LengthPrefix {
    pub const ALL: &[Self] = &[
        Self::U8,
        Self::U16Be,
        Self::U16Le,
        Self::U32Be,
        Self::U32Le,
        Self::U64Be,
        Self::U64Le,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16Be => "u16be",
            Self::U16Le => "u16le",
            Self::U32Be => "u32be",
            Self::U32Le => "u32le",
            Self::U64Be => "u64be",
            Self::U64Le => "u64le",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|prefix| prefix.name() == name)
    }

    /// Encoded prefix width in bytes.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Be | Self::U16Le => 2,
            Self::U32Be | Self::U32Le => 4,
            Self::U64Be | Self::U64Le => 8,
        }
    }

    const fn big_endian(self) -> bool {
        matches!(self, Self::U8 | Self::U16Be | Self::U32Be | Self::U64Be)
    }

    fn decode(self, bytes: &[u8]) -> u64 {
        let fold = |value: u64, byte: &u8| (value << 8) | u64::from(*byte);
        if self.big_endian() {
            bytes.iter().fold(0, fold)
        } else {
            bytes.iter().rev().fold(0, fold)
        }
    }

    fn encode(self, length: usize) -> Option<Vec<u8>> {
        let length = u64::try_from(length).ok()?;
        let width = self.width();
        if width < 8 && length >> (width * 8) != 0 {
            return None;
        }
        let bytes = length.to_be_bytes()[8 - width..].to_vec();
        Some(if self.big_endian() {
            bytes
        } else {
            bytes.into_iter().rev().collect()
        })
    }
}

/// A compile-validated built-in framing.
#[derive(Clone, Debug, PartialEq)]
pub enum BuiltinFraming {
    /// Frames end with a delimiter that is not part of the body.
    Delimited {
        delimiter: String,
        require_final_delimiter: bool,
    },
    /// Each body follows its unsigned byte length.
    LengthPrefixed(LengthPrefix),
    /// Every body has exactly this many bytes.
    Fixed(usize),
}

/// One installed framing, built in or custom.
pub trait ContentFramer: Send + Sync {
    /// Bound for one complete encoded frame, including prefixes or delimiters.
    fn max_frame_bytes(&self) -> usize;
    /// Media type assigned to every frame, if the framing declares one.
    fn fixed_media_type(&self) -> Option<&str>;
    /// Read one frame object with `bodyBytes` (and optionally `mediaType`), or
    /// null at a clean end of input.
    fn read<'a>(
        &'a self,
        reader: Arc<FrameReader>,
        sensitive: bool,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a>;
    /// Encode one prepared frame object, whose `bodyBytes` are already encoded.
    fn write<'a>(
        &'a self,
        frame: Value,
        span: Span,
        context: &'a IoContext,
    ) -> CapabilityFuture<'a>;
}

pub struct FramerBinding {
    pub name: String,
    pub framer: Arc<dyn ContentFramer>,
}

/// A bounded, sequential cursor over an owned byte source.
pub struct FrameReader {
    state: tokio::sync::Mutex<FrameReaderState>,
    max_frame_bytes: usize,
}

struct FrameReaderState {
    reader: Box<dyn ByteReader>,
    buffered: Vec<u8>,
    eof: bool,
    used: usize,
}

impl FrameReaderState {
    async fn fill(&mut self) -> Result<(), CapabilityError> {
        match self.reader.next_chunk().await? {
            Some(chunk) => self.buffered.extend_from_slice(&chunk),
            None => self.eof = true,
        }
        Ok(())
    }

    fn take(&mut self, count: usize, skip: usize) -> Arc<[u8]> {
        let bytes = Arc::from(&self.buffered[..count]);
        self.buffered.drain(..count + skip);
        self.used += count + skip;
        bytes
    }
}

impl FrameReader {
    #[must_use]
    pub fn new(reader: Box<dyn ByteReader>, max_frame_bytes: usize) -> Self {
        Self {
            state: tokio::sync::Mutex::new(FrameReaderState {
                reader,
                buffered: Vec::new(),
                eof: false,
                used: 0,
            }),
            max_frame_bytes,
        }
    }

    pub async fn begin_frame(&self) {
        self.state.lock().await.used = 0;
    }

    pub async fn consumed_in_frame(&self) -> usize {
        self.state.lock().await.used
    }

    /// Whether the source ended exactly at a frame boundary.
    /// # Errors
    /// Propagates an error from the owned byte reader.
    pub async fn at_clean_eof(&self) -> Result<bool, CapabilityError> {
        let mut state = self.state.lock().await;
        while !state.eof && state.buffered.is_empty() {
            state.fill().await?;
        }
        Ok(state.buffered.is_empty())
    }

    /// Read bytes preceding a delimiter, which is consumed but not returned.
    /// A clean EOF before any new bytes is `None`. EOF inside a frame is an
    /// error unless `allow_unterminated`, which returns the remaining bytes.
    /// # Errors
    /// Fails on truncation, an exceeded bound, or reader failure.
    pub async fn read_until(
        &self,
        delimiter: &[u8],
        max_bytes: usize,
        allow_unterminated: bool,
        span: Span,
    ) -> Result<Option<Arc<[u8]>>, CapabilityError> {
        if delimiter.is_empty() || max_bytes == 0 {
            return Err(CapabilityError::new(
                "readUntil requires a nonempty delimiter and a positive maxBytes",
                span,
            ));
        }
        let mut state = self.state.lock().await;
        // Bytes before the delimiter, and the complete frame, are both bounded.
        let limit = max_bytes.min(
            self.max_frame_bytes
                .saturating_sub(state.used)
                .saturating_sub(delimiter.len()),
        );
        let mut searched: usize = 0;
        loop {
            let start = searched.saturating_sub(delimiter.len() - 1);
            if let Some(offset) = state.buffered[start..]
                .windows(delimiter.len())
                .position(|window| window == delimiter)
            {
                let index = start + offset;
                if index > limit {
                    return Err(CapabilityError::new("frame exceeds maxFrameBytes", span));
                }
                return Ok(Some(state.take(index, delimiter.len())));
            }
            searched = state.buffered.len();
            if searched > limit.saturating_add(delimiter.len() - 1) {
                return Err(CapabilityError::new(
                    "frame delimiter was not found within maxFrameBytes",
                    span,
                ));
            }
            if state.eof {
                if state.buffered.is_empty() {
                    return Ok(None);
                }
                if allow_unterminated && state.buffered.len() <= limit {
                    let count = state.buffered.len();
                    return Ok(Some(state.take(count, 0)));
                }
                return Err(CapabilityError::new(
                    "truncated frame: input ended before the delimiter",
                    span,
                ));
            }
            state.fill().await?;
        }
    }

    /// Read exactly `count` bytes. EOF before the requested count is truncation.
    /// # Errors
    /// Fails on truncation, an exceeded frame bound, or reader failure.
    pub async fn read_exactly(
        &self,
        count: usize,
        span: Span,
    ) -> Result<Arc<[u8]>, CapabilityError> {
        let mut state = self.state.lock().await;
        if count > self.max_frame_bytes.saturating_sub(state.used) {
            return Err(CapabilityError::new("frame exceeds maxFrameBytes", span));
        }
        while state.buffered.len() < count {
            if state.eof {
                return Err(CapabilityError::new(
                    format!(
                        "truncated frame: expected {count} bytes, input ended after {}",
                        state.buffered.len()
                    ),
                    span,
                ));
            }
            state.fill().await?;
        }
        Ok(state.take(count, 0))
    }
}

impl fmt::Debug for FrameReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FrameReader(<opaque>)")
    }
}

impl PartialEq for FrameReader {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

/// A built-in framing with its compile-validated media type and bound.
pub struct BuiltinFramer {
    pub framing: BuiltinFraming,
    pub media_type: Option<String>,
    pub max_frame_bytes: usize,
}

fn with_sensitivity(value: Value, sensitive: bool) -> Value {
    if sensitive { value.sensitive() } else { value }
}

impl BuiltinFramer {
    fn frame(&self, bytes: Arc<[u8]>, sensitive: bool) -> Value {
        let mut fields = crate::Object::new();
        fields.insert("bodyBytes".into(), Value::Bytes(bytes));
        if let Some(media) = &self.media_type {
            fields.insert("mediaType".into(), Value::String(media.clone()));
        }
        with_sensitivity(Value::Object(fields), sensitive)
    }
}

impl ContentFramer for BuiltinFramer {
    fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    fn fixed_media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }

    fn read<'a>(
        &'a self,
        reader: Arc<FrameReader>,
        sensitive: bool,
        span: Span,
        _: &'a IoContext,
    ) -> CapabilityFuture<'a> {
        Box::pin(async move {
            let bytes = match &self.framing {
                BuiltinFraming::Delimited {
                    delimiter,
                    require_final_delimiter,
                } => {
                    reader
                        .read_until(
                            delimiter.as_bytes(),
                            self.max_frame_bytes,
                            !require_final_delimiter,
                            span,
                        )
                        .await?
                }
                BuiltinFraming::LengthPrefixed(prefix) => {
                    if reader.at_clean_eof().await? {
                        None
                    } else {
                        let header = reader.read_exactly(prefix.width(), span).await?;
                        let length = prefix.decode(&header);
                        let available = self.max_frame_bytes.saturating_sub(prefix.width());
                        let length = usize::try_from(length)
                            .ok()
                            .filter(|length| *length <= available)
                            .ok_or_else(|| {
                                CapabilityError::new(
                                    format!(
                                        "frame length {length} exceeds maxFrameBytes ({} bytes including the {}-byte prefix)",
                                        self.max_frame_bytes,
                                        prefix.width()
                                    ),
                                    span,
                                )
                            })?;
                        Some(reader.read_exactly(length, span).await?)
                    }
                }
                BuiltinFraming::Fixed(size) => {
                    if reader.at_clean_eof().await? {
                        None
                    } else {
                        Some(reader.read_exactly(*size, span).await?)
                    }
                }
            };
            Ok(bytes.map_or(Value::Null, |bytes| self.frame(bytes, sensitive)))
        })
    }

    fn write<'a>(&'a self, frame: Value, span: Span, _: &'a IoContext) -> CapabilityFuture<'a> {
        Box::pin(async move {
            let Some(Value::Bytes(body)) = frame
                .as_object()
                .and_then(|fields| fields.get("bodyBytes"))
                .map(Value::revealed)
            else {
                return Err(CapabilityError::new(
                    "framing encode requires bodyBytes",
                    span,
                ));
            };
            let too_large = || CapabilityError::new("encoded frame exceeds maxFrameBytes", span);
            let output = match &self.framing {
                BuiltinFraming::Delimited { delimiter, .. } => {
                    let delimiter = delimiter.as_bytes();
                    if body
                        .windows(delimiter.len())
                        .any(|window| window == delimiter)
                    {
                        return Err(CapabilityError::new(
                            "frame body contains the framing delimiter",
                            span,
                        ));
                    }
                    if body.len().saturating_add(delimiter.len()) > self.max_frame_bytes {
                        return Err(too_large());
                    }
                    [&body[..], delimiter].concat()
                }
                BuiltinFraming::LengthPrefixed(prefix) => {
                    if body.len().saturating_add(prefix.width()) > self.max_frame_bytes {
                        return Err(too_large());
                    }
                    let header = prefix.encode(body.len()).ok_or_else(|| {
                        CapabilityError::new(
                            format!(
                                "frame body of {} bytes does not fit a {} length prefix",
                                body.len(),
                                prefix.name()
                            ),
                            span,
                        )
                    })?;
                    [&header[..], &body[..]].concat()
                }
                BuiltinFraming::Fixed(size) => {
                    if body.len() != *size {
                        return Err(CapabilityError::new(
                            format!(
                                "fixed framing requires exactly {size} body bytes, got {}",
                                body.len()
                            ),
                            span,
                        ));
                    }
                    body.to_vec()
                }
            };
            Ok(with_sensitivity(
                Value::Bytes(Arc::from(output)),
                frame.contains_sensitive(),
            ))
        })
    }
}

/// Create a lazy, single-consumer stream of frames from complete bytes or an
/// owned byte source. Nothing is read until the stream is consumed.
/// # Errors
/// Fails if the input is neither bytes nor a byte source.
pub fn frames(
    input: &Value,
    framer: Arc<dyn ContentFramer>,
    span: Span,
    context: &IoContext,
) -> Result<Value, CapabilityError> {
    let source = match input.revealed() {
        Value::Source(source) => source.clone(),
        Value::Bytes(bytes) => Arc::new(ByteSource::from_bytes(bytes.clone(), span, context)),
        _ => {
            return Err(CapabilityError::new(
                "frames requires complete bytes or an owned byte source",
                span,
            ));
        }
    };
    let factory = FrameFactory {
        source,
        framer,
        span,
        sensitive: input.contains_sensitive(),
    };
    Ok(Value::Stream(Arc::new(ValueSource::new(
        Box::new(factory),
        span,
        context,
    ))))
}

/// Encode one frame object: its body codec runs first, then the framing adds
/// its boundary. The complete result is bounded by `maxFrameBytes`.
/// # Errors
/// Fails on an invalid frame, a codec error, or an exceeded bound.
pub async fn encode(
    input: &Value,
    framer: &dyn ContentFramer,
    span: Span,
    context: &IoContext,
) -> Result<Value, CapabilityError> {
    let frame = prepare_frame(input, framer, span, context).await?;
    let result = framer.write(frame, span, context).await?;
    let sensitive = result.contains_sensitive() || input.contains_sensitive();
    let bytes = match result.revealed() {
        Value::Bytes(bytes) => bytes.clone(),
        Value::Source(source) => tokio::time::timeout(crate::DEFAULT_IO_TIMEOUT, async {
            let mut reader = source.open(context).await?;
            let mut output = Vec::new();
            while let Some(chunk) = reader.next_chunk().await? {
                if chunk.len() > framer.max_frame_bytes().saturating_sub(output.len()) {
                    return Err(CapabilityError::new(
                        "encoded frame exceeds maxFrameBytes",
                        span,
                    ));
                }
                output.extend_from_slice(&chunk);
            }
            Ok::<Arc<[u8]>, CapabilityError>(Arc::from(output))
        })
        .await
        .map_err(|_| CapabilityError::new("framing write output timed out", span))??,
        _ => {
            return Err(CapabilityError::new(
                "framing write must return bytes or a byte source",
                span,
            ));
        }
    };
    if bytes.len() > framer.max_frame_bytes() {
        return Err(CapabilityError::new(
            "encoded frame exceeds maxFrameBytes",
            span,
        ));
    }
    Ok(with_sensitivity(Value::Bytes(bytes), sensitive))
}

async fn prepare_frame(
    input: &Value,
    framer: &dyn ContentFramer,
    span: Span,
    context: &IoContext,
) -> Result<Value, CapabilityError> {
    let mut fields = input
        .as_object()
        .cloned()
        .ok_or_else(|| CapabilityError::new("framing encode requires a frame object", span))?;
    let requested = match fields.get("mediaType").map(Value::revealed) {
        Some(Value::String(value)) => Some(MediaType::parse(value, span)?),
        None | Some(Value::Null) => None,
        Some(_) => {
            return Err(CapabilityError::new(
                "frame mediaType must be a string",
                span,
            ));
        }
    };
    let fixed = framer
        .fixed_media_type()
        .map(|value| MediaType::parse(value, span))
        .transpose()?;
    if let (Some(requested), Some(fixed)) = (&requested, &fixed)
        && !requested.equivalent(fixed)
    {
        return Err(CapabilityError::new(
            "frame mediaType conflicts with the framing's mediaType",
            span,
        ));
    }
    let media = requested.or(fixed);
    let bytes = match (fields.get("body"), fields.get("bodyBytes")) {
        (Some(_), Some(_)) => {
            return Err(CapabilityError::new(
                "framing encode accepts body or bodyBytes, not both",
                span,
            ));
        }
        (Some(body), None) => match &media {
            Some(media) => {
                crate::content::encode_media(body, media, framer.max_frame_bytes(), span, context)
                    .await?
            }
            None if matches!(body.revealed(), Value::Bytes(_)) => body.clone(),
            None => {
                return Err(CapabilityError::new(
                    "framing encode requires mediaType to encode a non-bytes body",
                    span,
                ));
            }
        },
        (None, Some(bytes)) if matches!(bytes.revealed(), Value::Bytes(_)) => bytes.clone(),
        (None, Some(_)) => {
            return Err(CapabilityError::new(
                "frame bodyBytes must be complete bytes",
                span,
            ));
        }
        (None, None) => {
            return Err(CapabilityError::new(
                "framing encode requires body or bodyBytes",
                span,
            ));
        }
    };
    fields.remove("body");
    fields.insert("bodyBytes".to_owned(), bytes);
    match media {
        Some(media) => fields.insert("mediaType".to_owned(), Value::String(media.normalized())),
        None => fields.remove("mediaType"),
    };
    Ok(with_sensitivity(
        Value::Object(fields),
        input.contains_sensitive(),
    ))
}

struct FrameFactory {
    source: Arc<ByteSource>,
    framer: Arc<dyn ContentFramer>,
    span: Span,
    sensitive: bool,
}

impl ValueSourceFactory for FrameFactory {
    fn open(&self, context: &IoContext) -> ValueReaderFuture<'_> {
        let context = context.clone();
        Box::pin(async move {
            let reader = self.source.open(&context).await?;
            Ok(Box::new(Frames {
                reader: Arc::new(FrameReader::new(reader, self.framer.max_frame_bytes())),
                framer: self.framer.clone(),
                span: self.span,
                context,
                sensitive: self.sensitive,
                ended: false,
            }) as Box<dyn ValueReader>)
        })
    }
}

struct Frames {
    reader: Arc<FrameReader>,
    framer: Arc<dyn ContentFramer>,
    span: Span,
    context: IoContext,
    sensitive: bool,
    ended: bool,
}

impl Frames {
    // Waiting for bytes is bounded by the source itself (for example an HTTP
    // request timeout), so a quiet long-lived stream is not a framing error.
    async fn next_frame(&mut self) -> Result<Option<Value>, CapabilityError> {
        self.reader.begin_frame().await;
        let value = self
            .framer
            .read(
                self.reader.clone(),
                self.sensitive,
                self.span,
                &self.context,
            )
            .await?;
        if matches!(value.revealed(), Value::Null) {
            if !self.reader.at_clean_eof().await? {
                return Err(CapabilityError::new(
                    "framing read returned null before the byte source ended",
                    self.span,
                ));
            }
            return Ok(None);
        }
        if self.reader.consumed_in_frame().await == 0 {
            return Err(CapabilityError::new(
                "framing read produced a frame without consuming bytes",
                self.span,
            ));
        }
        let sensitive = self.sensitive || value.contains_sensitive();
        let mut fields = value.as_object().cloned().ok_or_else(|| {
            CapabilityError::new(
                "framing read must return a frame object or null at the end of input",
                self.span,
            )
        })?;
        let bytes = fields
            .get("bodyBytes")
            .ok_or_else(|| CapabilityError::new("frame requires bodyBytes", self.span))?;
        let Value::Bytes(raw) = bytes.revealed() else {
            return Err(CapabilityError::new(
                "frame bodyBytes must be complete bytes",
                self.span,
            ));
        };
        if raw.len() > self.framer.max_frame_bytes() {
            return Err(CapabilityError::new(
                "frame body exceeds maxFrameBytes",
                self.span,
            ));
        }
        let bytes = with_sensitivity(bytes.clone(), sensitive);
        let media = match fields.get("mediaType").map(Value::revealed) {
            Some(Value::String(value)) => Some(MediaType::parse(value, self.span)?),
            None | Some(Value::Null) => None,
            _ => {
                return Err(CapabilityError::new(
                    "frame mediaType must be a string",
                    self.span,
                ));
            }
        };
        let decoded = crate::content::decode_media(
            &bytes,
            media.as_ref(),
            self.framer.max_frame_bytes(),
            self.span,
            &self.context,
        )
        .await?;
        fields.insert("bodyBytes".to_owned(), bytes);
        fields.insert("body".to_owned(), decoded);
        Ok(Some(with_sensitivity(Value::Object(fields), sensitive)))
    }
}

impl ValueReader for Frames {
    fn next_value(&mut self) -> ValueFuture<'_> {
        Box::pin(async move {
            if self.ended {
                return Ok(None);
            }
            // An error leaves the cursor inside a frame, so the stream is fused.
            let result = self.next_frame().await;
            if !matches!(result, Ok(Some(_))) {
                self.ended = true;
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChunkFuture;
    use std::collections::VecDeque;

    struct SplitReader(VecDeque<Arc<[u8]>>);

    impl ByteReader for SplitReader {
        fn next_chunk(&mut self) -> ChunkFuture<'_> {
            Box::pin(async move { Ok(self.0.pop_front()) })
        }
    }

    fn reader(chunks: &[&[u8]], max: usize) -> FrameReader {
        FrameReader::new(
            Box::new(SplitReader(
                chunks.iter().map(|chunk| Arc::from(*chunk)).collect(),
            )),
            max,
        )
    }

    fn builtin(framing: BuiltinFraming, max: usize) -> Arc<dyn ContentFramer> {
        Arc::new(BuiltinFramer {
            framing,
            media_type: None,
            max_frame_bytes: max,
        })
    }

    fn delimited(require_final_delimiter: bool) -> BuiltinFraming {
        BuiltinFraming::Delimited {
            delimiter: "\r\n".into(),
            require_final_delimiter,
        }
    }

    async fn collect(
        framer: Arc<dyn ContentFramer>,
        chunks: &[&[u8]],
    ) -> Result<Vec<Vec<u8>>, CapabilityError> {
        let context = IoContext::new(std::env::temp_dir());
        let span = Span::default();
        let payload: Vec<u8> = chunks.concat();
        let stream = frames(&Value::Bytes(Arc::from(payload)), framer, span, &context)?;
        let Value::Stream(stream) = stream else {
            unreachable!()
        };
        let mut reader = stream.open(&context).await?;
        let mut output = Vec::new();
        while let Some(frame) = reader.next_value().await? {
            let Some(Value::Bytes(bytes)) = frame.as_object().unwrap().get("bodyBytes") else {
                unreachable!()
            };
            output.push(bytes.to_vec());
        }
        assert!(reader.next_value().await?.is_none(), "stream stays ended");
        Ok(output)
    }

    /// Yields one chunk after a long quiet period, then records its drop.
    struct QuietReader {
        chunks: VecDeque<Arc<[u8]>>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ByteReader for QuietReader {
        fn next_chunk(&mut self) -> ChunkFuture<'_> {
            Box::pin(async move {
                tokio::time::sleep(crate::DEFAULT_IO_TIMEOUT * 4).await;
                Ok(self.chunks.pop_front())
            })
        }
    }

    impl Drop for QuietReader {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct QuietSource(std::sync::Mutex<Option<QuietReader>>);

    impl crate::SourceFactory for QuietSource {
        fn open(&self, _: &IoContext) -> crate::ReaderFuture<'_> {
            let reader = self.0.lock().unwrap().take();
            Box::pin(
                async move { Ok(Box::new(reader.expect("opened once")) as Box<dyn ByteReader>) },
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_sources_are_not_framing_timeouts_and_drop_releases_them() {
        let context = IoContext::new(std::env::temp_dir());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = QuietReader {
            chunks: VecDeque::from([Arc::from(&b"a\r\nb\r\n"[..])]),
            dropped: dropped.clone(),
        };
        let source = Value::Source(Arc::new(ByteSource::new(
            Box::new(QuietSource(std::sync::Mutex::new(Some(reader)))),
            Span::default(),
            &context,
        )));
        let Value::Stream(stream) = frames(
            &source,
            builtin(delimited(true), 16),
            Span::default(),
            &context,
        )
        .unwrap() else {
            unreachable!()
        };
        let mut frames = stream.open(&context).await.unwrap();
        let first = frames.next_value().await.unwrap().unwrap();
        let Some(Value::Bytes(body)) = first.as_object().unwrap().get("bodyBytes") else {
            unreachable!()
        };
        assert_eq!(&**body, b"a");
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(frames);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stream_is_fused_after_an_error() {
        let context = IoContext::new(std::env::temp_dir());
        let input = Value::Bytes(Arc::from(&b"abcdefgh\r\nok\r\n"[..]));
        let Value::Stream(stream) = frames(
            &input,
            builtin(delimited(true), 4),
            Span::default(),
            &context,
        )
        .unwrap() else {
            unreachable!()
        };
        let mut frames = stream.open(&context).await.unwrap();
        assert!(frames.next_value().await.is_err());
        assert!(frames.next_value().await.unwrap().is_none());
    }

    #[test]
    fn length_prefixes_round_trip_in_both_byte_orders() {
        for prefix in LengthPrefix::ALL {
            assert_eq!(LengthPrefix::parse(prefix.name()), Some(*prefix));
            let encoded = prefix.encode(0x0102).filter(|_| prefix.width() > 1);
            if let Some(encoded) = encoded {
                assert_eq!(encoded.len(), prefix.width());
                assert_eq!(prefix.decode(&encoded), 0x0102);
            }
        }
        assert_eq!(LengthPrefix::U16Be.encode(0x0102).unwrap(), [1, 2]);
        assert_eq!(LengthPrefix::U16Le.encode(0x0102).unwrap(), [2, 1]);
        assert!(LengthPrefix::U8.encode(256).is_none());
        assert_eq!(LengthPrefix::U8.encode(255).unwrap(), [255]);
    }

    #[tokio::test]
    async fn frame_reader_handles_split_delimiters_and_exact_payloads() {
        let reader = reader(&[b"{\"length\":4}", b"\r", b"\nDA", b"TA"], 64);
        let span = Span::default();
        reader.begin_frame().await;
        assert_eq!(
            &*reader
                .read_until(b"\r\n", 32, false, span)
                .await
                .unwrap()
                .unwrap(),
            b"{\"length\":4}"
        );
        assert_eq!(&*reader.read_exactly(4, span).await.unwrap(), b"DATA");
        assert_eq!(reader.consumed_in_frame().await, 18);
        reader.begin_frame().await;
        let end = reader.read_until(b"\r\n", 32, false, span).await.unwrap();
        assert!(end.is_none());
        assert!(reader.at_clean_eof().await.unwrap());
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversized_and_truncated_frames() {
        let span = Span::default();
        let oversized = reader(&[b"too-long\n"], 8);
        assert!(oversized.read_until(b"\n", 4, false, span).await.is_err());
        let truncated = reader(&[b"head\nxy"], 32);
        let head = truncated.read_until(b"\n", 16, false, span).await.unwrap();
        assert_eq!(&*head.unwrap(), b"head");
        let error = truncated.read_exactly(3, span).await.unwrap_err();
        assert!(error.message.contains("expected 3 bytes"), "{error:?}");
        let partial = reader(&[b"partial"], 32);
        assert!(partial.read_until(b"\n", 16, false, span).await.is_err());
        let tolerant = reader(&[b"par", b"tial"], 32);
        let last = tolerant.read_until(b"\n", 16, true, span).await.unwrap();
        assert_eq!(&*last.unwrap(), b"partial");
    }

    #[tokio::test]
    async fn builtin_framings_ignore_chunk_boundaries() {
        let lines = collect(
            builtin(delimited(true), 16),
            &[b"a\r", b"\nbc\r\n", b"\r\n"],
        );
        assert_eq!(
            lines.await.unwrap(),
            [b"a".to_vec(), b"bc".to_vec(), vec![]]
        );
        let prefixed = collect(
            builtin(BuiltinFraming::LengthPrefixed(LengthPrefix::U16Be), 16),
            &[b"\0", b"\x03ab", b"c\0\0", b"\0\x01z"],
        );
        assert_eq!(
            prefixed.await.unwrap(),
            [b"abc".to_vec(), vec![], b"z".to_vec()]
        );
        let fixed = collect(builtin(BuiltinFraming::Fixed(2), 4), &[b"a", b"bcd"]);
        assert_eq!(fixed.await.unwrap(), [b"ab".to_vec(), b"cd".to_vec()]);
    }

    #[tokio::test]
    async fn builtin_framings_reject_truncation_and_bounds() {
        let unterminated = collect(builtin(delimited(true), 16), &[b"a\r\nb"]).await;
        assert!(unterminated.unwrap_err().message.contains("truncated"));
        let tolerant = collect(builtin(delimited(false), 16), &[b"a\r\nb"]).await;
        assert_eq!(tolerant.unwrap(), [b"a".to_vec(), b"b".to_vec()]);
        let oversized = collect(builtin(delimited(true), 4), &[b"abcd\r\n"]).await;
        assert!(oversized.unwrap_err().message.contains("maxFrameBytes"));
        let prefix = LengthPrefix::U32Le;
        let long = collect(
            builtin(BuiltinFraming::LengthPrefixed(prefix), 8),
            &[b"\x05\0\0\0abcde"],
        )
        .await;
        assert!(long.unwrap_err().message.contains("frame length 5"));
        let short_header = collect(
            builtin(BuiltinFraming::LengthPrefixed(prefix), 8),
            &[b"\x01\0"],
        )
        .await;
        assert!(
            short_header
                .unwrap_err()
                .message
                .contains("expected 4 bytes")
        );
        let short_body = collect(builtin(BuiltinFraming::Fixed(3), 8), &[b"abcd"]).await;
        assert!(short_body.unwrap_err().message.contains("truncated"));
    }

    #[tokio::test]
    async fn builtin_encoders_add_and_validate_boundaries() {
        let context = IoContext::new(std::env::temp_dir());
        let span = Span::default();
        let frame = |bytes: &[u8]| {
            Value::Object(crate::Object::from([(
                "bodyBytes".to_owned(),
                Value::Bytes(Arc::from(bytes)),
            )]))
        };
        let encode_with = |framing, max, bytes: &'static [u8]| {
            let framer = builtin(framing, max);
            let context = context.clone();
            async move { encode(&frame(bytes), framer.as_ref(), span, &context).await }
        };
        let Value::Bytes(line) = encode_with(delimited(true), 8, b"ok").await.unwrap() else {
            unreachable!()
        };
        assert_eq!(&*line, b"ok\r\n");
        assert!(encode_with(delimited(true), 8, b"a\r\nb").await.is_err());
        let prefix = BuiltinFraming::LengthPrefixed(LengthPrefix::U16Le);
        let Value::Bytes(prefixed) = encode_with(prefix.clone(), 8, b"ab").await.unwrap() else {
            unreachable!()
        };
        assert_eq!(&*prefixed, b"\x02\0ab");
        assert!(encode_with(prefix, 3, b"ab").await.is_err());
        let small = BuiltinFraming::LengthPrefixed(LengthPrefix::U8);
        let body: &'static [u8] = &[0; 256];
        let error = encode_with(small, 1024, body).await.unwrap_err();
        assert!(error.message.contains("u8 length prefix"), "{error:?}");
        assert!(
            encode_with(BuiltinFraming::Fixed(3), 8, b"ab")
                .await
                .is_err()
        );
    }
}
