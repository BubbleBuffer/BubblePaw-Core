//! Streaming plumbing shared by the HTTP adapters. Server-sent events are read
//! with an idle timeout instead of a whole-request timeout, so a long answer
//! can finish. Text fragments reach the listener in batches; they are transient
//! and never stored.
use super::{classify, http_dump};
use anyhow::{Result, anyhow};
use bubblepaw_core::{CancellationToken, ProviderErrorKind, TextCallback};
use std::time::Duration;
use tokio::time::Instant;

/// Longest silence allowed before response headers and between body chunks.
/// Replaces the old 300 s whole-request timeout, which cut long answers short.
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Pending text reaches the listener at least this often while it streams.
pub(super) const FLUSH_INTERVAL: Duration = Duration::from_millis(75);
/// Pending text reaches the listener once it holds this many characters.
pub(super) const FLUSH_CHARS: usize = 512;
/// Memory-safety limit on one streamed body. Not an output-size limit: output
/// length never rejects a reply, and no other size cap applies to it.
const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Memory-safety limit on one unparsed SSE frame. Not an output-size limit.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// HTTP client for streamed calls. Only the connect time is bounded here; idle
/// time is bounded per read by `IDLE_TIMEOUT`.
pub(super) fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Why a stream stopped before its terminal event.
pub(super) enum StreamError {
    /// The caller cancelled the request.
    Cancelled,
    /// The connection dropped, went silent or ended early. Transient.
    Transport(anyhow::Error),
    /// The provider reported an error, or the bytes break the protocol. Already classified.
    Failed(anyhow::Error),
}

impl StreamError {
    pub(super) fn into_error(self) -> anyhow::Error {
        match self {
            Self::Cancelled => super::cancelled(),
            Self::Transport(error) => classify(ProviderErrorKind::Transient, error),
            Self::Failed(error) => error,
        }
    }
}

/// A classified failure for a stream handler.
pub(super) fn failed(kind: ProviderErrorKind, message: &str) -> StreamError {
    StreamError::Failed(classify(kind, anyhow!(message.to_owned())))
}

/// The error for a stream that stopped before its terminal event. It stays transient
/// only while no text has reached the listener, because a retry would repeat that text.
pub(super) fn stopped(error: StreamError, published: bool) -> anyhow::Error {
    match error {
        StreamError::Transport(error) if published => classify(ProviderErrorKind::Fatal, error),
        error => error.into_error(),
    }
}

/// Whether the reply is server-sent events. Anything else is a complete JSON reply.
pub(super) fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

/// The whole body of a non-streamed reply, bounded by the idle timeout.
pub(super) async fn read_body(
    mut response: reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    if http_dump::enabled(&response) {
        let read = async {
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                http_dump::append(&response, &chunk);
                body.extend_from_slice(&chunk);
            }
            Ok::<_, reqwest::Error>(body)
        };
        return tokio::select! {
            result = tokio::time::timeout(IDLE_TIMEOUT, read) => match result {
                Ok(Ok(body)) => Ok(body),
                Ok(Err(_)) => Err(classify(ProviderErrorKind::Transient, anyhow!("provider response transport failed"))),
                Err(_) => Err(classify(ProviderErrorKind::Transient, anyhow!("provider response was idle too long"))),
            },
            _ = cancellation.cancelled() => Err(super::cancelled()),
        };
    }
    tokio::select! {
        result = tokio::time::timeout(IDLE_TIMEOUT, response.bytes()) => match result {
            Ok(Ok(bytes)) => Ok(bytes.to_vec()),
            Ok(Err(_)) => Err(classify(ProviderErrorKind::Transient, anyhow!("provider response transport failed"))),
            Err(_) => Err(classify(ProviderErrorKind::Transient, anyhow!("provider response was idle too long"))),
        },
        _ = cancellation.cancelled() => Err(super::cancelled()),
    }
}

/// Reads server-sent events from a response body, one event's data at a time.
pub(super) struct SseReader {
    response: reqwest::Response,
    buffer: Vec<u8>,
    received: usize,
}

impl SseReader {
    pub(super) fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            received: 0,
        }
    }

    /// The data of the next event, or `None` when the body ends. A partial
    /// frame at the end is dropped; callers require their own terminal event.
    pub(super) async fn next_data(
        &mut self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<Option<String>, StreamError> {
        loop {
            if let Some(frame) = take_frame(&mut self.buffer) {
                if let Some(data) = frame_data(&frame)? {
                    return Ok(Some(data));
                }
                continue;
            }
            let chunk = tokio::select! {
                chunk = tokio::time::timeout(IDLE_TIMEOUT, self.response.chunk()) => chunk,
                _ = cancellation.cancelled() => return Err(StreamError::Cancelled),
            };
            let chunk = match chunk {
                Err(_) => {
                    return Err(StreamError::Transport(anyhow!(
                        "provider stream was idle too long"
                    )));
                }
                Ok(Err(_)) => {
                    return Err(StreamError::Transport(anyhow!(
                        "provider stream disconnected"
                    )));
                }
                Ok(Ok(None)) => return Ok(None),
                Ok(Ok(Some(chunk))) => chunk,
            };
            http_dump::append(&self.response, &chunk);
            self.received = self.received.saturating_add(chunk.len());
            if self.received > MAX_STREAM_BYTES {
                return Err(failed(
                    ProviderErrorKind::Fatal,
                    "provider stream exceeds 32 MiB",
                ));
            }
            self.buffer.extend_from_slice(&chunk);
            if self.buffer.len() > MAX_FRAME_BYTES {
                return Err(failed(
                    ProviderErrorKind::Fatal,
                    "provider SSE frame exceeds 8 MiB",
                ));
            }
        }
    }
}

/// Removes one complete frame (blank-line separated, LF or CRLF) from the buffer.
fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let unix = buffer.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4));
    let (position, len) = match (unix, crlf) {
        (Some(a), Some(b)) => {
            if a.0 < b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        _ => return None,
    };
    Some(buffer.drain(..position + len).collect())
}

/// The joined `data:` lines of one frame; `None` for frames without data.
fn frame_data(frame: &[u8]) -> std::result::Result<Option<String>, StreamError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| failed(ProviderErrorKind::Fatal, "provider SSE is not UTF-8"))?;
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((!data.is_empty()).then_some(data))
}

/// Collects text fragments and hands them to the listener in batches.
pub(super) struct TextBatch {
    listener: TextCallback,
    pending: String,
    chars: usize,
    since: Option<Instant>,
}

impl TextBatch {
    pub(super) fn new(listener: TextCallback) -> Self {
        Self {
            listener,
            pending: String::new(),
            chars: 0,
            since: None,
        }
    }

    /// Queue a fragment; delivers at once when the batch is full or old enough.
    pub(super) fn push(&mut self, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        self.pending.push_str(fragment);
        self.chars += fragment.chars().count();
        if self.chars >= FLUSH_CHARS || since.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    /// When pending text is due for delivery, if any is pending.
    pub(super) fn due(&self) -> Option<Instant> {
        self.since.map(|since| since + FLUSH_INTERVAL)
    }

    /// Deliver pending text now: at block ends, at the terminal event, on errors.
    pub(super) fn flush(&mut self) {
        self.since = None;
        self.chars = 0;
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            (self.listener)(&text);
        }
    }
}

/// Feed events to `handle` until it reports the terminal event (`Ok(true)`).
/// Pending text is delivered when its interval is due. Whatever ends the call,
/// except cancellation, delivers the pending text first. `ended` is the error
/// for a body that stops before the terminal event.
pub(super) async fn pump<F>(
    reader: &mut SseReader,
    batch: &mut TextBatch,
    cancellation: &CancellationToken,
    ended: &str,
    mut handle: F,
) -> std::result::Result<(), StreamError>
where
    F: FnMut(&str, &mut TextBatch) -> std::result::Result<bool, StreamError>,
{
    let result = drive(reader, batch, cancellation, ended, &mut handle).await;
    if !matches!(result, Err(StreamError::Cancelled)) {
        batch.flush();
    }
    result
}

async fn drive<F>(
    reader: &mut SseReader,
    batch: &mut TextBatch,
    cancellation: &CancellationToken,
    ended: &str,
    handle: &mut F,
) -> std::result::Result<(), StreamError>
where
    F: FnMut(&str, &mut TextBatch) -> std::result::Result<bool, StreamError>,
{
    loop {
        let next = match batch.due() {
            Some(due) => tokio::select! {
                next = reader.next_data(cancellation) => next,
                _ = tokio::time::sleep_until(due) => {
                    batch.flush();
                    continue;
                }
            },
            None => reader.next_data(cancellation).await,
        };
        match next? {
            Some(data) => {
                if handle(&data, batch)? {
                    return Ok(());
                }
            }
            None => return Err(StreamError::Transport(anyhow!(ended.to_owned()))),
        }
    }
}
