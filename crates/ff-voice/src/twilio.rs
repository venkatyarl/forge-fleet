//! Twilio Voice helpers.
//!
//! - TwiML response generation
//! - webhook payload parsing
//! - minimal outbound call client

use std::time::Duration;

use crate::{Result, VoiceError};

/// Twilio API configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TwilioConfig {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub base_url: String,
    pub timeout_ms: u64,
}

impl TwilioConfig {
    pub fn new(
        account_sid: impl Into<String>,
        auth_token: impl Into<String>,
        from_number: impl Into<String>,
    ) -> Self {
        Self {
            account_sid: account_sid.into(),
            auth_token: auth_token.into(),
            from_number: from_number.into(),
            base_url: "https://api.twilio.com".to_string(),
            timeout_ms: 30_000,
        }
    }
}

/// Response from Twilio call creation endpoint.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TwilioCallResponse {
    pub sid: Option<String>,
    pub status: Option<String>,
    pub to: Option<String>,
    pub from: Option<String>,
}

/// Twilio REST client.
#[derive(Debug, Clone)]
pub struct TwilioClient {
    config: TwilioConfig,
    http: reqwest::Client,
}

impl TwilioClient {
    pub fn new(config: TwilioConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(VoiceError::Http)?;

        Ok(Self { config, http })
    }

    pub fn config(&self) -> &TwilioConfig {
        &self.config
    }

    pub async fn create_call(
        &self,
        to: &str,
        twiml_or_webhook_url: &str,
        http_method: Option<&str>,
    ) -> Result<TwilioCallResponse> {
        let endpoint = format!(
            "{}/2010-04-01/Accounts/{}/Calls.json",
            self.config.base_url.trim_end_matches('/'),
            self.config.account_sid
        );

        let method = http_method.unwrap_or("POST").to_ascii_uppercase();
        let form = [
            ("To", to.to_string()),
            ("From", self.config.from_number.clone()),
            ("Url", twiml_or_webhook_url.to_string()),
            ("Method", method),
        ];

        let resp = self
            .http
            .post(endpoint)
            .basic_auth(&self.config.account_sid, Some(&self.config.auth_token))
            .form(&form)
            .send()
            .await
            .map_err(VoiceError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VoiceError::Twilio(format!(
                "twilio create_call failed {}: {}",
                status, body
            )));
        }

        resp.json::<TwilioCallResponse>()
            .await
            .map_err(VoiceError::Http)
    }
}

/// Twilio inbound webhook payload for voice requests.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TwilioWebhookPayload {
    #[serde(rename = "CallSid")]
    pub call_sid: Option<String>,
    #[serde(rename = "AccountSid")]
    pub account_sid: Option<String>,
    #[serde(rename = "From")]
    pub from: Option<String>,
    #[serde(rename = "To")]
    pub to: Option<String>,
    #[serde(rename = "CallStatus")]
    pub call_status: Option<String>,
    #[serde(rename = "Direction")]
    pub direction: Option<String>,
    #[serde(rename = "Digits")]
    pub digits: Option<String>,
    #[serde(rename = "SpeechResult")]
    pub speech_result: Option<String>,
    #[serde(rename = "Confidence")]
    pub speech_confidence: Option<String>,
    #[serde(rename = "RecordingUrl")]
    pub recording_url: Option<String>,
    #[serde(rename = "RecordingSid")]
    pub recording_sid: Option<String>,
}

impl TwilioWebhookPayload {
    /// Parse `application/x-www-form-urlencoded` Twilio webhook body.
    pub fn parse_form(body: &str) -> Result<Self> {
        serde_urlencoded::from_str(body)
            .map_err(|e| VoiceError::Twilio(format!("invalid Twilio webhook body: {e}")))
    }
}

/// Lightweight TwiML builder.
#[derive(Debug, Clone, Default)]
pub struct TwimlBuilder {
    verbs: Vec<String>,
}

impl TwimlBuilder {
    pub fn new() -> Self {
        Self { verbs: Vec::new() }
    }

    pub fn say(mut self, text: impl AsRef<str>) -> Self {
        self.verbs
            .push(format!("<Say>{}</Say>", xml_escape(text.as_ref())));
        self
    }

    pub fn play(mut self, url: impl AsRef<str>) -> Self {
        self.verbs
            .push(format!("<Play>{}</Play>", xml_escape(url.as_ref())));
        self
    }

    pub fn pause(mut self, length_seconds: u32) -> Self {
        self.verbs
            .push(format!("<Pause length=\"{}\" />", length_seconds));
        self
    }

    pub fn redirect(mut self, url: impl AsRef<str>, method: Option<&str>) -> Self {
        let method_attr = method
            .map(|m| format!(" method=\"{}\"", xml_escape(m)))
            .unwrap_or_default();
        self.verbs.push(format!(
            "<Redirect{}>{}</Redirect>",
            method_attr,
            xml_escape(url.as_ref())
        ));
        self
    }

    pub fn gather_speech(mut self, action: impl AsRef<str>, prompt: impl AsRef<str>) -> Self {
        self.verbs.push(format!(
            "<Gather input=\"speech\" action=\"{}\" method=\"POST\"><Say>{}</Say></Gather>",
            xml_escape(action.as_ref()),
            xml_escape(prompt.as_ref())
        ));
        self
    }

    pub fn hangup(mut self) -> Self {
        self.verbs.push("<Hangup />".to_string());
        self
    }

    pub fn build(self) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response>");
        for v in self.verbs {
            xml.push_str(&v);
        }
        xml.push_str("</Response>");
        xml
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_parses() {
        let body = "CallSid=CA123&From=%2B1555&To=%2B1666&SpeechResult=hello+world";
        let payload = TwilioWebhookPayload::parse_form(body).unwrap();
        assert_eq!(payload.call_sid.as_deref(), Some("CA123"));
        assert_eq!(payload.speech_result.as_deref(), Some("hello world"));
    }

    #[test]
    fn twiml_builds() {
        let xml = TwimlBuilder::new().say("hello").hangup().build();
        assert!(xml.contains("<Say>hello</Say>"));
        assert!(xml.contains("<Hangup />"));
    }
}
