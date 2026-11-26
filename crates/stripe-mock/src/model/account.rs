//! Connected accounts, built on the generated [`stripe::Account`] (the v1 truth state) and
//! [`stripe::V2CoreAccount`] (its v2 view); spec section 5.4.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat};
use serde_json::Value;

use crate::rng::SimRng;
use crate::stripe;

/// Which Accounts API created an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountApi {
    V1,
    V2,
}

/// The account's `type` in `/v1/accounts`. `V2` accounts show `type: "none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Standard,
    Express,
    Custom,
    V2,
}

/// An Accounts v2 configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Configuration {
    Customer,
    Merchant,
    Recipient,
}

impl Configuration {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Configuration::Customer => "customer",
            Configuration::Merchant => "merchant",
            Configuration::Recipient => "recipient",
        }
    }
}

impl Configuration {
    fn applied(self) -> stripe::V2CoreAccountAppliedConfigurations {
        match self {
            Configuration::Customer => stripe::V2CoreAccountAppliedConfigurations::Customer,
            Configuration::Merchant => stripe::V2CoreAccountAppliedConfigurations::Merchant,
            Configuration::Recipient => stripe::V2CoreAccountAppliedConfigurations::Recipient,
        }
    }
}

impl AccountKind {
    fn account_type(self) -> stripe::AccountType {
        match self {
            AccountKind::Standard => stripe::AccountType::Standard,
            AccountKind::Express => stripe::AccountType::Express,
            AccountKind::Custom => stripe::AccountType::Custom,
            AccountKind::V2 => stripe::AccountType::None,
        }
    }

    fn dashboard(self) -> stripe::V2CoreAccountDashboard {
        match self {
            AccountKind::Standard | AccountKind::V2 => stripe::V2CoreAccountDashboard::Full,
            AccountKind::Express => stripe::V2CoreAccountDashboard::Express,
            AccountKind::Custom => stripe::V2CoreAccountDashboard::None,
        }
    }

    /// Who controls the account. A Standard account controls itself; the platform controls
    /// the others, with the fee, loss, requirement and dashboard split of each kind.
    fn controller(self) -> stripe::AccountUnificationAccountController {
        use stripe::AccountUnificationAccountControllerFeesPayer as Payer;
        use stripe::AccountUnificationAccountControllerLossesPayments as Losses;
        use stripe::AccountUnificationAccountControllerRequirementCollection as Collection;
        use stripe::AccountUnificationAccountControllerStripeDashboardType as Dashboard;
        let platform =
            |payer, payments, collection, dashboard| stripe::AccountUnificationAccountController {
                fees: Some(stripe::AccountUnificationAccountControllerFees { payer }),
                is_controller: Some(true),
                losses: Some(stripe::AccountUnificationAccountControllerLosses { payments }),
                requirement_collection: Some(collection),
                stripe_dashboard: Some(
                    stripe::AccountUnificationAccountControllerStripeDashboard { type_: dashboard },
                ),
                type_: stripe::AccountUnificationAccountControllerType::Application,
            };
        match self {
            // An account that controls itself shows only its controller type and
            // `is_controller` to the platform.
            AccountKind::Standard => stripe::AccountUnificationAccountController {
                fees: None,
                is_controller: Some(false),
                losses: None,
                requirement_collection: None,
                stripe_dashboard: None,
                type_: stripe::AccountUnificationAccountControllerType::Account,
            },
            AccountKind::Express => platform(
                Payer::ApplicationExpress,
                Losses::Application,
                Collection::Stripe,
                Dashboard::Express,
            ),
            AccountKind::Custom => platform(
                Payer::ApplicationCustom,
                Losses::Application,
                Collection::Application,
                Dashboard::None,
            ),
            AccountKind::V2 => platform(
                Payer::Account,
                Losses::Stripe,
                Collection::Stripe,
                Dashboard::Full,
            ),
        }
    }
}

/// What the mock remembers about a connected account besides its truth state.
#[derive(Debug, Clone)]
pub(crate) struct AccountMeta {
    pub id: String,
    pub api: AccountApi,
    pub kind: AccountKind,
    pub configurations: Vec<Configuration>,
    pub connected: bool,
}

/// The fields churn changes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccountContent {
    pub name: String,
    pub email: String,
    pub metadata: BTreeMap<String, String>,
}

const NAMES: [&str; 5] = [
    "Acme Goods",
    "Bluebird Studio",
    "Café Önder",
    "Delta & Sons",
    "山田 商店",
];
const METADATA_KEYS: [&str; 3] = ["region", "tier", "note"];

/// Random fields for a new or updated account.
pub(crate) fn random_content(rng: &mut SimRng) -> AccountContent {
    let name = rng.choose(&NAMES).to_string();
    let email = format!("owner{}@example.com", rng.below(1_000_000));
    let mut metadata = BTreeMap::new();
    for _ in 0..rng.below(3) {
        let key = rng.choose(&METADATA_KEYS).to_string();
        metadata.insert(key, format!("v{}", rng.below(100)));
    }
    AccountContent {
        name,
        email,
        metadata,
    }
}

/// Empty requirements: the mock's accounts never owe Stripe anything.
fn no_requirements() -> stripe::AccountRequirements {
    stripe::AccountRequirements {
        alternatives: Some(Some(Vec::new())),
        current_deadline: Some(None),
        currently_due: Some(Some(Vec::new())),
        disabled_reason: Some(None),
        errors: Some(Some(Vec::new())),
        eventually_due: Some(Some(Vec::new())),
        past_due: Some(Some(Vec::new())),
        pending_verification: Some(Some(Vec::new())),
    }
}

fn no_future_requirements() -> stripe::AccountFutureRequirements {
    stripe::AccountFutureRequirements {
        alternatives: Some(Some(Vec::new())),
        current_deadline: Some(None),
        currently_due: Some(Some(Vec::new())),
        disabled_reason: Some(None),
        errors: Some(Some(Vec::new())),
        eventually_due: Some(Some(Vec::new())),
        past_due: Some(Some(Vec::new())),
        pending_verification: Some(Some(Vec::new())),
    }
}

/// `card_payments` and `transfers`, both active: the capabilities every mock account has.
/// Stripe lists only the capabilities an account requested, so the rest are absent.
fn capabilities() -> stripe::AccountCapabilities {
    stripe::AccountCapabilities {
        card_payments: Some(stripe::AccountCapabilitiesCardPayments::Active),
        transfers: Some(stripe::AccountCapabilitiesTransfers::Active),
        acss_debit_payments: None,
        affirm_payments: None,
        afterpay_clearpay_payments: None,
        alma_payments: None,
        amazon_pay_payments: None,
        app_distribution: None,
        au_becs_debit_payments: None,
        bacs_debit_payments: None,
        bancontact_payments: None,
        bank_transfer_payments: None,
        billie_payments: None,
        bizum_payments: None,
        blik_payments: None,
        blik_recurring_payments: None,
        boleto_payments: None,
        card_issuing: None,
        cartes_bancaires_payments: None,
        cashapp_payments: None,
        crypto_payments: None,
        eps_payments: None,
        fpx_payments: None,
        gb_bank_transfer_payments: None,
        giropay_payments: None,
        grabpay_payments: None,
        ideal_payments: None,
        india_international_payments: None,
        jcb_payments: None,
        jp_bank_transfer_payments: None,
        kakao_pay_payments: None,
        klarna_payments: None,
        konbini_payments: None,
        kr_card_payments: None,
        legacy_payments: None,
        link_payments: None,
        mb_way_payments: None,
        mobilepay_payments: None,
        multibanco_payments: None,
        mx_bank_transfer_payments: None,
        naver_pay_payments: None,
        nz_bank_account_becs_debit_payments: None,
        oxxo_payments: None,
        p24_payments: None,
        pay_by_bank_payments: None,
        payco_payments: None,
        paynow_payments: None,
        paypay_payments: None,
        payto_payments: None,
        pix_payments: None,
        promptpay_payments: None,
        revolut_pay_payments: None,
        samsung_pay_payments: None,
        satispay_payments: None,
        scalapay_payments: None,
        sepa_bank_transfer_payments: None,
        sepa_debit_payments: None,
        sequra_payments: None,
        sofort_payments: None,
        sunbit_payments: None,
        swish_payments: None,
        tax_reporting_us_1099_k: None,
        tax_reporting_us_1099_misc: None,
        treasury: None,
        twint_payments: None,
        upi_payments: None,
        us_bank_account_ach_payments: None,
        us_bank_transfer_payments: None,
        wero_payments: None,
        zip_payments: None,
    }
}

fn business_profile(name: &str) -> stripe::AccountBusinessProfile {
    stripe::AccountBusinessProfile {
        annual_revenue: Some(None),
        estimated_worker_count: Some(None),
        mcc: Some(None),
        minority_owned_business_designation: Some(None),
        monthly_estimated_revenue: Some(stripe::AccountMonthlyEstimatedRevenue {
            amount: 0,
            currency: "usd".to_string(),
        }),
        name: Some(Some(name.to_string())),
        product_description: Some(None),
        specified_commercial_transactions_act_url: Some(None),
        support_address: Some(None),
        support_email: Some(None),
        support_phone: Some(None),
        support_url: Some(None),
        url: Some(None),
    }
}

/// The settings Stripe always returns (`branding`, `card_payments`, `dashboard`, `payments`),
/// all unset. Per-method, invoice, payout and treasury settings are absent: the mock models
/// none of them.
fn settings() -> stripe::AccountSettings {
    stripe::AccountSettings {
        bacs_debit_payments: None,
        branding: stripe::AccountBrandingSettings {
            icon: Some(None),
            logo: Some(None),
            primary_color: Some(None),
            secondary_color: Some(None),
        },
        card_issuing: None,
        card_payments: stripe::AccountCardPaymentsSettings {
            decline_on: Some(stripe::AccountDeclineChargeOn {
                avs_failure: false,
                cvc_failure: false,
            }),
            statement_descriptor_prefix: Some(None),
            statement_descriptor_prefix_kana: Some(None),
            statement_descriptor_prefix_kanji: Some(None),
        },
        dashboard: stripe::AccountDashboardSettings {
            display_name: Some(None),
            timezone: Some(Some("Etc/UTC".to_string())),
        },
        invoices: None,
        payments: stripe::AccountPaymentsSettings {
            statement_descriptor: Some(None),
            statement_descriptor_kana: Some(None),
            statement_descriptor_kanji: Some(None),
        },
        payouts: None,
        paypay_payments: None,
        sepa_debit_payments: None,
        treasury: None,
    }
}

/// The account's truth state: the v1 object. Presence follows ruling R1: every field is
/// present (`null` when unset) except `company` and `individual`, which Stripe returns only
/// for a matching `business_type` (always `null` here).
pub(crate) fn v1_account(
    id: &str,
    kind: AccountKind,
    created: i64,
    c: &AccountContent,
) -> stripe::Account {
    stripe::Account {
        business_profile: Some(Some(business_profile(&c.name))),
        business_type: Some(None),
        capabilities: Some(capabilities()),
        charges_enabled: Some(true),
        company: None,
        controller: Some(kind.controller()),
        country: Some("US".to_string()),
        created: Some(created),
        default_currency: Some("usd".to_string()),
        details_submitted: Some(true),
        email: Some(Some(c.email.clone())),
        external_accounts: Some(stripe::List {
            object: stripe::ListObject::List,
            data: Vec::new(),
            has_more: false,
            url: format!("/v1/accounts/{id}/external_accounts"),
            total_count: None,
        }),
        future_requirements: Some(no_future_requirements()),
        groups: Some(None),
        id: id.to_string(),
        individual: None,
        metadata: Some(c.metadata.clone()),
        object: stripe::AccountObject::Account,
        payouts_enabled: Some(true),
        requirements: Some(no_requirements()),
        settings: Some(Some(settings())),
        tos_acceptance: Some(stripe::AccountTosAcceptance {
            date: Some(Some(created)),
            ip: Some(None),
            service_agreement: Some("full".to_string()),
            user_agent: Some(None),
        }),
        type_: Some(kind.account_type()),
    }
}

/// Replaces the fields churn changes.
pub(crate) fn apply_content(a: &mut stripe::Account, c: &AccountContent) {
    match &mut a.business_profile {
        Some(Some(p)) => p.name = Some(Some(c.name.clone())),
        _ => a.business_profile = Some(Some(business_profile(&c.name))),
    }
    a.email = Some(Some(c.email.clone()));
    a.metadata = Some(c.metadata.clone());
}

/// What Stripe returns for a deleted account: the tombstone of a disconnect.
pub(crate) fn deleted_account(id: &str) -> stripe::DeletedAccount {
    stripe::DeletedAccount {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedAccountObject::Account,
    }
}

/// A stored account state as the typed object. A state that does not deserialize is a bug
/// in the mock, so it panics with the path.
pub(crate) fn from_state(state: &Value) -> stripe::Account {
    serde_path_to_error::deserialize(state).unwrap_or_else(|e| {
        panic!(
            "stored account is not a valid account at {}: {}",
            e.path(),
            e.inner()
        )
    })
}

/// The platform's Connect application, the `data.object` of `account.application.*` events.
pub(crate) fn platform_application(id: String) -> stripe::Application {
    stripe::Application {
        id,
        name: Some(Some("Ductkit Mock Platform".to_string())),
        object: stripe::ApplicationObject::Application,
    }
}

/// The `/v2/core/accounts` view of an account's v1 state. `configuration`, `defaults`,
/// `identity`, `requirements` and `future_requirements` are includable-only (the v2
/// `include` parameter) and the mock serves no includes, so they are absent (ruling R1).
/// `contact_phone` is absent because the mock's accounts have no phone.
pub(crate) fn v2_view(
    a: &stripe::Account,
    meta: &AccountMeta,
    closed: bool,
) -> stripe::V2CoreAccount {
    let mut applied: Vec<Configuration> = meta.configurations.clone();
    applied.sort();
    let created = a
        .created
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true))
        .expect("an account has a valid `created`");
    let display_name = match &a.business_profile {
        Some(Some(p)) => p.name.clone().flatten(),
        _ => None,
    };
    stripe::V2CoreAccount {
        applied_configurations: applied.iter().map(|c| c.applied()).collect(),
        closed: Some(closed),
        configuration: None,
        contact_email: a.email.clone().flatten(),
        contact_phone: None,
        created,
        dashboard: Some(meta.kind.dashboard()),
        defaults: None,
        display_name,
        future_requirements: None,
        id: a.id.clone(),
        identity: None,
        livemode: false,
        metadata: Some(a.metadata.clone().unwrap_or_default()),
        object: stripe::V2CoreAccountObject::V2CoreAccount,
        requirements: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn content() -> AccountContent {
        AccountContent {
            name: "Acme".into(),
            email: "a@example.com".into(),
            metadata: BTreeMap::from([("k".to_string(), "v".to_string())]),
        }
    }

    fn v1(kind: AccountKind) -> Value {
        serde_json::to_value(v1_account("acct_1", kind, 100, &content())).unwrap()
    }

    #[test]
    fn v1_shape() {
        let s = v1(AccountKind::Express);
        assert_eq!(s["type"], "express");
        assert_eq!(s["business_profile"]["name"], "Acme");
        assert_eq!(s["email"], "a@example.com");
        assert_eq!(s["business_type"], Value::Null);
        assert_eq!(s["controller"]["type"], "application");
        assert_eq!(s["controller"]["stripe_dashboard"]["type"], "express");
        assert_eq!(
            s["capabilities"],
            json!({"card_payments": "active", "transfers": "active"})
        );
        assert_eq!(
            s["external_accounts"]["url"],
            "/v1/accounts/acct_1/external_accounts"
        );
        for absent in ["company", "individual"] {
            assert!(s.get(absent).is_none(), "{absent}");
        }
        assert_eq!(
            v1(AccountKind::Standard)["controller"],
            json!({"is_controller": false, "type": "account"})
        );
        assert_eq!(v1(AccountKind::Custom)["controller"]["is_controller"], true);
        let v = v1(AccountKind::V2);
        assert_eq!(
            (v["type"].as_str(), v["controller"]["type"].as_str()),
            (Some("none"), Some("application"))
        );
    }

    #[test]
    fn apply_content_round_trips_through_the_stored_state() {
        let mut a = from_state(&v1(AccountKind::Custom));
        let next = AccountContent {
            name: "Bluebird".into(),
            email: "b@example.com".into(),
            metadata: BTreeMap::new(),
        };
        apply_content(&mut a, &next);
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["business_profile"]["name"], "Bluebird");
        assert_eq!(v["business_profile"]["url"], Value::Null);
        assert_eq!(v["email"], "b@example.com");
        assert_eq!(v["metadata"], json!({}));
    }

    #[test]
    fn v2_shape() {
        let a = v1_account("acct_1", AccountKind::V2, 1_767_225_600, &content());
        let meta = AccountMeta {
            id: "acct_1".into(),
            api: AccountApi::V2,
            kind: AccountKind::V2,
            configurations: vec![Configuration::Recipient, Configuration::Merchant],
            connected: true,
        };
        let v = serde_json::to_value(v2_view(&a, &meta, false)).unwrap();
        assert_eq!(v["object"], "v2.core.account");
        assert_eq!(
            v["applied_configurations"],
            json!(["merchant", "recipient"])
        );
        for absent in [
            "configuration",
            "defaults",
            "future_requirements",
            "identity",
            "requirements",
            "contact_phone",
        ] {
            assert!(v.get(absent).is_none(), "{absent}");
        }
        assert_eq!(v["created"], "2026-01-01T00:00:00.000Z");
        assert_eq!(v["dashboard"], "full");
        assert_eq!(v["display_name"], "Acme");
        assert_eq!(v["contact_email"], "a@example.com");
        assert_eq!(v["closed"], false);
        assert_eq!(v2_view(&a, &meta, true).closed, Some(true));
    }
}
