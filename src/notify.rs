//! Outbound notification webhooks (Discord / Slack / Telegram / generic JSON) for app state changes.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::auth::crypto::{Cipher, aad};

pub const KINDS: [&str; 4] = ["discord", "slack", "telegram", "generic"];
pub const STATUS_CHANGED: &str = "app.status_changed";
pub const BACKUP_FAILED: &str = "backup.failed";
/// Sent from the panel to check a target's setup.
pub const TEST: &str = "test";

/// Telegram counts the text after entity parsing; this leaves room for the other lines.
const TELEGRAM_DETAIL_CHARS: usize = 3500;

#[derive(Debug, Clone)]
pub struct Event {
    /// `app.status_changed`, `backup.failed` or `test`.
    pub kind: &'static str,
    pub app_id: String,
    pub app_name: String,
    pub from: String,
    pub to: String,
    pub detail: String,
}

impl Event {
    pub fn message(&self) -> String {
        match self.kind {
            BACKUP_FAILED => format!(
                "forgenerd: backup of app `{}` failed: {}",
                self.app_name, self.detail
            ),
            TEST => "forgenerd: test notification; this target is set up correctly".into(),
            _ => format!(
                "forgenerd: app `{}` changed from {} to {}: {}",
                self.app_name, self.from, self.to, self.detail
            ),
        }
    }

    /// Telegram message, sent with `parse_mode: HTML`:
    ///
    /// ```text
    /// 🔴 qwe is failed            (app name bold)
    /// Container no longer exists; recreating it from the active deployment
    /// was running · forgenerd     (italic)
    /// ```
    pub fn telegram_html(&self) -> String {
        let app = html_escape(&self.app_name);
        let detail: String = self
            .detail
            .trim()
            .chars()
            .take(TELEGRAM_DETAIL_CHARS)
            .collect();
        let detail = html_escape(&capitalize(&detail));
        match self.kind {
            TEST => {
                "✅ <b>forgenerd</b> test notification\nThis target is set up correctly.".into()
            }
            BACKUP_FAILED => format!(
                "🔴 Backup of <b>{app}</b> failed\n{detail}\n<i>{} · forgenerd</i>",
                html_escape(&self.from)
            ),
            _ => format!(
                "{} <b>{app}</b> is {}\n{detail}\n<i>was {} · forgenerd</i>",
                status_emoji(&self.to),
                html_escape(&self.to),
                html_escape(&self.from)
            ),
        }
    }
}

fn status_emoji(status: &str) -> &'static str {
    match status {
        "running" => "🟢",
        "unhealthy" => "🟠",
        "failed" => "🔴",
        "stopped" => "⚪",
        _ => "🔵",
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Telegram's HTML mode only requires `<`, `>` and `&` to be escaped.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default()
    })
}

pub fn payload(kind: &str, event: &Event) -> Value {
    match kind {
        "discord" => json!({ "content": event.message() }),
        "telegram" => json!({
            "text": event.telegram_html(),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        }),
        "slack" => json!({ "text": event.message() }),
        _ => json!({
            "event": event.kind,
            "app_id": event.app_id,
            "app": event.app_name,
            "from": event.from,
            "to": event.to,
            "detail": event.detail,
            "message": event.message(),
        }),
    }
}

/// POST one payload; returns an error message on failure.
pub async fn send(url: &str, body: &Value) -> Result<(), String> {
    match client().post(url).json(body).send().await {
        Ok(res) if res.status().is_success() => Ok(()),
        Ok(res) => Err(format!("HTTP {}", res.status())),
        Err(err) => Err(err.without_url().to_string()),
    }
}

/// Fan an event out to every enabled target in the background. Failures are logged only.
pub fn dispatch(db: SqlitePool, cipher: Cipher, event: Event) {
    tokio::spawn(async move {
        let targets: Vec<(String, String, String)> = match sqlx::query_as(
            "SELECT id, kind, url_enc FROM notification_targets WHERE enabled = 1",
        )
        .fetch_all(&db)
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "loading notification targets");
                return;
            }
        };
        for (id, kind, url_enc) in targets {
            let url = match cipher.decrypt(&url_enc, &aad("notification_targets", &id, "url_enc")) {
                Ok(url) => url,
                Err(err) => {
                    tracing::warn!(target_id = %id, error = %err, "cannot decrypt notification URL");
                    continue;
                }
            };
            if let Err(err) = send(&url, &payload(&kind, &event)).await {
                tracing::warn!(target_id = %id, %kind, error = %err, "notification failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shapes() {
        let e = Event {
            kind: STATUS_CHANGED,
            app_id: "1".into(),
            app_name: "web".into(),
            from: "running".into(),
            to: "unhealthy".into(),
            detail: "restarting".into(),
        };
        assert!(
            payload("discord", &e)["content"]
                .as_str()
                .unwrap()
                .contains("`web`")
        );
        assert!(payload("slack", &e)["text"].is_string());
        let telegram = payload("telegram", &e);
        assert_eq!(telegram["parse_mode"], "HTML");
        assert_eq!(
            telegram["text"],
            "🟠 <b>web</b> is unhealthy\nRestarting\n<i>was running · forgenerd</i>"
        );
        let hostile = Event {
            app_name: "a<b>".into(),
            detail: "x & <script>".into(),
            ..e.clone()
        };
        let text = hostile.telegram_html();
        assert!(text.contains("<b>a&lt;b&gt;</b>"));
        assert!(text.contains("X &amp; &lt;script&gt;"));
        let long = Event {
            detail: "界".repeat(5000),
            ..e.clone()
        };
        assert!(long.telegram_html().chars().count() < 4096);
        assert_eq!(payload("generic", &e)["to"], "unhealthy");
    }
}
