//! AWS `vnd.amazon.eventstream` binary framing for Bedrock Converse — ported from
//! `packages/llm/src/protocols/bedrock-event-stream.ts`.
//!
//! Bedrock streams the Converse response as a sequence of binary frames, each laid out as
//! `[total_len: u32][headers_len: u32][prelude_crc: u32][headers][payload][message_crc: u32]`
//! (all integers big-endian). Every frame carries `:message-type` and `:event-type` string headers;
//! for `event` frames we take the JSON payload and rewrap it under its `:event-type`
//! (e.g. `{"messageStart": { … }}`) so a [`Protocol`](crate::Protocol) can decode it as a plain
//! tagged record — mirroring the TS codec's `{ [eventType]: parsed }` reshaping. The AWS framing pads
//! short payloads with a `p` field, which the event structs simply ignore as an unknown field.
//!
//! CRCs are *not* validated here: frame boundaries come from the `total_len` prelude, which is all the
//! decoder needs for the trusted recorded cassettes. CRC validation belongs with the live wire
//! transport (where bytes arrive unverified), alongside SigV4 request signing — a later increment.

use crate::LlmError;

/// `total_len(4) + headers_len(4) + prelude_crc(4)`.
const PRELUDE_LEN: usize = 12;
/// Trailing per-message CRC32.
const MESSAGE_CRC_LEN: usize = 4;
/// AWS event-stream header value type tag for a UTF-8 string.
const HEADER_TYPE_STRING: u8 = 7;

fn decode_err(msg: impl Into<String>) -> LlmError {
    LlmError::Decode(msg.into())
}

/// Read a big-endian `u32` at `at`, erroring if the buffer is too short.
fn be_u32(bytes: &[u8], at: usize) -> Result<u32, LlmError> {
    let s = bytes
        .get(at..at + 4)
        .ok_or_else(|| decode_err("truncated event-stream integer"))?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Find a string-typed header value by name, skipping over other value types by their encoded width.
/// Only the `:message-type` / `:event-type` string headers matter; other types are stepped over so a
/// frame that carries them still parses.
fn header_value<'a>(headers: &'a [u8], wanted: &str) -> Result<Option<&'a str>, LlmError> {
    let mut off = 0;
    while off < headers.len() {
        let name_len = *headers
            .get(off)
            .ok_or_else(|| decode_err("event-stream header name length"))?
            as usize;
        off += 1;
        let name = headers
            .get(off..off + name_len)
            .ok_or_else(|| decode_err("event-stream header name"))?;
        off += name_len;
        let value_type = *headers
            .get(off)
            .ok_or_else(|| decode_err("event-stream header value type"))?;
        off += 1;
        // Encoded width of each AWS event-stream header value type.
        let width = match value_type {
            0 | 1 => 0, // bool true / false
            2 => 1,     // byte
            3 => 2,     // int16
            4 => 4,     // int32
            5 | 8 => 8, // int64 / timestamp
            9 => 16,    // uuid
            6 | HEADER_TYPE_STRING => {
                // byte-array / string: u16 length prefix
                let len = u16::from_be_bytes([
                    *headers
                        .get(off)
                        .ok_or_else(|| decode_err("event-stream header value length"))?,
                    *headers
                        .get(off + 1)
                        .ok_or_else(|| decode_err("event-stream header value length"))?,
                ]) as usize;
                off += 2;
                len
            }
            other => {
                return Err(decode_err(format!(
                    "unsupported event-stream header type {other}"
                )))
            }
        };
        let value = headers
            .get(off..off + width)
            .ok_or_else(|| decode_err("event-stream header value"))?;
        off += width;
        if value_type == HEADER_TYPE_STRING && name == wanted.as_bytes() {
            return std::str::from_utf8(value)
                .map(Some)
                .map_err(|_| decode_err("event-stream header value not utf-8"));
        }
    }
    Ok(None)
}

/// Decode a complete `vnd.amazon.eventstream` body into the JSON payloads of its `event` frames, each
/// rewrapped as `{"<event-type>": <payload>}`. Non-`event` frames (e.g. exceptions delivered with a
/// different `:message-type`) and empty payloads are dropped.
pub fn frames(bytes: &[u8]) -> Result<Vec<String>, LlmError> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < bytes.len() {
        let total = be_u32(bytes, off)? as usize;
        let headers_len = be_u32(bytes, off + 4)? as usize;
        if total < PRELUDE_LEN + MESSAGE_CRC_LEN {
            return Err(decode_err("event-stream frame shorter than its framing"));
        }
        let end = off
            .checked_add(total)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| decode_err("event-stream frame length past end of buffer"))?;
        let headers_start = off + PRELUDE_LEN;
        let payload_start = headers_start
            .checked_add(headers_len)
            .filter(|s| *s <= end - MESSAGE_CRC_LEN)
            .ok_or_else(|| decode_err("event-stream headers length past end of frame"))?;
        let headers = &bytes[headers_start..payload_start];
        let payload = &bytes[payload_start..end - MESSAGE_CRC_LEN];
        off = end;

        if header_value(headers, ":message-type")? != Some("event") {
            continue;
        }
        let Some(event_type) = header_value(headers, ":event-type")? else {
            continue;
        };
        if payload.is_empty() {
            continue;
        }
        let payload = std::str::from_utf8(payload)
            .map_err(|_| decode_err("event-stream payload not utf-8"))?;
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| decode_err(format!("event-stream payload not json: {e}")))?;
        // Rewrap as a single-key object tagged by the event type.
        let mut wrapper = serde_json::Map::with_capacity(1);
        wrapper.insert(event_type.to_string(), value);
        out.push(serde_json::Value::Object(wrapper).to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut h = vec![name.len() as u8];
        h.extend_from_slice(name.as_bytes());
        h.push(HEADER_TYPE_STRING);
        h.extend_from_slice(&(value.len() as u16).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
        h
    }

    /// Build one synthetic frame (CRCs left zero — the decoder does not check them).
    fn frame(message_type: &str, event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = string_header(":message-type", message_type);
        headers.extend(string_header(":event-type", event_type));
        let total = PRELUDE_LEN + headers.len() + payload.len() + MESSAGE_CRC_LEN;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // prelude crc (unchecked)
        out.extend(headers);
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // message crc (unchecked)
        out
    }

    #[test]
    fn rewraps_event_frames_and_skips_non_events() {
        let mut bytes = frame("event", "messageStart", r#"{"role":"assistant","p":"xx"}"#);
        bytes.extend(frame(
            "event",
            "contentBlockDelta",
            r#"{"delta":{"text":"Hi"}}"#,
        ));
        // A non-`event` message-type frame is dropped.
        bytes.extend(frame(
            "exception",
            "internalServerException",
            r#"{"message":"boom"}"#,
        ));

        let frames = frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(
            first,
            serde_json::json!({ "messageStart": { "role": "assistant", "p": "xx" } })
        );
        let second: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(
            second,
            serde_json::json!({ "contentBlockDelta": { "delta": { "text": "Hi" } } })
        );
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let bytes = frame("event", "messageStart", r#"{"role":"assistant"}"#);
        assert!(frames(&bytes[..bytes.len() - 3]).is_err());
    }
}
