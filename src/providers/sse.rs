//! Minimal Server-Sent Events decoder over a reqwest byte stream.

use anyhow::{Result, bail};
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::Response;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Stream of parsed SSE events from an HTTP response body.
pub fn sse_events(response: Response) -> BoxStream<'static, Result<SseEvent>> {
    let bytes = response.bytes_stream();
    Box::pin(async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut event_type: Option<String> = None;
        let mut data_lines: Vec<String> = Vec::new();

        let flush = |event_type: &mut Option<String>, data_lines: &mut Vec<String>| -> Option<SseEvent> {
            if data_lines.is_empty() {
                return None;
            }
            let data = data_lines.join("\n");
            let ev = SseEvent { event: event_type.take(), data };
            data_lines.clear();
            Some(ev)
        };

        #[allow(unused_mut)]
        let mut chunks = bytes;
        while let Some(chunk) = chunks.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(anyhow::anyhow!("SSE read error: {e}"));
                    return;
                }
            };
            buf.extend_from_slice(&chunk);
            // Process complete lines.
            let mut start = 0usize;
            while let Some(pos) = find_line(&buf[start..]) {
                let line_end = start + pos;
                let mut line = String::from_utf8_lossy(&buf[start..line_end]).to_string();
                // Strip trailing \r
                if let Some(stripped) = line.strip_suffix('\r') {
                    line = stripped.to_string();
                }
                if line.is_empty() {
                    if let Some(ev) = flush(&mut event_type, &mut data_lines) {
                        yield Ok(ev);
                    }
                } else if let Some((field, value)) = line.split_once(':') {
                    let (field, value) = (field.trim(), value.trim_start());
                    match field {
                        "event" => event_type = Some(value.to_string()),
                        "data" => data_lines.push(value.to_string()),
                        _ => {}
                    }
                }
                start = line_end + 1;
            }
            buf.drain(..start);
            if buf.len() > 16 * 1024 * 1024 {
                yield Err(anyhow::anyhow!("SSE buffer overflow"));
                return;
            }
        }
        // Flush trailing event without a trailing blank line.
        if let Some(ev) = flush(&mut event_type, &mut data_lines) {
            yield Ok(ev);
        }
    })
}

fn find_line(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == b'\n')
}

fn _unused() -> Result<()> {
    bail!("sentinel")
}
