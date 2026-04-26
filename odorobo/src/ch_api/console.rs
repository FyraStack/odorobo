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
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use tokio::io::unix::AsyncFd;
use tokio::time::{Duration, sleep};
use tracing::{debug, trace, warn};

use super::error::ApiError;
use crate::state::{ConsoleStream, VMInstance};

pub async fn console_stream(
    vmid: Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let vmid = vmid.0;
    let vm = VMInstance::get(&vmid).ok_or_else(|| ApiError::VmNotFound { vmid: vmid.clone() })?;
    let console = vm
        .open_console()
        .await
        .map_err(|e| ApiError::ConsoleFailed { msg: e.to_string() })?;

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
    Connected { vm_id: &'a str },
    Error { message: &'a str },
}

async fn proxy_console_socket(vm_id: String, mut socket: WebSocket, console: ConsoleStream) {
    if let Err(err) = send_console_event(
        &mut socket,
        ConsoleServerMessage::Connected { vm_id: &vm_id },
    )
    .await
    {
        warn!(vm_id, ?err, "Failed to send connected event");
        return;
    }

    let console_fd = match AsyncFd::new(console) {
        Ok(fd) => fd,
        Err(err) => {
            warn!(vm_id, ?err, "Failed to wrap console in AsyncFd");
            return;
        }
    };

    match proxy_console(&mut socket, console_fd).await {
        Ok(()) => debug!(vm_id, "Console websocket disconnected"),
        Err(err) => warn!(vm_id, ?err, "Console websocket proxy failed"),
    }

    let _ = socket.close().await;
}

async fn proxy_console(socket: &mut WebSocket, mut console: AsyncFd<ConsoleStream>) -> Result<()> {
    let mut buf = [0_u8; 8192];

    loop {
        // trace!("select waiting on ws.recv() and console.read()");
        tokio::select! {
            message = socket.recv() => {
                // trace!("ws.recv() returned: {:?}", message.as_ref().map(|m| match m {
                //     Ok(Message::Binary(d)) => format!("Binary({}b)", d.len()),
                //     Ok(Message::Text(t)) => format!("Text({}b)", t.len()),
                //     Ok(Message::Close(_)) => "Close".into(),
                //     Ok(Message::Ping(_)) => "Ping".into(),
                //     Ok(Message::Pong(_)) => "Pong".into(),
                //     Err(e) => format!("Error: {}", e),
                // }));
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        // trace!(bytes = data.len(), "ws -> pty (binary)");
                        let _ = console.writable().await?;
                        let data_copy = data.to_vec();
                        let write_result = tokio::task::block_in_place(|| {
                            console.get_mut().write_all(&data_copy)?;
                            console.get_mut().flush()?;
                            Ok::<(), io::Error>(())
                        });
                        write_result?;
                        // trace!("write and flush done");
                    }
                    Some(Ok(Message::Text(text))) => {
                        // trace!(len = text.len(), "ws -> pty (text/control)");
                        handle_console_control(socket, &mut console, text.to_string()).await?
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // trace!("ws close or none");
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        // trace!("ws ping");
                        socket.send(Message::Pong(payload)).await?
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // trace!("ws pong");
                    }
                    Some(Err(err)) => {
                        trace!(?err, "ws error");
                        return Err(err.into());
                    }
                }
            }
            read_result = async {
                let _ = console.readable().await?;
                tokio::task::block_in_place(|| {
                    console.get_mut().read(&mut buf)
                })
            } => {
                match read_result {
                    Ok(n) => {
                        if n == 0 {
                            // trace!("pty read returned 0 bytes");
                            break;
                        }
                        // trace!(bytes = n, "pty -> ws");
                        socket.send(Message::Binary(buf[..n].to_vec().into())).await?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // trace!("pty read would block, retrying");
                        // Continue select loop to try again
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    Ok(())
}

async fn handle_console_control(
    socket: &mut WebSocket,
    console: &mut AsyncFd<ConsoleStream>,
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
                send_console_event(socket, ConsoleServerMessage::Error { message: &message })
                    .await?;
            }
        }
        ConsoleControlMessage::ResetSession => {
            if let Err(err) = reset_console_session(console).await {
                let message = format!("Failed to reset console session: {err}");
                send_console_event(socket, ConsoleServerMessage::Error { message: &message })
                    .await?;
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
    console: &AsyncFd<ConsoleStream>,
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

async fn reset_console_session(console: &mut AsyncFd<ConsoleStream>) -> Result<()> {
    const STEP_DELAY: Duration = Duration::from_millis(75);

    macro_rules! write_to_console {
        ($data:expr) => {{
            let _ = console.writable().await?;
            let data = $data.to_vec();
            tokio::task::block_in_place(|| {
                console.get_mut().write_all(&data)?;
                console.get_mut().flush()?;
                Ok::<(), io::Error>(())
            })?;
        }};
    }

    write_to_console!(&[0x03]);
    sleep(STEP_DELAY).await;

    write_to_console!(b"\r");
    sleep(STEP_DELAY).await;

    write_to_console!(&[0x04]);

    Ok(())
}
