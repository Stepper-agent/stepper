//! A hermetic fake language server for `stepper-lsp` integration tests: it
//! completes the `initialize` handshake and pushes one ERROR diagnostic whenever a
//! document is opened. Built by cargo as `CARGO_BIN_EXE_fake_lsp_server`.

use serde_json::{json, Value};
use stepper_lsp::protocol::{read_message, write_message};
use tokio::io::{stdin, stdout, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut reader = BufReader::new(stdin());
    let mut out = stdout();
    while let Ok(Some(msg)) = read_message(&mut reader).await {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => {
                let _ = write_message(
                    &mut out,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{"textDocumentSync":1}}}),
                )
                .await;
            }
            "textDocument/didOpen" | "textDocument/didChange" => {
                let uri = msg
                    .pointer("/params/textDocument/uri")
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = write_message(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                                "severity": 1,
                                "message": "fake error"
                            }]
                        }
                    }),
                )
                .await;
            }
            "shutdown" => {
                let _ = write_message(&mut out, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
            }
            "exit" => break,
            _ => {
                // Answer any other server-bound request so the client never hangs.
                if id.as_ref().is_some_and(|v| !v.is_null()) {
                    let _ =
                        write_message(&mut out, &json!({"jsonrpc":"2.0","id":id,"result":null})).await;
                }
            }
        }
    }
}
