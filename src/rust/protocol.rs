use crate::engine::{AppEngine, BackendError, EngineEvent, ErrorPayload, EventSink};
use crate::model::{DownloadMode, DownloadRequest, VideoQuality};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, path::PathBuf, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;

const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    protocol_version: u32,
    kind: String,
    request_id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    protocol_version: u32,
    kind: &'static str,
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorPayload>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventMessage {
    protocol_version: u32,
    kind: &'static str,
    operation_id: String,
    event: String,
    data: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutputFolderParams {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelParams {
    operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadParams {
    url: String,
    mode: String,
    video_quality: String,
    output_folder: String,
}

pub async fn run(engine: Arc<AppEngine>) -> io::Result<()> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = BufWriter::new(tokio::io::stdout());
        while let Some(line) = receiver.recv().await {
            stdout.write_all(line.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        Ok::<(), io::Error>(())
    });

    let event_sender = sender.clone();
    let events: EventSink = Arc::new(move |event: EngineEvent| {
        let message = EventMessage {
            protocol_version: PROTOCOL_VERSION,
            kind: "event",
            operation_id: event.operation_id,
            event: event.event,
            data: event.data,
        };
        if let Ok(line) = serde_json::to_string(&message) {
            let _ = event_sender.send(line);
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                send_protocol_error(&sender, "", "malformedRequest", error.to_string());
                continue;
            }
        };
        if request.kind != "request" {
            send_protocol_error(
                &sender,
                &request.request_id,
                "invalidKind",
                "expected a request message".into(),
            );
            continue;
        }
        if request.protocol_version != PROTOCOL_VERSION {
            send_protocol_error(
                &sender,
                &request.request_id,
                "protocolMismatch",
                format!(
                    "backend protocol {PROTOCOL_VERSION} does not support protocol {}",
                    request.protocol_version
                ),
            );
            continue;
        }
        if request.method == "shutdown" {
            engine.cancel_active();
            send_result(
                &sender,
                &request.request_id,
                json!({ "shuttingDown": true }),
            );
            break;
        }
        let request_engine = Arc::clone(&engine);
        let request_sender = sender.clone();
        let request_events = Arc::clone(&events);
        tokio::spawn(async move {
            handle_request(request_engine, request_sender, request_events, request).await;
        });
    }

    engine.cancel_active();
    drop(events);
    drop(sender);
    match tokio::time::timeout(std::time::Duration::from_secs(2), writer).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(io::Error::other(error)),
        Err(_) => Ok(()),
    }
}

async fn handle_request(
    engine: Arc<AppEngine>,
    sender: mpsc::UnboundedSender<String>,
    events: EventSink,
    request: Request,
) {
    let result = match request.method.as_str() {
        "initialize" => engine.initialize().and_then(to_value),
        "setOutputFolder" => {
            parse_params::<OutputFolderParams>(request.params).and_then(|params| {
                engine.set_output_folder(PathBuf::from(params.path))?;
                Ok(json!({ "saved": true }))
            })
        }
        "checkTools" => {
            let lease = match engine.reserve_operation() {
                Ok(lease) => lease,
                Err(error) => {
                    send_error(&sender, &request.request_id, &error);
                    return;
                }
            };
            engine.check_tools(&lease).await.and_then(to_value)
        }
        "installTools" => {
            let lease = match engine.reserve_operation() {
                Ok(lease) => lease,
                Err(error) => {
                    send_error(&sender, &request.request_id, &error);
                    return;
                }
            };
            let operation_id = lease.id.clone();
            send_result(
                &sender,
                &request.request_id,
                json!({ "operationId": operation_id }),
            );
            tokio::spawn(Arc::clone(&engine).install_tools(lease, events));
            return;
        }
        "startDownload" => {
            let parameters = parse_download(request.params)
                .and_then(|parameters| engine.validate_download(parameters));
            match parameters.and_then(|parameters| {
                let lease = engine.reserve_operation()?;
                let operation_id = lease.id.clone();
                send_result(
                    &sender,
                    &request.request_id,
                    json!({ "operationId": operation_id }),
                );
                tokio::spawn(Arc::clone(&engine).download(lease, parameters, events));
                Ok(())
            }) {
                Ok(()) => {}
                Err(error) => send_error(&sender, &request.request_id, &error),
            }
            return;
        }
        "cancel" => parse_params::<CancelParams>(request.params)
            .map(|params| json!({ "cancelled": engine.cancel(&params.operation_id) })),
        _ => Err(BackendError::InvalidRequest(format!(
            "unknown method {:?}",
            request.method
        ))),
    };

    match result {
        Ok(value) => send_result(&sender, &request.request_id, value),
        Err(error) => send_error(&sender, &request.request_id, &error),
    }
}

fn parse_download(value: Value) -> Result<DownloadRequest, BackendError> {
    let params = parse_params::<DownloadParams>(value)?;
    let mode = match params.mode.as_str() {
        "video" => DownloadMode::Video,
        "audioOnly" => DownloadMode::AudioOnly,
        value => {
            return Err(BackendError::InvalidRequest(format!(
                "unknown download mode {value:?}"
            )));
        }
    };
    let video_quality = match params.video_quality.as_str() {
        "p1080" => VideoQuality::P1080,
        "p1440" => VideoQuality::P1440,
        "best" => VideoQuality::Best,
        value => {
            return Err(BackendError::InvalidRequest(format!(
                "unknown video quality {value:?}"
            )));
        }
    };
    Ok(DownloadRequest {
        url: params.url,
        mode,
        video_quality,
        output_directory: PathBuf::from(params.output_folder),
    })
}

fn parse_params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, BackendError> {
    serde_json::from_value(value).map_err(|error| BackendError::InvalidRequest(error.to_string()))
}

fn to_value<T: Serialize>(value: T) -> Result<Value, BackendError> {
    serde_json::to_value(value).map_err(|error| BackendError::InvalidRequest(error.to_string()))
}

fn send_result(sender: &mpsc::UnboundedSender<String>, request_id: &str, result: Value) {
    send_response(
        sender,
        Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: request_id.into(),
            ok: true,
            result: Some(result),
            error: None,
        },
    );
}

fn send_error(sender: &mpsc::UnboundedSender<String>, request_id: &str, error: &BackendError) {
    send_response(
        sender,
        Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: request_id.into(),
            ok: false,
            result: None,
            error: Some(error.payload()),
        },
    );
}

fn send_protocol_error(
    sender: &mpsc::UnboundedSender<String>,
    request_id: &str,
    code: &str,
    details: String,
) {
    send_response(
        sender,
        Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: request_id.into(),
            ok: false,
            result: None,
            error: Some(ErrorPayload {
                code: code.into(),
                message: "The backend received an invalid protocol message.".into(),
                details: Some(details),
            }),
        },
    );
}

fn send_response(sender: &mpsc::UnboundedSender<String>, response: Response) {
    if let Ok(line) = serde_json::to_string(&response) {
        let _ = sender.send(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versioned_request_envelope() {
        let request: Request = serde_json::from_str(
            r#"{"protocolVersion":1,"kind":"request","requestId":"abc","method":"initialize","params":{}}"#,
        )
        .unwrap();
        assert_eq!(request.protocol_version, PROTOCOL_VERSION);
        assert_eq!(request.request_id, "abc");
    }

    #[test]
    fn parses_platform_neutral_download_values() {
        let request = parse_download(json!({
            "url": "https://example.com/video",
            "mode": "video",
            "videoQuality": "p1440",
            "outputFolder": "/media/output"
        }))
        .unwrap();
        assert_eq!(request.video_quality, VideoQuality::P1440);
        assert_eq!(request.output_directory, PathBuf::from("/media/output"));
    }

    #[test]
    fn response_uses_camel_case_wire_names() {
        let response = Response {
            protocol_version: PROTOCOL_VERSION,
            kind: "response",
            request_id: "one".into(),
            ok: true,
            result: Some(json!({})),
            error: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"protocolVersion\":1"));
        assert!(json.contains("\"requestId\":\"one\""));
    }
}
