//! WebSocket helpers shared by the log endpoints.

use axum::extract::ws::{Message, WebSocket};
use axum::http::{HeaderMap, header};
use tokio::sync::mpsc;

use crate::error::{AppError, AppResult};

/// Cookie-authenticated WebSocket upgrades are not covered by CORS, so a hostile page
/// could open one with the user's cookie (cross-site WebSocket hijacking). Browsers
/// always send `Origin`; it must match the `Host` the request was sent to. Non-browser
/// clients (no `Origin`) authenticate with bearer tokens and are unaffected.
pub fn check_origin(headers: &HeaderMap) -> AppResult<()> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://").map(|(_, rest)| rest))
        .map(|rest| rest.trim_end_matches('/'));
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    match (origin_host, host) {
        (Some(o), Some(h)) if o.eq_ignore_ascii_case(h) => Ok(()),
        _ => Err(AppError::Forbidden),
    }
}

/// Send each line as a text frame until the source ends or the client disconnects.
pub async fn forward_lines(mut socket: WebSocket, mut lines: mpsc::Receiver<String>) {
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(line) => {
                    if socket.send(Message::Text(line.into())).await.is_err() {
                        return;
                    }
                }
                None => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    #[test]
    fn origin_must_match_host() {
        assert!(check_origin(&headers(None, "127.0.0.1:4367")).is_ok());
        assert!(check_origin(&headers(Some("http://127.0.0.1:4367"), "127.0.0.1:4367")).is_ok());
        assert!(
            check_origin(&headers(
                Some("https://Panel.example.com"),
                "panel.example.com"
            ))
            .is_ok()
        );
        assert!(check_origin(&headers(Some("https://evil.example"), "panel.example.com")).is_err());
        assert!(check_origin(&headers(Some("null"), "panel.example.com")).is_err());
    }
}
