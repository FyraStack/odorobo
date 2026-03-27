use axum::{
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use stable_eyre::{Result, eyre::eyre};
use std::os::fd::AsRawFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, sleep};
use tracing::{debug, warn};

use super::error::ApiError;
use crate::state::{ConsoleStream, VMInstance};

pub async fn console_stream(
    vmid: Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let vmid = vmid.0;
    let vm = VMInstance::get(&vmid).ok_or_else(|| ApiError::VmNotFound(vmid.clone()))?;
    let console = vm.open_console().await.map_err(|_| ApiError::ConsoleFailed)?;

    Ok(ws.on_upgrade(move |socket| proxy_console_socket(vmid, socket, console)))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ConsoleControlMessage {
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        x_pixels: u16,
        #[serde(default)]
        y_pixels: u16,
    },
    ResetSession,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ConsoleServerMessage<'a> {
    Error { message: &'a str },
}

async fn proxy_console_socket(vm_id: String, mut socket: WebSocket, mut console: ConsoleStream) {
    match proxy_console(&mut socket, &mut console).await {
        Ok(()) => debug!(vm_id, "Console websocket disconnected"),
        Err(err) => warn!(vm_id, ?err, "Console websocket proxy failed"),
    }

    let _ = socket.close().await;
}

async fn proxy_console(socket: &mut WebSocket, console: &mut ConsoleStream) -> Result<()> {
    let mut buf = [0_u8; 8192];

    loop {
        tokio::select! {
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Binary(data))) => console.write_all(&data).await?,
                    Some(Ok(Message::Text(text))) => handle_console_control(socket, console, text.to_string()).await?,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => socket.send(Message::Pong(payload)).await?,
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(err)) => return Err(err.into()),
                }
            }
            read = console.read(&mut buf) => {
                let read = read?;
                if read == 0 {
                    break;
                }

                socket.send(Message::Binary(buf[..read].to_vec().into())).await?;
            }
        }
    }

    Ok(())
}

async fn handle_console_control(
    socket: &mut WebSocket,
    console: &mut ConsoleStream,
    raw_message: String,
) -> Result<()> {
    let message: ConsoleControlMessage = match serde_json::from_str(&raw_message) {
        Ok(message) => message,
        Err(_) => {
            send_console_event(
                socket,
                ConsoleServerMessage::Error {
                    message: "Text frames are reserved for JSON control messages such as {\"type\":\"resize\",\"cols\":120,\"rows\":40} or {\"type\":\"reset_session\"}",
                },
            )
            .await?;
            return Ok(());
        }
    };

    match message {
        ConsoleControlMessage::Resize {
            cols,
            rows,
            x_pixels,
            y_pixels,
        } => {
            if let Err(err) = resize_console(console, cols, rows, x_pixels, y_pixels) {
                let message = format!("Failed to resize console: {err}");
                send_console_event(socket, ConsoleServerMessage::Error { message: &message }).await?;
            }
        }
        ConsoleControlMessage::ResetSession => {
            if let Err(err) = reset_console_session(console).await {
                let message = format!("Failed to reset console session: {err}");
                send_console_event(socket, ConsoleServerMessage::Error { message: &message }).await?;
            }
        }
    }

    Ok(())
}

async fn send_console_event(socket: &mut WebSocket, event: ConsoleServerMessage<'_>) -> Result<()> {
    let payload = serde_json::to_string(&event)?;
    socket.send(Message::Text(payload.into())).await?;
    Ok(())
}

fn resize_console(
    console: &ConsoleStream,
    cols: u16,
    rows: u16,
    x_pixels: u16,
    y_pixels: u16,
) -> Result<()> {
    if cols == 0 || rows == 0 {
        return Err(eyre!("Console dimensions must be greater than zero"));
    }

    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: x_pixels,
        ws_ypixel: y_pixels,
    };

    let result = unsafe { libc::ioctl(console.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(())
}

async fn reset_console_session(console: &mut ConsoleStream) -> Result<()> {
    const STEP_DELAY: Duration = Duration::from_millis(75);

    console.write_all(&[0x03]).await?;
    console.flush().await?;
    sleep(STEP_DELAY).await;

    console.write_all(b"\r").await?;
    console.flush().await?;
    sleep(STEP_DELAY).await;

    console.write_all(&[0x04]).await?;
    console.flush().await?;

    Ok(())
}
