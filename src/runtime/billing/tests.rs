use super::*;

const CATALOG_EFFECTIVE_FROM: u64 = 1_787_788_800;
const CATALOG_REVIEW_AFTER: u64 = 1_788_393_600;
const GLM_CATALOG_EFFECTIVE_FROM: u64 = 1_787_961_600;
const GLM_CATALOG_REVIEW_AFTER: u64 = 1_788_566_400;

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: Some(input + output),
        cache_hit_input_tokens: None,
        cache_miss_input_tokens: None,
        reasoning_tokens: None,
    }
}

fn response(purpose: UsagePurpose, model: &str, usage: Option<TokenUsage>) -> ResponseUsage {
    ResponseUsage {
        purpose,
        provider: "deepseek".to_string(),
        origin: "https://api.deepseek.com/".to_string(),
        response_id: Some("response-1".to_string()),
        observed_at_unix_seconds: CATALOG_EFFECTIVE_FROM,
        observation_time_source: ObservationTimeSource::Provider,
        reported_model: model.to_string(),
        usage,
    }
}

#[test]
fn embedded_catalog_is_the_2026_08_27_official_usd_card() {
    let catalog = PriceCatalog::builtin_deepseek_usd();

    assert_eq!(catalog.schema, PRICE_CATALOG_SCHEMA);
    assert_eq!(catalog.id, "deepseek-official-usd");
    assert_eq!(catalog.version, "2026-08-27.1");
    assert_eq!(catalog.currency, "USD");
    assert_eq!(catalog.review_after, Some(CATALOG_REVIEW_AFTER));
    assert_eq!(catalog.cards.len(), 2);

    let flash = &catalog.cards[0];
    assert_eq!(flash.effective_until, None);
    assert_eq!(flash.default_band, "off-peak");
    assert_eq!(flash.schedule.len(), 2);
    let flash_off_peak = band(flash, "off-peak");
    assert_eq!(
        flash_off_peak.rates.cache_hit_input_nanos_per_million,
        7_000_000
    );
    assert_eq!(
        flash_off_peak.rates.cache_miss_input_nanos_per_million,
        220_000_000
    );
    assert_eq!(flash_off_peak.rates.output_nanos_per_million, 660_000_000);
    let flash_peak = band(flash, "peak");
    assert_eq!(
        flash_peak.rates.cache_hit_input_nanos_per_million,
        14_000_000
    );
    assert_eq!(
        flash_peak.rates.cache_miss_input_nanos_per_million,
        440_000_000
    );
    assert_eq!(flash_peak.rates.output_nanos_per_million, 1_320_000_000);

    let pro = &catalog.cards[1];
    let pro_off_peak = band(pro, "off-peak");
    assert_eq!(
        pro_off_peak.rates.cache_hit_input_nanos_per_million,
        22_000_000
    );
    assert_eq!(
        pro_off_peak.rates.cache_miss_input_nanos_per_million,
        660_000_000
    );
    assert_eq!(pro_off_peak.rates.output_nanos_per_million, 1_980_000_000);
    let pro_peak = band(pro, "peak");
    assert_eq!(pro_peak.rates.cache_hit_input_nanos_per_million, 44_000_000);
    assert_eq!(
        pro_peak.rates.cache_miss_input_nanos_per_million,
        1_320_000_000
    );
    assert_eq!(pro_peak.rates.output_nanos_per_million, 3_960_000_000);
}

#[test]
fn embedded_glm_catalog_quotes_cache_hits_in_cny() {
    let catalog = PriceCatalog::builtin_glm_cny();

    assert_eq!(catalog.schema, PRICE_CATALOG_SCHEMA);
    assert_eq!(catalog.id, "glm-official-cny");
    assert_eq!(catalog.version, "2026-08-29");
    assert_eq!(catalog.currency, "CNY");
    assert_eq!(catalog.review_after, Some(GLM_CATALOG_REVIEW_AFTER));
    assert_eq!(catalog.cards.len(), 1);
    assert_eq!(catalog.cards[0].models, ["glm-5.3", "glm-5.2"]);

    let mut tokens = usage(1_000, 100);
    tokens.cache_hit_input_tokens = Some(600);
    let mut record = response(UsagePurpose::Chat, "glm-5.3", Some(tokens));
    record.provider = "glm".to_string();
    record.origin = "https://open.bigmodel.cn".to_string();
    record.observed_at_unix_seconds = GLM_CATALOG_EFFECTIVE_FROM;

    let quote = catalog.quote(&record).expect("GLM quote");
    assert_eq!(quote.cost.currency, "CNY");
    assert_eq!(quote.cost.femto_units, 7_200_000_000_000);
    assert_eq!(quote.price_ref.band_id, "standard");
}

#[test]
fn utc_weekday_schedule_uses_exact_half_open_boundaries() {
    let mut catalog = PriceCatalog::builtin_deepseek_usd();
    for card in &mut catalog.cards {
        card.effective_from = 0;
    }
    // 1970-01-05 was a Monday. Windows are [01:00, 04:00) and [06:00, 10:00).
    let monday = 4 * 86_400;
    for (minute, expected) in [
        (59, "off-peak"),
        (60, "peak"),
        (239, "peak"),
        (240, "off-peak"),
        (359, "off-peak"),
        (360, "peak"),
        (599, "peak"),
        (600, "off-peak"),
    ] {
        assert_eq!(quoted_band(&catalog, monday + minute * 60), expected);
    }
    let saturday = monday + 5 * 86_400;
    assert_eq!(quoted_band(&catalog, saturday + 60 * 60), "off-peak");
}

#[test]
fn records_exact_cost_and_embeds_the_price_reference() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    let mut tokens = usage(1_000, 100);
    tokens.cache_hit_input_tokens = Some(600);
    tokens.cache_miss_input_tokens = Some(400);
    tokens.reasoning_tokens = Some(50);

    let entry = ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(tokens),
    ));

    assert_eq!(entry.response_id.as_deref(), Some("response-1"));
    assert_eq!(entry.input_tokens, Some(1_000));
    assert_eq!(entry.reasoning_tokens, Some(50));
    assert_eq!(entry.cost.as_ref().unwrap().femto_units, 158_200_000_000);
    assert!(!entry.stale);
    assert!(entry.assumptions.is_empty());
    assert_eq!(entry.unpriced_reason, None);

    let price_ref = entry.price_ref.as_ref().unwrap();
    assert_eq!(price_ref.catalog_version, "2026-08-27.1");
    assert_eq!(price_ref.card_id, "deepseek-v4-flash-standard");
    assert_eq!(price_ref.band_id, "off-peak");
    assert_eq!(
        price_ref.rates.cache_miss_input_nanos_per_million,
        220_000_000
    );

    let json = serde_json::to_value(entry).unwrap();
    assert_eq!(json["cost"]["femto_units"], "158200000000");
}

#[test]
fn missing_cache_split_is_priced_as_all_miss_and_marked() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));

    let entry = ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(usage(1_000, 0)),
    ));

    assert_eq!(entry.cache_hit_input_tokens, None);
    assert_eq!(entry.cache_miss_input_tokens, None);
    assert_eq!(
        entry.assumptions,
        vec![PricingAssumption::AllInputCacheMiss]
    );
    assert_eq!(entry.cost.as_ref().unwrap().femto_units, 220_000_000_000);
    assert_eq!(ledger.summary().cache_hit_input_tokens, 0);
    assert_eq!(ledger.summary().cache_miss_input_tokens, 1_000);
    assert_eq!(ledger.summary().reported_cache_hit_percent(), None);
    assert_eq!(ledger.summary().assumed_price_responses, 1);
}

#[test]
fn cache_hit_percent_uses_only_provider_reported_splits() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    let mut reported = usage(1_000, 100);
    reported.cache_hit_input_tokens = Some(600);
    reported.cache_miss_input_tokens = Some(400);
    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(reported),
    ));
    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(usage(2_000, 100)),
    ));

    let summary = ledger.summary();
    assert_eq!(summary.input_tokens, 3_000);
    assert_eq!(summary.cache_hit_input_tokens, 600);
    assert_eq!(summary.cache_miss_input_tokens, 2_400);
    assert_eq!(summary.reported_cache_input_tokens, 1_000);
    assert_eq!(summary.reported_cache_hit_percent(), Some(60.0));
}

#[test]
fn review_after_marks_stale_without_suppressing_last_known_price() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    let mut record = response(UsagePurpose::Chat, "deepseek-v4-flash", Some(usage(10, 1)));
    record.observed_at_unix_seconds = CATALOG_REVIEW_AFTER + 86_400;

    let entry = ledger.record(record);

    assert!(entry.stale);
    assert!(entry.cost.is_some());
    assert!(entry.price_ref.is_some());
    assert_eq!(entry.unpriced_reason, None);
}

#[test]
fn optional_effective_until_excludes_later_responses() {
    let mut document: serde_json::Value = serde_json::from_str(DEEPSEEK_CATALOG_JSON).unwrap();
    document["cards"][0]["effective_until"] = serde_json::Value::from(CATALOG_EFFECTIVE_FROM + 10);
    let json = serde_json::to_string(&document).unwrap();
    let mut ledger = UsageLedger::new(Some(PriceCatalog::from_json(&json).unwrap()));
    let mut record = response(UsagePurpose::Chat, "deepseek-v4-flash", Some(usage(1, 1)));
    record.observed_at_unix_seconds = CATALOG_EFFECTIVE_FROM + 10;

    let entry = ledger.record(record);

    assert_eq!(entry.cost, None);
    assert_eq!(
        entry.unpriced_reason,
        Some(UnpricedReason::NoMatchingPriceCard)
    );
}

#[test]
fn record_failures_are_unpriced_entries_not_errors() {
    let missing_usage = response(UsagePurpose::Chat, "deepseek-v4-flash", None);
    let mut priced_ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    assert_eq!(
        priced_ledger.record(missing_usage).unpriced_reason,
        Some(UnpricedReason::UsageMissing)
    );

    let mut no_catalog = UsageLedger::new(None);
    assert_eq!(
        no_catalog
            .record(response(UsagePurpose::Chat, "any-model", Some(usage(1, 1)),))
            .unpriced_reason,
        Some(UnpricedReason::PriceCatalogMissing)
    );

    let mut invalid = usage(10, 1);
    invalid.cache_hit_input_tokens = Some(9);
    invalid.cache_miss_input_tokens = Some(9);
    assert_eq!(
        priced_ledger
            .record(response(
                UsagePurpose::Chat,
                "deepseek-v4-flash",
                Some(invalid),
            ))
            .unpriced_reason,
        Some(UnpricedReason::InvalidUsage)
    );
}

#[test]
fn summary_aggregates_chat_compaction_tokens_and_cost() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(usage(10, 1)),
    ));
    ledger.record(response(
        UsagePurpose::Compaction,
        "deepseek-v4-flash",
        Some(usage(20, 2)),
    ));

    let summary = ledger.summary();
    assert_eq!(summary.responses, 2);
    assert_eq!(summary.chat_responses, 1);
    assert_eq!(summary.compaction_responses, 1);
    assert_eq!(summary.responses_with_usage, 2);
    assert_eq!(summary.priced_responses, 2);
    assert_eq!(summary.unpriced_responses, 0);
    assert_eq!(summary.input_tokens, 30);
    assert_eq!(summary.output_tokens, 3);
    assert_eq!(summary.total_tokens, 33);
    assert_eq!(summary.cache_miss_input_tokens, 30);
    assert_eq!(summary.pricing_status, PricingStatus::Priced);
    assert_eq!(
        summary.total_cost.as_ref().unwrap().femto_units,
        8_580_000_000
    );
    assert_eq!(ledger.entries().len(), 2);
}

#[test]
fn merged_summary_preserves_partial_pricing_and_exact_token_totals() {
    let mut priced = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    priced.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(usage(10, 2)),
    ));
    let mut unpriced = UsageLedger::new(None);
    unpriced.record(response(
        UsagePurpose::Chat,
        "local-model",
        Some(usage(4, 1)),
    ));

    let mut merged = priced.summary().clone();
    merged.merge(unpriced.summary());

    assert_eq!(merged.responses, 2);
    assert_eq!(merged.total_tokens, 17);
    assert_eq!(merged.priced_responses, 1);
    assert_eq!(merged.unpriced_responses, 1);
    assert_eq!(merged.pricing_status, PricingStatus::Partial);
    assert!(merged.total_cost.is_some());
}

#[test]
fn ledger_round_trips_and_can_rebuild_summary_from_entries() {
    let mut ledger = UsageLedger::new(Some(PriceCatalog::builtin_deepseek_usd()));
    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(usage(10, 1)),
    ));

    let json = serde_json::to_string(&ledger).unwrap();
    let decoded: UsageLedger = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, ledger);

    let rebuilt = UsageLedger::from_entries(None, ledger.entries().to_vec());
    assert_eq!(rebuilt.summary(), ledger.summary());
}

#[test]
fn aggregate_overflow_never_interrupts_recording() {
    let mut catalog = PriceCatalog::builtin_deepseek_usd();
    catalog.cards[0].bands[0].rates = PriceRates {
        cache_hit_input_nanos_per_million: u64::MAX,
        cache_miss_input_nanos_per_million: 0,
        output_nanos_per_million: 0,
    };
    let mut huge = usage(u64::MAX, 0);
    huge.cache_hit_input_tokens = Some(u64::MAX);
    huge.cache_miss_input_tokens = Some(0);
    let mut ledger = UsageLedger::new(Some(catalog));

    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(huge.clone()),
    ));
    ledger.record(response(
        UsagePurpose::Chat,
        "deepseek-v4-flash",
        Some(huge),
    ));

    assert_eq!(ledger.summary().priced_responses, 2);
    assert_eq!(ledger.summary().total_cost, None);
    assert!(ledger.summary().total_cost_unavailable);
}

#[test]
fn compact_cost_formatter_uses_three_decimals_without_hiding_nonzero_cost() {
    assert_eq!(format_femto_amount_3dp(0), "0.000");
    assert_eq!(format_femto_amount_3dp(1), "<0.001");
    assert_eq!(format_femto_amount_3dp(FEMTO_UNITS_PER_MILLI - 1), "<0.001");
    assert_eq!(format_femto_amount_3dp(1_234_400_000_000_000), "1.234");
    assert_eq!(format_femto_amount_3dp(1_234_500_000_000_000), "1.235");
}

#[test]
fn caller_supplied_catalog_json_is_validated_at_setup() {
    let ledger = UsageLedger::new(Some(
        PriceCatalog::from_json(DEEPSEEK_CATALOG_JSON).unwrap(),
    ));
    assert!(ledger.entries().is_empty());

    let error = PriceCatalog::from_json("{\"schema\":\"unknown\"}").unwrap_err();
    assert!(error.to_string().contains("invalid price catalog JSON"));
}

#[test]
fn catalog_rejects_ambiguous_schedules_and_card_periods() {
    let mut schedule_overlap = PriceCatalog::builtin_deepseek_usd();
    schedule_overlap.cards[0].schedule[1].start_minute_utc = 200;
    assert!(
        schedule_overlap
            .validate()
            .unwrap_err()
            .to_string()
            .contains("schedules 0 and 1 overlap")
    );

    let mut missing_default = PriceCatalog::builtin_deepseek_usd();
    missing_default.cards[0].default_band = "missing".to_string();
    assert!(
        missing_default
            .validate()
            .unwrap_err()
            .to_string()
            .contains("default_band `missing` does not exist")
    );

    let mut card_overlap = PriceCatalog::builtin_deepseek_usd();
    let mut duplicate_period = card_overlap.cards[0].clone();
    duplicate_period.id = "another-flash-card".to_string();
    card_overlap.cards.push(duplicate_period);
    assert!(
        card_overlap
            .validate()
            .unwrap_err()
            .to_string()
            .contains("overlap for the same model")
    );
}

#[test]
fn catalog_requires_orderable_version_and_effective_ranges() {
    let mut catalog = PriceCatalog::builtin_deepseek_usd();
    catalog.version = "latest".to_string();
    assert!(
        catalog
            .validate()
            .unwrap_err()
            .to_string()
            .contains("numeric")
    );

    let mut catalog = PriceCatalog::builtin_deepseek_usd();
    catalog.cards[0].effective_until = Some(catalog.cards[0].effective_from);
    assert!(
        catalog
            .validate()
            .unwrap_err()
            .to_string()
            .contains("effective_until")
    );
}

fn band<'a>(card: &'a PriceCard, id: &str) -> &'a PriceBand {
    card.bands
        .iter()
        .find(|band| band.id == id)
        .expect("price band")
}

fn quoted_band(catalog: &PriceCatalog, observed_at: u64) -> String {
    let mut record = response(UsagePurpose::Chat, "deepseek-v4-flash", Some(usage(1, 1)));
    record.observed_at_unix_seconds = observed_at;
    catalog.quote(&record).unwrap().price_ref.band_id
}
