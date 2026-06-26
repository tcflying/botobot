//! Debug Adapter Protocol wire framing.

use serde_json::Value;

const HEADER_END: &[u8] = b"\r\n\r\n";
const CONTENT_LENGTH: &str = "content-length";
const MAX_DAP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Encode one DAP JSON message using `Content-Length` framing.
pub fn encode_dap_message(message: &Value) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(message)?;
    anyhow::ensure!(
        body.len() <= MAX_DAP_MESSAGE_BYTES,
        "DAP message exceeds {} bytes",
        MAX_DAP_MESSAGE_BYTES
    );
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend(body);
    Ok(out)
}

/// Incremental DAP message decoder.
#[derive(Debug, Default)]
pub struct DapMessageDecoder {
    buffer: Vec<u8>,
}

impl DapMessageDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Value>> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(header_end) = find_header_end(&self.buffer) else {
                break;
            };
            let header = std::str::from_utf8(&self.buffer[..header_end])?;
            let content_len = parse_content_length(header)?;
            anyhow::ensure!(
                content_len <= MAX_DAP_MESSAGE_BYTES,
                "DAP content length exceeds {} bytes",
                MAX_DAP_MESSAGE_BYTES
            );
            let message_start = header_end + HEADER_END.len();
            let message_end = message_start + content_len;
            if self.buffer.len() < message_end {
                break;
            }
            let message = serde_json::from_slice(&self.buffer[message_start..message_end])?;
            self.buffer.drain(..message_end);
            out.push(message);
        }
        Ok(out)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(HEADER_END.len())
        .position(|window| window == HEADER_END)
}

fn parse_content_length(header: &str) -> anyhow::Result<usize> {
    for line in header.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH) {
            return Ok(value.trim().parse()?);
        }
    }
    anyhow::bail!("DAP message is missing Content-Length header")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_and_decodes_one_message() {
        let message = json!({ "seq": 1, "type": "request", "command": "initialize" });
        let bytes = encode_dap_message(&message).unwrap();
        let mut decoder = DapMessageDecoder::new();
        let decoded = decoder.push(&bytes).unwrap();
        assert_eq!(decoded, vec![message]);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn decoder_waits_for_partial_body() {
        let message = json!({ "seq": 2, "type": "event", "event": "initialized" });
        let bytes = encode_dap_message(&message).unwrap();
        let split = bytes.len() - 3;
        let mut decoder = DapMessageDecoder::new();
        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        assert!(decoder.buffered_len() > 0);
        let decoded = decoder.push(&bytes[split..]).unwrap();
        assert_eq!(decoded, vec![message]);
    }

    #[test]
    fn decoder_returns_multiple_messages_from_one_chunk() {
        let left = json!({ "seq": 1, "type": "event", "event": "output" });
        let right = json!({ "seq": 2, "type": "response", "request_seq": 1, "success": true });
        let mut bytes = encode_dap_message(&left).unwrap();
        bytes.extend(encode_dap_message(&right).unwrap());
        let mut decoder = DapMessageDecoder::new();
        assert_eq!(decoder.push(&bytes).unwrap(), vec![left, right]);
    }

    #[test]
    fn decoder_rejects_missing_content_length() {
        let mut decoder = DapMessageDecoder::new();
        let err = decoder
            .push(b"X-Test: 1\r\n\r\n{}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Content-Length"));
    }
}
