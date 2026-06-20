//! Minimal LSP base protocol framing: JSON-RPC messages prefixed with a
//! `Content-Length` header (`Content-Length: N\r\n\r\n<json>`). Both sides of an
//! LSP connection speak this; we hand-roll it (no vscode-jsonrpc) over the
//! language server's child stdio.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Serialize and write one framed message, flushing so the server sees it.
pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Read one framed message. `Ok(None)` is a clean EOF (the server closed its
/// stdout); an unparseable body is an `InvalidData` error.
pub async fn read_message<R: AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF before any complete message
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().ok();
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP message had no Content-Length header",
        )
    })?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    let value = serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trips_a_message_through_framing() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"jsonrpc":"2.0","method":"ping","id":1}))
            .await
            .unwrap();
        // The wire bytes carry the Content-Length envelope.
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));

        let mut reader = BufReader::new(&buf[..]);
        let msg = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(msg["method"], "ping");
        assert_eq!(msg["id"], 1);
    }

    #[tokio::test]
    async fn reads_two_back_to_back_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"id":1})).await.unwrap();
        write_message(&mut buf, &json!({"id":2})).await.unwrap();
        let mut reader = BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 1);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 2);
        // Clean EOF after the last message.
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_input_is_clean_eof() {
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }
}
