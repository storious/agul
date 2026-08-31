use serde_json::{Value, json};

use super::transport::{AppServer, CodexError};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CodexLogin {
    Browser {
        login_id: String,
        auth_url: String,
    },
    DeviceCode {
        login_id: String,
        verification_url: String,
        user_code: String,
    },
}

impl CodexLogin {
    pub(crate) fn login_id(&self) -> &str {
        match self {
            Self::Browser { login_id, .. } | Self::DeviceCode { login_id, .. } => login_id,
        }
    }
}

impl AppServer {
    pub(crate) fn account(&mut self) -> Result<Value, CodexError> {
        self.call("account/read", json!({"refreshToken": false}))
    }

    pub(crate) fn rate_limits(&mut self) -> Result<Value, CodexError> {
        self.call("account/rateLimits/read", json!({}))
    }

    pub(crate) fn usage(&mut self) -> Result<Value, CodexError> {
        self.call("account/usage/read", json!({}))
    }

    pub(crate) fn login(&mut self, device_code: bool) -> Result<CodexLogin, CodexError> {
        let params = login_params(device_code);
        let result = self.call("account/login/start", params)?;
        let login_id = required_string(&result, "loginId")?;
        if device_code {
            Ok(CodexLogin::DeviceCode {
                login_id,
                verification_url: required_string(&result, "verificationUrl")?,
                user_code: required_string(&result, "userCode")?,
            })
        } else {
            Ok(CodexLogin::Browser {
                login_id,
                auth_url: required_string(&result, "authUrl")?,
            })
        }
    }

    pub(crate) fn wait_for_login(&mut self, login_id: &str) -> Result<(), CodexError> {
        loop {
            let message = self.next_message()?;
            if message.get("method").and_then(Value::as_str) != Some("account/login/completed") {
                continue;
            }
            let params = message.get("params").unwrap_or(&Value::Null);
            if params.get("loginId").and_then(Value::as_str) != Some(login_id) {
                continue;
            }
            if params.get("success").and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            let error = params
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("login did not complete");
            return Err(CodexError::new(error));
        }
    }

    pub(crate) fn logout(&mut self) -> Result<(), CodexError> {
        self.call("account/logout", json!({})).map(|_| ())
    }
}

fn login_params(device_code: bool) -> Value {
    if device_code {
        json!({"type": "chatgptDeviceCode"})
    } else {
        json!({
            "type": "chatgpt",
            "useHostedLoginSuccessPage": true,
            "appBrand": "chatgpt"
        })
    }
}

fn required_string(value: &Value, field: &str) -> Result<String, CodexError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CodexError::new(format!("Codex response has no {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_shapes_require_the_documented_fields() {
        let browser = json!({"loginId": "one", "authUrl": "https://chatgpt.com/login"});
        let device = json!({
            "loginId": "two",
            "verificationUrl": "https://auth.openai.com/codex/device",
            "userCode": "ABCD-1234"
        });

        assert_eq!(required_string(&browser, "loginId").unwrap(), "one");
        assert_eq!(required_string(&device, "userCode").unwrap(), "ABCD-1234");
        assert!(required_string(&browser, "userCode").is_err());
        assert_eq!(login_params(true), json!({"type": "chatgptDeviceCode"}));
        assert_eq!(
            login_params(false),
            json!({
                "type": "chatgpt",
                "useHostedLoginSuccessPage": true,
                "appBrand": "chatgpt"
            })
        );
    }
}
