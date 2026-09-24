use anyhow::Result;
use reqwest::Client;

#[derive(Clone)]
pub struct TelegramClient {
    token: String,
    chat_id: String,
    http: Client,
}

impl TelegramClient {
    pub fn new(token: String, chat_id: String) -> Result<Self> {
        Ok(TelegramClient {
            token,
            chat_id,
            http: Client::new(),
        })
    }

    /// plain text, truncated to the Telegram message limit
    pub async fn send(&self, text: &str) -> Result<()> {
        let text: String = if text.chars().count() > 3900 {
            text.chars().take(3900).collect::<String>() + "\n…"
        } else {
            text.to_string()
        };
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("telegram sendMessage failed: {body}");
        }
        Ok(())
    }
}
