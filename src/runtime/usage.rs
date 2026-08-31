use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::billing::{
    ObservationTimeSource, PriceCatalog, ResponseUsage, TokenUsage, UnpricedReason, UsageEntry,
    UsageLedger, UsagePurpose,
};
use super::direct_chat::ResponseObservation;
use super::{
    DEEPSEEK_DEFAULT_API_KEY_ENV, DEEPSEEK_DEFAULT_BASE_URL, DEEPSEEK_DEFAULT_MODEL,
    GLM_CODING_DEFAULT_BASE_URL, GLM_CODING_DEFAULT_MODEL, GLM_DEFAULT_API_KEY_ENV,
    GLM_DEFAULT_BASE_URL, GLM_DEFAULT_MODEL,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NativeConnectionPreset {
    #[default]
    Deepseek,
    Glm,
    GlmCoding,
}

impl NativeConnectionPreset {
    pub(crate) const fn provider(self) -> NativeProvider {
        match self {
            Self::Deepseek => NativeProvider::Deepseek,
            Self::Glm | Self::GlmCoding => NativeProvider::Glm,
        }
    }

    pub(crate) const fn base_url(self) -> &'static str {
        match self {
            Self::Deepseek => DEEPSEEK_DEFAULT_BASE_URL,
            Self::Glm => GLM_DEFAULT_BASE_URL,
            Self::GlmCoding => GLM_CODING_DEFAULT_BASE_URL,
        }
    }

    pub(crate) const fn model(self) -> &'static str {
        match self {
            Self::Deepseek => DEEPSEEK_DEFAULT_MODEL,
            Self::Glm => GLM_DEFAULT_MODEL,
            Self::GlmCoding => GLM_CODING_DEFAULT_MODEL,
        }
    }

    pub(crate) const fn api_key_env(self) -> &'static str {
        self.provider().api_key_env()
    }

    pub(crate) const fn is_subscription(self) -> bool {
        matches!(self, Self::GlmCoding)
    }

    pub(crate) fn from_official_endpoint(endpoint: &str) -> Option<Self> {
        let url = reqwest::Url::parse(endpoint).ok()?;
        let host = url.host_str()?;
        if host.eq_ignore_ascii_case("api.deepseek.com") {
            return Some(Self::Deepseek);
        }
        if !host.eq_ignore_ascii_case("open.bigmodel.cn") {
            return None;
        }
        if is_glm_coding_path(url.path()) {
            Some(Self::GlmCoding)
        } else {
            Some(Self::Glm)
        }
    }

    pub(crate) fn validate_official_endpoint(self, endpoint: &str) -> Result<(), String> {
        if let Some(actual) = Self::from_official_endpoint(endpoint)
            && actual != self
        {
            return Err(format!(
                "provider {self} conflicts with URL provider {actual}"
            ));
        }
        Ok(())
    }
}

impl FromStr for NativeConnectionPreset {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Ok(Self::Deepseek),
            "glm" | "glm-coding" => Ok(Self::GlmCoding),
            _ => Err("provider must be `deepseek` or `glm`".to_string()),
        }
    }
}

impl fmt::Display for NativeConnectionPreset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Deepseek => "deepseek",
            Self::Glm => "glm-api",
            Self::GlmCoding => "glm",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeProvider {
    #[default]
    Deepseek,
    Glm,
}

impl NativeProvider {
    pub(crate) const fn model(self) -> &'static str {
        match self {
            Self::Deepseek => DEEPSEEK_DEFAULT_MODEL,
            Self::Glm => GLM_DEFAULT_MODEL,
        }
    }

    pub(crate) const fn api_key_env(self) -> &'static str {
        match self {
            Self::Deepseek => DEEPSEEK_DEFAULT_API_KEY_ENV,
            Self::Glm => GLM_DEFAULT_API_KEY_ENV,
        }
    }

    pub(crate) fn catalog(self) -> PriceCatalog {
        match self {
            Self::Deepseek => PriceCatalog::builtin_deepseek_usd(),
            Self::Glm => PriceCatalog::builtin_glm_cny(),
        }
    }

    pub(crate) fn normalize_reasoning_effort(
        self,
        requested: Option<&str>,
    ) -> Result<Option<String>, String> {
        let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        if self != Self::Glm {
            return Ok(Some(requested.to_string()));
        }
        let effective = match requested.to_ascii_lowercase().as_str() {
            "none" | "minimal" | "low" => "low",
            "medium" | "high" => "high",
            "xhigh" | "max" | "ultra" => "max",
            _ => {
                return Err(format!(
                    "GLM reasoning effort must be one of none, minimal, low, medium, high, xhigh, max, or ultra; got `{requested}`"
                ));
            }
        };
        Ok(Some(effective.to_string()))
    }
}

impl FromStr for NativeProvider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Ok(Self::Deepseek),
            "glm" => Ok(Self::Glm),
            _ => Err("provider must be `deepseek` or `glm`".to_string()),
        }
    }
}

impl fmt::Display for NativeProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Deepseek => "deepseek",
            Self::Glm => "glm",
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderIdentity {
    provider: String,
    origin: String,
    billing: ProviderBilling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderBilling {
    PriceCatalog,
    ChatgptQuota,
    SubscriptionQuota,
}

impl ProviderIdentity {
    pub(crate) fn from_endpoint(endpoint: &str) -> Self {
        let parsed = reqwest::Url::parse(endpoint).ok();
        let provider = match parsed.as_ref().and_then(reqwest::Url::host_str) {
            Some(host) if host.eq_ignore_ascii_case("api.deepseek.com") => "deepseek",
            Some(host) if host.eq_ignore_ascii_case("open.bigmodel.cn") => "glm",
            _ => "openai-compatible",
        };
        Self {
            provider: provider.to_string(),
            billing: if parsed.as_ref().is_some_and(|url| {
                url.host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case("open.bigmodel.cn"))
                    && is_glm_coding_path(url.path())
            }) {
                ProviderBilling::SubscriptionQuota
            } else {
                ProviderBilling::PriceCatalog
            },
            origin: parsed
                .map(|url| url.origin().ascii_serialization())
                .unwrap_or_else(|| endpoint.to_string()),
        }
    }

    pub(crate) fn codex_subscription() -> Self {
        Self {
            provider: "codex".to_string(),
            origin: "codex://chatgpt".to_string(),
            billing: ProviderBilling::ChatgptQuota,
        }
    }

    pub(crate) fn from_native_endpoint(provider: Option<NativeProvider>, endpoint: &str) -> Self {
        let mut identity = Self::from_endpoint(endpoint);
        if let Some(provider) = provider {
            identity.provider = provider.to_string();
        }
        identity
    }

    pub(crate) fn from_native_preset(
        preset: Option<NativeConnectionPreset>,
        provider: Option<NativeProvider>,
        endpoint: &str,
    ) -> Self {
        let mut identity = Self::from_native_endpoint(provider, endpoint);
        if let Some(preset) = preset {
            identity.billing = if preset.is_subscription() {
                ProviderBilling::SubscriptionQuota
            } else {
                ProviderBilling::PriceCatalog
            };
        }
        identity
    }

    pub(crate) fn provider_name(&self) -> &str {
        &self.provider
    }

    pub(crate) fn is_subscription(&self) -> bool {
        self.billing != ProviderBilling::PriceCatalog
    }

    pub(crate) fn billing_label(&self) -> &'static str {
        match self.billing {
            ProviderBilling::PriceCatalog => "price_catalog",
            ProviderBilling::ChatgptQuota => "chatgpt_quota",
            ProviderBilling::SubscriptionQuota => "subscription_quota",
        }
    }

    pub(crate) fn quota_label(&self) -> Option<&'static str> {
        match self.billing {
            ProviderBilling::PriceCatalog => None,
            ProviderBilling::ChatgptQuota => Some("ChatGPT quota"),
            ProviderBilling::SubscriptionQuota => Some("subscription quota"),
        }
    }

    pub(crate) fn default_catalog(&self) -> Option<PriceCatalog> {
        if self.is_subscription() {
            return None;
        }
        match self.provider.as_str() {
            "deepseek" => Some(NativeProvider::Deepseek.catalog()),
            "glm" => Some(NativeProvider::Glm.catalog()),
            _ => None,
        }
    }

    pub(crate) fn default_api_key_env(&self) -> Option<String> {
        self.native_provider()
            .map(|provider| provider.api_key_env().to_string())
    }

    pub(crate) fn default_model(&self) -> Option<&'static str> {
        self.native_provider().map(NativeProvider::model)
    }

    pub(crate) fn native_provider(&self) -> Option<NativeProvider> {
        match self.provider.as_str() {
            "deepseek" => Some(NativeProvider::Deepseek),
            "glm" => Some(NativeProvider::Glm),
            _ => None,
        }
    }

    pub(crate) fn validate_native_provider(
        &self,
        expected: Option<NativeProvider>,
    ) -> Result<(), String> {
        if let (Some(expected), Some(actual)) = (expected, self.native_provider())
            && expected != actual
        {
            return Err(format!(
                "provider {expected} conflicts with URL provider {actual}"
            ));
        }
        Ok(())
    }

    pub(crate) fn record<'a>(
        &self,
        ledger: &'a mut UsageLedger,
        purpose: UsagePurpose,
        response: &ResponseObservation,
    ) -> &'a UsageEntry {
        let usage = self.response_usage(purpose, response);
        if self.is_subscription() {
            ledger.record_unpriced(usage, UnpricedReason::SubscriptionQuota)
        } else {
            ledger.record(usage)
        }
    }

    fn response_usage(
        &self,
        purpose: UsagePurpose,
        response: &ResponseObservation,
    ) -> ResponseUsage {
        let usage = response.usage.as_ref().map(|usage| TokenUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.input_tokens.checked_add(usage.output_tokens),
            cache_hit_input_tokens: usage.cache_hit_tokens,
            cache_miss_input_tokens: usage.cache_miss_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        });
        let (observed_at_unix_seconds, observation_time_source) = response
            .provider_created_at
            .map(|created| (created, ObservationTimeSource::Provider))
            .unwrap_or((response.received_at, ObservationTimeSource::Host));
        ResponseUsage {
            purpose,
            provider: self.provider.clone(),
            origin: self.origin.clone(),
            response_id: response.response_id.clone(),
            observed_at_unix_seconds,
            observation_time_source,
            reported_model: response
                .reported_model
                .clone()
                .unwrap_or_else(|| response.requested_model.clone()),
            usage,
        }
    }
}

fn is_glm_coding_path(path: &str) -> bool {
    path.split('/').any(|segment| segment == "coding")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_provider_identities_select_defaults() {
        let identity =
            ProviderIdentity::from_endpoint("https://api.deepseek.com/v1/chat/completions");
        assert!(identity.default_catalog().is_some());
        assert_eq!(
            identity.default_api_key_env().as_deref(),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(identity.default_model(), Some("deepseek-v4-flash"));

        let glm = ProviderIdentity::from_endpoint(
            "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        );
        let catalog = glm.default_catalog().expect("GLM catalog");
        assert_eq!(catalog.id, "glm-official-cny");
        assert_eq!(glm.default_api_key_env().as_deref(), Some("GLM_API_KEY"));
        assert_eq!(glm.default_model(), Some("glm-5.3"));

        let local = ProviderIdentity::from_endpoint("http://127.0.0.1:51100/v1/chat/completions");
        assert!(local.default_catalog().is_none());
        assert!(local.default_api_key_env().is_none());
        assert_eq!(local.default_model(), None);

        let glm_proxy = ProviderIdentity::from_native_endpoint(
            Some(NativeProvider::Glm),
            "http://127.0.0.1:51100/v1/chat/completions",
        );
        assert_eq!(glm_proxy.provider_name(), "glm");
        assert!(glm_proxy.default_catalog().is_some());

        let codex = ProviderIdentity::codex_subscription();
        assert!(codex.is_subscription());
        assert_eq!(codex.billing_label(), "chatgpt_quota");

        let glm_coding = ProviderIdentity::from_endpoint(
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
        );
        assert_eq!(glm_coding.provider_name(), "glm");
        assert_eq!(glm_coding.billing_label(), "subscription_quota");
        assert!(glm_coding.default_catalog().is_none());
    }

    #[test]
    fn native_provider_names_are_strict_and_stable() {
        assert_eq!("deepseek".parse(), Ok(NativeProvider::Deepseek));
        assert_eq!("GLM".parse(), Ok(NativeProvider::Glm));
        assert!("custom".parse::<NativeProvider>().is_err());
        assert_eq!(NativeProvider::Glm.to_string(), "glm");

        let deepseek = ProviderIdentity::from_endpoint("https://api.deepseek.com");
        assert!(
            deepseek
                .validate_native_provider(Some(NativeProvider::Glm))
                .unwrap_err()
                .contains("provider glm conflicts with URL provider deepseek")
        );
        let proxy = ProviderIdentity::from_endpoint("https://proxy.example/v1");
        assert!(
            proxy
                .validate_native_provider(Some(NativeProvider::Glm))
                .is_ok()
        );
    }

    #[test]
    fn native_connection_presets_separate_routes_from_wire_providers() {
        assert_eq!("glm".parse(), Ok(NativeConnectionPreset::GlmCoding));
        assert_eq!("glm-coding".parse(), Ok(NativeConnectionPreset::GlmCoding));
        assert!("glm-api".parse::<NativeConnectionPreset>().is_err());
        assert_eq!(
            NativeConnectionPreset::GlmCoding.provider(),
            NativeProvider::Glm
        );
        assert_eq!(
            NativeConnectionPreset::GlmCoding.base_url(),
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(NativeConnectionPreset::GlmCoding.model(), "glm-4.7");
        assert_eq!(
            NativeConnectionPreset::from_official_endpoint(
                "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
            ),
            Some(NativeConnectionPreset::GlmCoding)
        );
        assert_eq!(NativeConnectionPreset::Glm.provider(), NativeProvider::Glm);
        assert!(
            NativeConnectionPreset::GlmCoding
                .validate_official_endpoint("https://open.bigmodel.cn/api/paas/v4")
                .unwrap_err()
                .contains("provider glm conflicts with URL provider glm-api")
        );
        assert!(
            NativeConnectionPreset::Glm
                .validate_official_endpoint("https://open.bigmodel.cn/api/coding/paas/v4")
                .unwrap_err()
                .contains("provider glm-api conflicts with URL provider glm")
        );
        assert!(
            NativeConnectionPreset::GlmCoding
                .validate_official_endpoint("https://proxy.example/v1")
                .is_ok()
        );
        assert_eq!(
            ProviderIdentity::from_native_preset(
                Some(NativeConnectionPreset::Glm),
                Some(NativeProvider::Glm),
                "https://proxy.example/coding/v1",
            )
            .billing_label(),
            "price_catalog"
        );
    }

    #[test]
    fn persisted_glm_presets_keep_their_original_route_identity() {
        assert_eq!(
            serde_json::from_str::<NativeConnectionPreset>(r#""glm""#).unwrap(),
            NativeConnectionPreset::Glm
        );
        assert_eq!(
            serde_json::from_str::<NativeConnectionPreset>(r#""glm-coding""#).unwrap(),
            NativeConnectionPreset::GlmCoding
        );
    }

    #[test]
    fn glm_reasoning_effort_maps_to_the_effective_api_values() {
        for (requested, expected) in [
            ("none", "low"),
            ("minimal", "low"),
            ("low", "low"),
            ("medium", "high"),
            ("high", "high"),
            ("xhigh", "max"),
            ("max", "max"),
            ("ultra", "max"),
        ] {
            assert_eq!(
                NativeProvider::Glm
                    .normalize_reasoning_effort(Some(requested))
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            NativeProvider::Glm
                .normalize_reasoning_effort(None)
                .unwrap(),
            None
        );
        assert!(
            NativeProvider::Glm
                .normalize_reasoning_effort(Some("turbo"))
                .unwrap_err()
                .contains("GLM reasoning effort")
        );
    }
}
