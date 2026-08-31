//! Per-response usage accounting with versioned provider price cards.
//!
//! Billing is independent from provider, chat, and session types. Callers
//! submit the response facts they have; recording never fails. A missing or
//! unusable price becomes an unpriced entry instead of interrupting the model
//! response path.

use std::cmp::Ordering;
use std::error::Error as StdError;
use std::fmt;

use serde::{Deserialize, Serialize};

pub(crate) const PRICE_CATALOG_SCHEMA: &str = "agul/price-catalog/v0.3";
const FEMTO_UNITS_PER_MILLI: u128 = 1_000_000_000_000;
const DEEPSEEK_CATALOG_JSON: &str = include_str!("billing/deepseek-2026-08-27.json");
const GLM_CATALOG_JSON: &str = include_str!("billing/glm-2026-08-29.json");

mod sync;

pub(crate) use sync::{PriceCatalogStore, PriceSelection};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceRates {
    /// Billionths of one currency unit per one million cache-hit input tokens.
    pub(crate) cache_hit_input_nanos_per_million: u64,
    /// Billionths of one currency unit per one million cache-miss input tokens.
    pub(crate) cache_miss_input_nanos_per_million: u64,
    /// Billionths of one currency unit per one million output tokens.
    pub(crate) output_nanos_per_million: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceBand {
    pub(crate) id: String,
    pub(crate) rates: PriceRates,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UtcWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl UtcWeekday {
    const fn index(self) -> u64 {
        match self {
            Self::Mon => 0,
            Self::Tue => 1,
            Self::Wed => 2,
            Self::Thu => 3,
            Self::Fri => 4,
            Self::Sat => 5,
            Self::Sun => 6,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceSchedule {
    pub(crate) band: String,
    pub(crate) weekdays_utc: Vec<UtcWeekday>,
    pub(crate) start_minute_utc: u16,
    pub(crate) end_minute_utc: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceCard {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) origin: String,
    pub(crate) models: Vec<String>,
    pub(crate) effective_from: u64,
    pub(crate) effective_until: Option<u64>,
    pub(crate) default_band: String,
    pub(crate) bands: Vec<PriceBand>,
    #[serde(default)]
    pub(crate) schedule: Vec<PriceSchedule>,
}

impl PriceCard {
    fn band_at(&self, unix_seconds: u64) -> &PriceBand {
        let days = unix_seconds / 86_400;
        let weekday = (days + UtcWeekday::Thu.index()) % 7;
        let minute = ((unix_seconds % 86_400) / 60) as u16;
        let band_id = self
            .schedule
            .iter()
            .find(|window| {
                window.weekdays_utc.iter().any(|day| day.index() == weekday)
                    && window.start_minute_utc <= minute
                    && minute < window.end_minute_utc
            })
            .map_or(self.default_band.as_str(), |window| window.band.as_str());
        self.bands
            .iter()
            .find(|band| band.id == band_id)
            .expect("validated price card references an existing band")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceCatalog {
    pub(crate) schema: String,
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) source: String,
    pub(crate) source_checked_at: u64,
    pub(crate) review_after: Option<u64>,
    pub(crate) currency: String,
    pub(crate) cards: Vec<PriceCard>,
}

impl PriceCatalog {
    pub(crate) fn from_json(json: &str) -> Result<Self, BillingError> {
        let catalog: Self = serde_json::from_str(json)
            .map_err(|error| BillingError::new(format!("invalid price catalog JSON: {error}")))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub(crate) fn builtin_deepseek_usd() -> Self {
        Self::from_json(DEEPSEEK_CATALOG_JSON)
            .expect("the embedded DeepSeek price catalog must remain valid")
    }

    pub(crate) fn builtin_glm_cny() -> Self {
        Self::from_json(GLM_CATALOG_JSON).expect("the embedded GLM price catalog must remain valid")
    }

    fn validate(&self) -> Result<(), BillingError> {
        if self.schema != PRICE_CATALOG_SCHEMA {
            return Err(BillingError::new(format!(
                "unsupported price catalog schema `{}`",
                self.schema
            )));
        }
        for (field, value) in [
            ("id", self.id.as_str()),
            ("version", self.version.as_str()),
            ("source", self.source.as_str()),
            ("currency", self.currency.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(BillingError::new(format!(
                    "price catalog {field} must not be empty"
                )));
            }
        }
        version_parts(&self.version)?;
        if self
            .review_after
            .is_some_and(|review_after| review_after <= self.source_checked_at)
        {
            return Err(BillingError::new(
                "price catalog review_after must be later than source_checked_at",
            ));
        }
        if self.cards.is_empty() {
            return Err(BillingError::new(
                "price catalog must contain at least one card",
            ));
        }
        for (index, card) in self.cards.iter().enumerate() {
            validate_card(card, index)?;
            if self.cards[..index].iter().any(|other| other.id == card.id) {
                return Err(BillingError::new(format!(
                    "duplicate price card id `{}`",
                    card.id
                )));
            }
            for (other_index, other) in self.cards[..index].iter().enumerate() {
                if cards_overlap(other, card) {
                    return Err(BillingError::new(format!(
                        "price catalog cards {other_index} and {index} overlap for the same model"
                    )));
                }
            }
        }
        Ok(())
    }

    fn quote(&self, response: &ResponseUsage) -> Result<Quote, UnpricedReason> {
        let mut matches = self.cards.iter().filter(|card| {
            card.provider == response.provider
                && same_origin(&card.origin, &response.origin)
                && card
                    .models
                    .iter()
                    .any(|model| model == &response.reported_model)
                && response.observed_at_unix_seconds >= card.effective_from
                && card
                    .effective_until
                    .is_none_or(|until| response.observed_at_unix_seconds < until)
        });
        let Some(card) = matches.next() else {
            return Err(UnpricedReason::NoMatchingPriceCard);
        };
        if matches.next().is_some() {
            return Err(UnpricedReason::AmbiguousPriceCard);
        }
        let usage = response
            .usage
            .as_ref()
            .ok_or(UnpricedReason::UsageMissing)?;
        let cache = split_cache(usage)?;
        validate_usage(usage)?;

        let band = card.band_at(response.observed_at_unix_seconds);
        let rates = &band.rates;
        let hit_cost = price_component(cache.hit, rates.cache_hit_input_nanos_per_million)?;
        let miss_cost = price_component(cache.miss, rates.cache_miss_input_nanos_per_million)?;
        let output_cost = price_component(usage.output_tokens, rates.output_nanos_per_million)?;
        let femto_units = hit_cost
            .checked_add(miss_cost)
            .and_then(|amount| amount.checked_add(output_cost))
            .ok_or(UnpricedReason::CostOverflow)?;

        Ok(Quote {
            cost: FixedMoney::new(&self.currency, femto_units),
            price_ref: PriceReference {
                catalog_id: self.id.clone(),
                catalog_version: self.version.clone(),
                catalog_source: self.source.clone(),
                source_checked_at: self.source_checked_at,
                review_after: self.review_after,
                card_id: card.id.clone(),
                band_id: band.id.clone(),
                currency: self.currency.clone(),
                effective_from: card.effective_from,
                effective_until: card.effective_until,
                rates: rates.clone(),
            },
            stale: self
                .review_after
                .is_some_and(|review_after| response.observed_at_unix_seconds >= review_after),
            assumptions: if cache.assumed_all_miss {
                vec![PricingAssumption::AllInputCacheMiss]
            } else {
                Vec::new()
            },
        })
    }
}

fn validate_card(card: &PriceCard, index: usize) -> Result<(), BillingError> {
    for (field, value) in [
        ("id", card.id.as_str()),
        ("provider", card.provider.as_str()),
        ("origin", card.origin.as_str()),
        ("default_band", card.default_band.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(BillingError::new(format!(
                "price catalog card {index} {field} must not be empty"
            )));
        }
    }
    if card.models.is_empty() || card.models.iter().any(|model| model.trim().is_empty()) {
        return Err(BillingError::new(format!(
            "price catalog card {index} must contain non-empty models"
        )));
    }
    if card.bands.is_empty() {
        return Err(BillingError::new(format!(
            "price catalog card {index} must contain at least one band"
        )));
    }
    for (band_index, band) in card.bands.iter().enumerate() {
        if band.id.trim().is_empty() {
            return Err(BillingError::new(format!(
                "price catalog card {index} band {band_index} id must not be empty"
            )));
        }
        if card.bands[..band_index]
            .iter()
            .any(|other| other.id == band.id)
        {
            return Err(BillingError::new(format!(
                "price catalog card {index} has duplicate band id `{}`",
                band.id
            )));
        }
    }
    if !card.bands.iter().any(|band| band.id == card.default_band) {
        return Err(BillingError::new(format!(
            "price catalog card {index} default_band `{}` does not exist",
            card.default_band
        )));
    }
    for (window_index, window) in card.schedule.iter().enumerate() {
        if !card.bands.iter().any(|band| band.id == window.band) {
            return Err(BillingError::new(format!(
                "price catalog card {index} schedule {window_index} band `{}` does not exist",
                window.band
            )));
        }
        if window.weekdays_utc.is_empty() {
            return Err(BillingError::new(format!(
                "price catalog card {index} schedule {window_index} weekdays_utc must not be empty"
            )));
        }
        let mut days = std::collections::HashSet::new();
        if window.weekdays_utc.iter().any(|day| !days.insert(*day)) {
            return Err(BillingError::new(format!(
                "price catalog card {index} schedule {window_index} repeats a weekday"
            )));
        }
        if window.start_minute_utc >= window.end_minute_utc || window.end_minute_utc > 1_440 {
            return Err(BillingError::new(format!(
                "price catalog card {index} schedule {window_index} must be within one UTC day"
            )));
        }
        for (other_index, other) in card.schedule[..window_index].iter().enumerate() {
            let shares_day = window
                .weekdays_utc
                .iter()
                .any(|day| other.weekdays_utc.contains(day));
            let overlaps = window.start_minute_utc < other.end_minute_utc
                && other.start_minute_utc < window.end_minute_utc;
            if shares_day && overlaps {
                return Err(BillingError::new(format!(
                    "price catalog card {index} schedules {other_index} and {window_index} overlap"
                )));
            }
        }
    }
    if card
        .effective_until
        .is_some_and(|until| until <= card.effective_from)
    {
        return Err(BillingError::new(format!(
            "price catalog card {index} effective_until must be later than effective_from"
        )));
    }
    Ok(())
}

fn cards_overlap(left: &PriceCard, right: &PriceCard) -> bool {
    left.provider == right.provider
        && same_origin(&left.origin, &right.origin)
        && left.models.iter().any(|model| right.models.contains(model))
        && left.effective_from < right.effective_until.unwrap_or(u64::MAX)
        && right.effective_from < left.effective_until.unwrap_or(u64::MAX)
}

fn version_parts(version: &str) -> Result<Vec<u64>, BillingError> {
    let parts = version
        .split(['.', '-'])
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(BillingError::new(
                    "price catalog version must contain numeric components separated by dots or dashes",
                ));
            }
            part.parse::<u64>()
                .map_err(|_| BillingError::new("price catalog version component is too large"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.is_empty() {
        return Err(BillingError::new("price catalog version must not be empty"));
    }
    Ok(parts)
}

pub(super) fn compare_versions(left: &str, right: &str) -> Result<Ordering, BillingError> {
    let left = version_parts(left)?;
    let right = version_parts(right)?;
    let count = left.len().max(right.len());
    Ok((0..count)
        .map(|index| {
            left.get(index)
                .copied()
                .unwrap_or(0)
                .cmp(&right.get(index).copied().unwrap_or(0))
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal))
}

fn same_origin(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsagePurpose {
    Chat,
    Compaction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationTimeSource {
    Provider,
    Host,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: Option<u64>,
    pub(crate) cache_hit_input_tokens: Option<u64>,
    pub(crate) cache_miss_input_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseUsage {
    pub(crate) purpose: UsagePurpose,
    pub(crate) provider: String,
    pub(crate) origin: String,
    pub(crate) response_id: Option<String>,
    pub(crate) observed_at_unix_seconds: u64,
    pub(crate) observation_time_source: ObservationTimeSource,
    pub(crate) reported_model: String,
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PricingAssumption {
    AllInputCacheMiss,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnpricedReason {
    PriceCatalogMissing,
    SubscriptionQuota,
    UsageMissing,
    NoMatchingPriceCard,
    AmbiguousPriceCard,
    InvalidUsage,
    CostOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceReference {
    pub(crate) catalog_id: String,
    pub(crate) catalog_version: String,
    pub(crate) catalog_source: String,
    pub(crate) source_checked_at: u64,
    pub(crate) review_after: Option<u64>,
    pub(crate) card_id: String,
    pub(crate) band_id: String,
    pub(crate) currency: String,
    pub(crate) effective_from: u64,
    pub(crate) effective_until: Option<u64>,
    pub(crate) rates: PriceRates,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixedMoney {
    pub(crate) currency: String,
    #[serde(with = "decimal_u128")]
    pub(crate) femto_units: u128,
}

impl FixedMoney {
    fn new(currency: &str, femto_units: u128) -> Self {
        Self {
            currency: currency.to_string(),
            femto_units,
        }
    }

    pub(crate) const fn femto_units(&self) -> u128 {
        self.femto_units
    }
}

/// Formats a femtocurrency amount to three decimal places. A positive amount
/// too small to display at that precision remains visibly non-zero.
pub(crate) fn format_femto_amount_3dp(femto_units: u128) -> String {
    if femto_units == 0 {
        return "0.000".to_string();
    }
    if femto_units < FEMTO_UNITS_PER_MILLI {
        return "<0.001".to_string();
    }
    let mut milli_units = femto_units / FEMTO_UNITS_PER_MILLI;
    if femto_units % FEMTO_UNITS_PER_MILLI >= FEMTO_UNITS_PER_MILLI / 2 {
        milli_units = milli_units.saturating_add(1);
    }
    format!("{}.{:03}", milli_units / 1_000, milli_units % 1_000)
}

/// One provider response and its pricing outcome. Token fields remain flat so
/// the record can be inspected without resolving another usage object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageEntry {
    pub(crate) purpose: UsagePurpose,
    pub(crate) provider: String,
    pub(crate) origin: String,
    pub(crate) response_id: Option<String>,
    pub(crate) observed_at_unix_seconds: u64,
    pub(crate) observation_time_source: ObservationTimeSource,
    pub(crate) reported_model: String,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) total_tokens: Option<u64>,
    pub(crate) cache_hit_input_tokens: Option<u64>,
    pub(crate) cache_miss_input_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) cost: Option<FixedMoney>,
    pub(crate) price_ref: Option<PriceReference>,
    pub(crate) stale: bool,
    pub(crate) assumptions: Vec<PricingAssumption>,
    pub(crate) unpriced_reason: Option<UnpricedReason>,
}

impl UsageEntry {
    fn unpriced(response: ResponseUsage, reason: UnpricedReason) -> Self {
        Self::from_response(response, None, None, false, Vec::new(), Some(reason))
    }

    fn priced(response: ResponseUsage, quote: Quote) -> Self {
        Self::from_response(
            response,
            Some(quote.cost),
            Some(quote.price_ref),
            quote.stale,
            quote.assumptions,
            None,
        )
    }

    fn from_response(
        response: ResponseUsage,
        cost: Option<FixedMoney>,
        price_ref: Option<PriceReference>,
        stale: bool,
        assumptions: Vec<PricingAssumption>,
        unpriced_reason: Option<UnpricedReason>,
    ) -> Self {
        let ResponseUsage {
            purpose,
            provider,
            origin,
            response_id,
            observed_at_unix_seconds,
            observation_time_source,
            reported_model,
            usage,
        } = response;
        let (
            input_tokens,
            output_tokens,
            total_tokens,
            cache_hit_input_tokens,
            cache_miss_input_tokens,
            reasoning_tokens,
        ) = usage.map_or((None, None, None, None, None, None), |usage| {
            (
                Some(usage.input_tokens),
                Some(usage.output_tokens),
                usage.total_tokens,
                usage.cache_hit_input_tokens,
                usage.cache_miss_input_tokens,
                usage.reasoning_tokens,
            )
        });
        Self {
            purpose,
            provider,
            origin,
            response_id,
            observed_at_unix_seconds,
            observation_time_source,
            reported_model,
            input_tokens,
            output_tokens,
            total_tokens,
            cache_hit_input_tokens,
            cache_miss_input_tokens,
            reasoning_tokens,
            cost,
            price_ref,
            stale,
            assumptions,
            unpriced_reason,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PricingStatus {
    Priced,
    Partial,
    #[default]
    Unpriced,
    Unavailable,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageSummary {
    pub(crate) responses: u64,
    pub(crate) chat_responses: u64,
    pub(crate) compaction_responses: u64,
    pub(crate) responses_with_usage: u64,
    pub(crate) priced_responses: u64,
    pub(crate) unpriced_responses: u64,
    pub(crate) stale_price_responses: u64,
    pub(crate) assumed_price_responses: u64,
    pub(crate) input_tokens: u128,
    pub(crate) output_tokens: u128,
    pub(crate) total_tokens: u128,
    pub(crate) cache_hit_input_tokens: u128,
    pub(crate) cache_miss_input_tokens: u128,
    #[serde(skip)]
    pub(crate) reported_cache_input_tokens: u128,
    pub(crate) reasoning_tokens: u128,
    pub(crate) total_cost: Option<FixedMoney>,
    pub(crate) total_cost_unavailable: bool,
    pub(crate) pricing_status: PricingStatus,
}

impl UsageSummary {
    pub(crate) fn from_entries(entries: &[UsageEntry]) -> Self {
        let mut summary = Self::default();
        for entry in entries {
            add_to_summary(&mut summary, entry);
        }
        summary
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.responses = self.responses.saturating_add(other.responses);
        self.chat_responses = self.chat_responses.saturating_add(other.chat_responses);
        self.compaction_responses = self
            .compaction_responses
            .saturating_add(other.compaction_responses);
        self.responses_with_usage = self
            .responses_with_usage
            .saturating_add(other.responses_with_usage);
        self.priced_responses = self.priced_responses.saturating_add(other.priced_responses);
        self.unpriced_responses = self
            .unpriced_responses
            .saturating_add(other.unpriced_responses);
        self.stale_price_responses = self
            .stale_price_responses
            .saturating_add(other.stale_price_responses);
        self.assumed_price_responses = self
            .assumed_price_responses
            .saturating_add(other.assumed_price_responses);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cache_hit_input_tokens = self
            .cache_hit_input_tokens
            .saturating_add(other.cache_hit_input_tokens);
        self.cache_miss_input_tokens = self
            .cache_miss_input_tokens
            .saturating_add(other.cache_miss_input_tokens);
        self.reported_cache_input_tokens = self
            .reported_cache_input_tokens
            .saturating_add(other.reported_cache_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        merge_cost(self, other);
        refresh_pricing_status(self);
    }

    pub(crate) fn reported_cache_hit_percent(&self) -> Option<f64> {
        (self.reported_cache_input_tokens > 0).then(|| {
            self.cache_hit_input_tokens as f64 * 100.0 / self.reported_cache_input_tokens as f64
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageLedger {
    catalog: Option<PriceCatalog>,
    pub(crate) entries: Vec<UsageEntry>,
    pub(crate) summary: UsageSummary,
}

impl UsageLedger {
    pub(crate) fn new(catalog: Option<PriceCatalog>) -> Self {
        Self {
            catalog,
            entries: Vec::new(),
            summary: UsageSummary::default(),
        }
    }

    pub(crate) fn from_entries(catalog: Option<PriceCatalog>, entries: Vec<UsageEntry>) -> Self {
        let summary = UsageSummary::from_entries(&entries);
        Self {
            catalog,
            entries,
            summary,
        }
    }

    pub(crate) fn record(&mut self, response: ResponseUsage) -> &UsageEntry {
        let entry = match &self.catalog {
            Some(catalog) => match catalog.quote(&response) {
                Ok(quote) => UsageEntry::priced(response, quote),
                Err(reason) => UsageEntry::unpriced(response, reason),
            },
            None => UsageEntry::unpriced(response, UnpricedReason::PriceCatalogMissing),
        };
        self.push(entry)
    }

    pub(crate) fn record_unpriced(
        &mut self,
        response: ResponseUsage,
        reason: UnpricedReason,
    ) -> &UsageEntry {
        self.push(UsageEntry::unpriced(response, reason))
    }

    fn push(&mut self, entry: UsageEntry) -> &UsageEntry {
        add_to_summary(&mut self.summary, &entry);
        self.entries.push(entry);
        self.entries
            .last()
            .expect("a just-recorded usage entry must exist")
    }

    pub(crate) fn entries(&self) -> &[UsageEntry] {
        &self.entries
    }

    pub(crate) const fn summary(&self) -> &UsageSummary {
        &self.summary
    }
}

fn add_to_summary(summary: &mut UsageSummary, entry: &UsageEntry) {
    summary.responses += 1;
    match entry.purpose {
        UsagePurpose::Chat => summary.chat_responses += 1,
        UsagePurpose::Compaction => summary.compaction_responses += 1,
    }
    if entry.input_tokens.is_some() && entry.output_tokens.is_some() {
        summary.responses_with_usage += 1;
    }
    summary.input_tokens += u128::from(entry.input_tokens.unwrap_or(0));
    summary.output_tokens += u128::from(entry.output_tokens.unwrap_or(0));
    summary.total_tokens += u128::from(entry.total_tokens.unwrap_or_else(|| {
        entry
            .input_tokens
            .unwrap_or(0)
            .saturating_add(entry.output_tokens.unwrap_or(0))
    }));
    summary.reasoning_tokens += u128::from(entry.reasoning_tokens.unwrap_or(0));
    if let Some((hit, miss)) = effective_cache_split(entry) {
        summary.cache_hit_input_tokens += u128::from(hit);
        summary.cache_miss_input_tokens += u128::from(miss);
        if entry.cache_hit_input_tokens.is_some() || entry.cache_miss_input_tokens.is_some() {
            summary.reported_cache_input_tokens = summary
                .reported_cache_input_tokens
                .saturating_add(u128::from(hit).saturating_add(u128::from(miss)));
        }
    }
    if entry.stale {
        summary.stale_price_responses += 1;
    }
    if !entry.assumptions.is_empty() {
        summary.assumed_price_responses += 1;
    }
    let Some(cost) = &entry.cost else {
        summary.unpriced_responses += 1;
        refresh_pricing_status(summary);
        return;
    };
    summary.priced_responses += 1;
    if summary.total_cost_unavailable {
        refresh_pricing_status(summary);
        return;
    }
    match &mut summary.total_cost {
        Some(total) if total.currency == cost.currency => {
            let Some(combined) = total.femto_units.checked_add(cost.femto_units) else {
                summary.total_cost = None;
                summary.total_cost_unavailable = true;
                refresh_pricing_status(summary);
                return;
            };
            total.femto_units = combined;
        }
        Some(_) => {
            summary.total_cost = None;
            summary.total_cost_unavailable = true;
        }
        None => summary.total_cost = Some(cost.clone()),
    }
    refresh_pricing_status(summary);
}

fn merge_cost(summary: &mut UsageSummary, other: &UsageSummary) {
    if summary.total_cost_unavailable || other.total_cost_unavailable {
        summary.total_cost = None;
        summary.total_cost_unavailable = true;
        return;
    }
    let Some(other_cost) = &other.total_cost else {
        return;
    };
    match &mut summary.total_cost {
        Some(total) if total.currency == other_cost.currency => {
            let Some(combined) = total.femto_units.checked_add(other_cost.femto_units) else {
                summary.total_cost = None;
                summary.total_cost_unavailable = true;
                return;
            };
            total.femto_units = combined;
        }
        Some(_) => {
            summary.total_cost = None;
            summary.total_cost_unavailable = true;
        }
        None => summary.total_cost = Some(other_cost.clone()),
    }
}

fn refresh_pricing_status(summary: &mut UsageSummary) {
    summary.pricing_status = if summary.total_cost_unavailable {
        PricingStatus::Unavailable
    } else if summary.priced_responses > 0 && summary.unpriced_responses > 0 {
        PricingStatus::Partial
    } else if summary.priced_responses > 0 {
        PricingStatus::Priced
    } else {
        PricingStatus::Unpriced
    };
}

fn effective_cache_split(entry: &UsageEntry) -> Option<(u64, u64)> {
    let all_miss_was_assumed = entry
        .assumptions
        .contains(&PricingAssumption::AllInputCacheMiss);
    if entry.cache_hit_input_tokens.is_none()
        && entry.cache_miss_input_tokens.is_none()
        && !all_miss_was_assumed
    {
        return None;
    }
    split_cache_fields(
        entry.input_tokens?,
        entry.cache_hit_input_tokens,
        entry.cache_miss_input_tokens,
    )
    .ok()
    .map(|cache| (cache.hit, cache.miss))
}

struct Quote {
    cost: FixedMoney,
    price_ref: PriceReference,
    stale: bool,
    assumptions: Vec<PricingAssumption>,
}

#[derive(Clone, Copy)]
struct CacheSplit {
    hit: u64,
    miss: u64,
    assumed_all_miss: bool,
}

fn split_cache(usage: &TokenUsage) -> Result<CacheSplit, UnpricedReason> {
    split_cache_fields(
        usage.input_tokens,
        usage.cache_hit_input_tokens,
        usage.cache_miss_input_tokens,
    )
}

fn split_cache_fields(
    input: u64,
    cache_hit: Option<u64>,
    cache_miss: Option<u64>,
) -> Result<CacheSplit, UnpricedReason> {
    let (hit, miss, assumed_all_miss) = match (cache_hit, cache_miss) {
        (Some(hit), Some(miss)) if hit.checked_add(miss) == Some(input) => (hit, miss, false),
        (Some(_), Some(_)) => return Err(UnpricedReason::InvalidUsage),
        (Some(hit), None) => (
            hit,
            input.checked_sub(hit).ok_or(UnpricedReason::InvalidUsage)?,
            false,
        ),
        (None, Some(miss)) => (
            input
                .checked_sub(miss)
                .ok_or(UnpricedReason::InvalidUsage)?,
            miss,
            false,
        ),
        (None, None) => (0, input, true),
    };
    Ok(CacheSplit {
        hit,
        miss,
        assumed_all_miss,
    })
}

fn validate_usage(usage: &TokenUsage) -> Result<(), UnpricedReason> {
    if usage
        .total_tokens
        .is_some_and(|total| usage.input_tokens.checked_add(usage.output_tokens) != Some(total))
        || usage
            .reasoning_tokens
            .is_some_and(|reasoning| reasoning > usage.output_tokens)
    {
        return Err(UnpricedReason::InvalidUsage);
    }
    Ok(())
}

fn price_component(tokens: u64, nanos_per_million: u64) -> Result<u128, UnpricedReason> {
    // One nanocurrency unit per one million tokens equals one femtocurrency
    // unit per token, so the multiplication is exact and requires no rounding.
    u128::from(tokens)
        .checked_mul(u128::from(nanos_per_million))
        .ok_or(UnpricedReason::CostOverflow)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BillingError {
    message: String,
}

impl BillingError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BillingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for BillingError {}

mod decimal_u128 {
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub(super) fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
#[path = "billing/tests.rs"]
mod tests;
