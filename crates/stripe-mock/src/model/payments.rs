//! Constructors for the payment objects (payment intents, charges, refunds, disputes,
//! balance transactions, payment methods, setup intents, checkout sessions and invoice
//! payments), built on the generated types in `crate::stripe`.
//!
//! Presence follows ruling R1: every field is present (`null`, empty or a value) unless
//! Stripe documents it as includable-only or expandable-only, or it is optional and not
//! nullable and only appears in a situation the mock does not model (a payment method's
//! other-type details, `presentment_details`, `radar_options`, `hooks`, the card
//! enterprise features, `charge.transfer`). Those are absent.

use std::collections::BTreeMap;

use crate::stripe::{self, Expandable};

/// Seconds from a charge to its balance becoming available.
pub(crate) const AVAILABLE_AFTER_SECS: i64 = 2 * 86_400;

/// The test card the mock pays with when a customer has no saved card: Visa, `4242`.
pub(crate) const BRAND: &str = "visa";
pub(crate) const LAST4: &str = "4242";
const CARD_COUNTRY: &str = "US";
const FUNDING: &str = "credit";

pub(crate) const DECLINE_CODE: &str = "card_declined";
const DECLINE_MESSAGE: &str = "Your card was declined.";

/// Stripe's fee for a card charge: 2.9% + 30 in the minor unit, rounded half up; 0 for a
/// zero amount.
pub(crate) fn card_fee(amount: i64) -> i64 {
    if amount <= 0 {
        0
    } else {
        (amount * 29 + 500) / 1000 + 30
    }
}

/// The drawn parts of a card.
#[derive(Debug, Clone)]
pub(crate) struct CardDraw {
    /// `visa`, `mastercard` or `amex`.
    pub brand: String,
    pub last4: String,
    pub exp_month: i64,
    pub exp_year: i64,
    pub fingerprint: String,
}

fn empty_billing_details() -> stripe::BillingDetails {
    stripe::BillingDetails {
        address: Some(Some(stripe::Address {
            city: Some(None),
            country: Some(None),
            line1: Some(None),
            line2: Some(None),
            postal_code: Some(None),
            state: Some(None),
        })),
        email: Some(None),
        name: Some(None),
        phone: Some(None),
        tax_id: Some(None),
    }
}

/// The `display_brand` Stripe shows for a card brand.
fn display_brand(brand: &str) -> &str {
    match brand {
        "amex" => "american_express",
        other => other,
    }
}

/// A card payment method, attached to `customer` when given.
pub(crate) fn new_card_payment_method(
    id: &str,
    created: i64,
    customer: Option<&str>,
    card: &CardDraw,
) -> stripe::PaymentMethod {
    stripe::PaymentMethod {
        acss_debit: None,
        affirm: None,
        afterpay_clearpay: None,
        alipay: None,
        allow_redisplay: Some(stripe::PaymentMethodAllowRedisplay::Unspecified),
        alma: None,
        amazon_pay: None,
        au_becs_debit: None,
        bacs_debit: None,
        bancontact: None,
        billie: None,
        billing_details: empty_billing_details(),
        bizum: None,
        blik: None,
        boleto: None,
        card: Some(stripe::PaymentMethodCard {
            brand: card.brand.clone(),
            checks: Some(Some(stripe::PaymentMethodCardChecks {
                address_line1_check: Some(None),
                address_postal_code_check: Some(None),
                cvc_check: Some(None),
            })),
            country: Some(Some(CARD_COUNTRY.to_string())),
            display_brand: Some(Some(display_brand(&card.brand).to_string())),
            exp_month: card.exp_month,
            exp_year: card.exp_year,
            fingerprint: Some(Some(card.fingerprint.clone())),
            funding: FUNDING.to_string(),
            generated_from: Some(None),
            last4: card.last4.clone(),
            networks: Some(Some(stripe::Networks {
                available: vec![card.brand.clone()],
                preferred: Some(None),
            })),
            regulated_status: Some(Some(stripe::PaymentMethodCardRegulatedStatus::Unregulated)),
            three_d_secure_usage: Some(Some(stripe::ThreeDSecureUsage { supported: true })),
            wallet: Some(None),
        }),
        card_present: None,
        cashapp: None,
        created,
        crypto: None,
        custom: None,
        customer: Some(customer.map(|c| Expandable::Id(c.to_string()))),
        customer_account: Some(None),
        customer_balance: None,
        eps: None,
        fpx: None,
        giropay: None,
        grabpay: None,
        id: id.to_string(),
        ideal: None,
        interac_present: None,
        kakao_pay: None,
        klarna: None,
        konbini: None,
        kr_card: None,
        link: None,
        livemode: false,
        mb_way: None,
        metadata: Some(Some(BTreeMap::new())),
        mobilepay: None,
        multibanco: None,
        naver_pay: None,
        nz_bank_account: None,
        object: stripe::PaymentMethodObject::PaymentMethod,
        oxxo: None,
        p24: None,
        pay_by_bank: None,
        payco: None,
        paynow: None,
        paypal: None,
        paypay: None,
        payto: None,
        pix: None,
        promptpay: None,
        radar_options: None,
        revolut_pay: None,
        samsung_pay: None,
        satispay: None,
        scalapay: None,
        sepa_debit: None,
        sequra: None,
        sofort: None,
        sunbit: None,
        swish: None,
        twint: None,
        type_: stripe::PaymentMethodType::Card,
        upi: None,
        us_bank_account: None,
        wechat_pay: None,
        zip: None,
    }
}

/// Inputs for a payment intent.
pub(crate) struct NewPaymentIntent<'a> {
    pub id: &'a str,
    pub created: i64,
    pub amount: i64,
    pub currency: &'a str,
    pub customer: Option<&'a str>,
    pub payment_method: &'a str,
    pub client_secret: &'a str,
    /// The fee a connected account's direct charge pays the platform.
    pub application_fee_amount: Option<i64>,
}

fn card_options() -> stripe::PaymentIntentPaymentMethodOptions {
    stripe::PaymentIntentPaymentMethodOptions {
        acss_debit: None,
        affirm: None,
        afterpay_clearpay: None,
        alipay: None,
        alma: None,
        amazon_pay: None,
        au_becs_debit: None,
        bacs_debit: None,
        bancontact: None,
        billie: None,
        bizum: None,
        blik: None,
        boleto: None,
        card: Some(
            stripe::PaymentIntentPaymentMethodOptionsCardUnion::PaymentIntentPaymentMethodOptionsCard(
                stripe::PaymentIntentPaymentMethodOptionsCard {
                    capture_method: None,
                    installments: Some(None),
                    mandate_options: Some(None),
                    network: Some(None),
                    request_extended_authorization: None,
                    request_incremental_authorization: None,
                    request_multicapture: None,
                    request_overcapture: None,
                    request_three_d_secure: Some(Some(
                        stripe::PaymentIntentPaymentMethodOptionsCardRequestThreeDSecure::Automatic,
                    )),
                    require_cvc_recollection: None,
                    setup_credential_usage: None,
                    setup_future_usage: None,
                    statement_descriptor_suffix_kana: None,
                    statement_descriptor_suffix_kanji: None,
                    stored_credential_usage: None,
                },
            ),
        ),
        card_present: None,
        cashapp: None,
        crypto: None,
        customer_balance: None,
        eps: None,
        fpx: None,
        giropay: None,
        grabpay: None,
        ideal: None,
        interac_present: None,
        kakao_pay: None,
        klarna: None,
        konbini: None,
        kr_card: None,
        link: None,
        mb_way: None,
        mobilepay: None,
        multibanco: None,
        naver_pay: None,
        nz_bank_account: None,
        oxxo: None,
        p24: None,
        pay_by_bank: None,
        payco: None,
        paynow: None,
        paypal: None,
        paypay: None,
        payto: None,
        pix: None,
        promptpay: None,
        revolut_pay: None,
        samsung_pay: None,
        satispay: None,
        scalapay: None,
        sepa_debit: None,
        sequra: None,
        sofort: None,
        sunbit: None,
        swish: None,
        twint: None,
        upi: None,
        us_bank_account: None,
        wechat_pay: None,
        zip: None,
    }
}

/// A new payment intent, `requires_confirmation`, with its payment method set.
pub(crate) fn new_payment_intent(p: &NewPaymentIntent) -> stripe::PaymentIntent {
    stripe::PaymentIntent {
        allowed_payment_method_types: Some(None),
        amount: Some(p.amount),
        amount_capturable: Some(0),
        amount_details: Some(
            stripe::PaymentIntentAmountDetailsUnion::PaymentFlowsAmountDetails(
                stripe::PaymentFlowsAmountDetails {
                    discount_amount: None,
                    error: None,
                    line_items: None,
                    shipping: None,
                    tax: None,
                    tip: Some(stripe::PaymentFlowsAmountDetailsClientResourceTip { amount: None }),
                },
            ),
        ),
        amount_received: Some(0),
        application: Some(None),
        application_fee_amount: Some(p.application_fee_amount),
        automatic_payment_methods: Some(None),
        canceled_at: Some(None),
        cancellation_reason: Some(None),
        capture_method: Some(stripe::PaymentIntentCaptureMethod::Automatic),
        client_secret: Some(Some(p.client_secret.to_string())),
        confirmation_method: Some(stripe::PaymentIntentConfirmationMethod::Automatic),
        created: p.created,
        currency: Some(p.currency.to_string()),
        customer: Some(p.customer.map(|c| Expandable::Id(c.to_string()))),
        customer_account: Some(None),
        description: Some(None),
        excluded_payment_method_types: Some(None),
        hooks: None,
        id: p.id.to_string(),
        last_payment_error: Some(None),
        latest_charge: Some(None),
        livemode: false,
        managed_payments: Some(None),
        metadata: Some(BTreeMap::new()),
        next_action: Some(None),
        object: stripe::PaymentIntentObject::PaymentIntent,
        on_behalf_of: Some(None),
        payment_details: None,
        payment_method: Some(Some(Expandable::Id(p.payment_method.to_string()))),
        payment_method_configuration_details: Some(None),
        payment_method_options: Some(Some(card_options())),
        payment_method_types: Some(vec!["card".to_string()]),
        payment_record: Some(None),
        presentment_details: None,
        processing: Some(None),
        receipt_email: Some(None),
        review: Some(None),
        setup_future_usage: Some(None),
        shipping: Some(None),
        statement_descriptor: Some(None),
        statement_descriptor_suffix: Some(None),
        status: stripe::PaymentIntentStatus::RequiresConfirmation,
        transfer_data: Some(None),
        transfer_group: Some(None),
    }
}

/// The `last_payment_error` of a declined card.
pub(crate) fn decline_error(charge: &str, pm: &stripe::PaymentMethod) -> stripe::ApiErrors {
    stripe::ApiErrors {
        advice_code: None,
        charge: Some(charge.to_string()),
        code: Some(DECLINE_CODE.to_string()),
        decline_code: Some("generic_decline".to_string()),
        doc_url: Some("https://docs.stripe.com/error-codes/card-declined".to_string()),
        message: Some(DECLINE_MESSAGE.to_string()),
        network_advice_code: None,
        network_decline_code: None,
        param: None,
        payment_intent: None,
        payment_method: Some(pm.clone()),
        payment_method_type: None,
        request_log_url: None,
        setup_intent: None,
        source: None,
        type_: stripe::ApiErrorsType::CardError,
    }
}

/// Inputs for a charge.
pub(crate) struct NewCharge<'a> {
    pub id: &'a str,
    pub created: i64,
    pub amount: i64,
    pub currency: &'a str,
    pub customer: Option<&'a str>,
    pub payment_intent: &'a str,
    pub payment_method: &'a stripe::PaymentMethod,
    pub succeeded: bool,
    /// The charge's balance transaction (succeeded charges only).
    pub balance_transaction: Option<&'a str>,
    pub risk_score: i64,
    pub network_transaction_id: &'a str,
    /// The platform's application fee on a succeeded direct charge: its ID and amount.
    pub application_fee: Option<(&'a str, i64)>,
}

fn outcome(succeeded: bool, risk_score: i64) -> stripe::ChargeOutcome {
    let (network_status, reason, seller_message, type_) = if succeeded {
        (
            "approved_by_network",
            None,
            "Payment complete.",
            "authorized",
        )
    } else {
        (
            "declined_by_network",
            Some("generic_decline".to_string()),
            "The bank did not return any further details with this decline.",
            "issuer_declined",
        )
    };
    stripe::ChargeOutcome {
        advice_code: Some(None),
        network_advice_code: Some(None),
        network_decline_code: Some(None),
        network_status: Some(Some(network_status.to_string())),
        reason: Some(reason),
        risk_level: Some("normal".to_string()),
        risk_score: Some(risk_score),
        rule: None,
        seller_message: Some(Some(seller_message.to_string())),
        type_: type_.to_string(),
    }
}

fn card_details(c: &NewCharge) -> stripe::PaymentMethodDetails {
    let pm_card = c
        .payment_method
        .card
        .as_ref()
        .expect("the mock pays with cards");
    stripe::PaymentMethodDetails {
        ach_credit_transfer: None,
        ach_debit: None,
        acss_debit: None,
        affirm: None,
        afterpay_clearpay: None,
        alipay: None,
        alma: None,
        amazon_pay: None,
        au_becs_debit: None,
        bacs_debit: None,
        bancontact: None,
        billie: None,
        bizum: None,
        blik: None,
        boleto: None,
        card: Some(stripe::PaymentMethodDetailsCard {
            amount_authorized: Some(c.succeeded.then_some(c.amount)),
            authorization_code: Some(None),
            brand: Some(Some(pm_card.brand.clone())),
            capture_before: None,
            checks: Some(Some(stripe::PaymentMethodDetailsCardChecks {
                address_line1_check: Some(None),
                address_postal_code_check: Some(None),
                cvc_check: Some(None),
            })),
            country: Some(pm_card.country.clone().flatten()),
            electronic_commerce_indicator: Some(None),
            exp_month: pm_card.exp_month,
            exp_year: pm_card.exp_year,
            extended_authorization: None,
            fingerprint: Some(pm_card.fingerprint.clone().flatten()),
            funding: Some(Some(pm_card.funding.clone())),
            incremental_authorization: None,
            installments: Some(None),
            last4: Some(Some(pm_card.last4.clone())),
            mandate: Some(None),
            multicapture: None,
            network: Some(Some(pm_card.brand.clone())),
            network_token: Some(Some(stripe::PaymentMethodDetailsCardNetworkToken {
                used: false,
            })),
            network_transaction_id: Some(Some(c.network_transaction_id.to_string())),
            overcapture: None,
            regulated_status: Some(Some(
                stripe::PaymentMethodDetailsCardRegulatedStatus::Unregulated,
            )),
            setup_credential_usage: None,
            stored_credential_usage: None,
            three_d_secure: Some(None),
            transaction_link_id: Some(None),
            wallet: Some(None),
        }),
        card_present: None,
        cashapp: None,
        crypto: None,
        customer_balance: None,
        eps: None,
        fpx: None,
        giropay: None,
        grabpay: None,
        ideal: None,
        interac_present: None,
        kakao_pay: None,
        klarna: None,
        konbini: None,
        kr_card: None,
        link: None,
        mb_way: None,
        mobilepay: None,
        multibanco: None,
        naver_pay: None,
        nz_bank_account: None,
        oxxo: None,
        p24: None,
        pay_by_bank: None,
        payco: None,
        paynow: None,
        paypal: None,
        paypay: None,
        payto: None,
        pix: None,
        promptpay: None,
        revolut_pay: None,
        samsung_pay: None,
        satispay: None,
        scalapay: None,
        sepa_debit: None,
        sequra: None,
        sofort: None,
        stripe_account: None,
        sunbit: None,
        swish: None,
        twint: None,
        type_: "card".to_string(),
        upi: None,
        us_bank_account: None,
        wechat: None,
        wechat_pay: None,
        zip: None,
    }
}

pub(crate) fn refunds_url(charge: &str) -> String {
    format!("/v1/charges/{charge}/refunds")
}

/// A captured `succeeded` charge, or a `failed` one (`card_declined`, no balance
/// transaction). `refunds` is includable, so absent: `expand[]` builds it from the store.
pub(crate) fn new_charge(c: &NewCharge) -> stripe::Charge {
    let (status, failure_code, failure_message) = if c.succeeded {
        (stripe::ChargeStatus::Succeeded, None, None)
    } else {
        (
            stripe::ChargeStatus::Failed,
            Some(DECLINE_CODE.to_string()),
            Some(DECLINE_MESSAGE.to_string()),
        )
    };
    stripe::Charge {
        amount: c.amount,
        amount_captured: if c.succeeded { c.amount } else { 0 },
        amount_refunded: 0,
        application: Some(None),
        application_fee: Some(
            c.application_fee
                .map(|(id, _)| Expandable::Id(id.to_string())),
        ),
        application_fee_amount: Some(c.application_fee.map(|(_, amount)| amount)),
        balance_transaction: Some(c.balance_transaction.map(|t| Expandable::Id(t.to_string()))),
        billing_details: c.payment_method.billing_details.clone(),
        calculated_statement_descriptor: Some(Some("DUCTKIT MOCK".to_string())),
        captured: c.succeeded,
        created: c.created,
        currency: c.currency.to_string(),
        customer: Some(c.customer.map(|id| Expandable::Id(id.to_string()))),
        description: Some(None),
        disputed: false,
        failure_balance_transaction: Some(None),
        failure_code: Some(failure_code),
        failure_message: Some(failure_message),
        fraud_details: Some(Some(stripe::ChargeFraudDetails {
            stripe_report: None,
            user_report: None,
        })),
        id: c.id.to_string(),
        livemode: false,
        metadata: BTreeMap::new(),
        object: stripe::ChargeObject::Charge,
        on_behalf_of: Some(None),
        outcome: Some(Some(outcome(c.succeeded, c.risk_score))),
        paid: c.succeeded,
        payment_intent: Some(Some(Expandable::Id(c.payment_intent.to_string()))),
        payment_method: Some(Some(c.payment_method.id.clone())),
        payment_method_details: Some(Some(card_details(c))),
        presentment_details: None,
        radar_options: None,
        receipt_email: Some(None),
        receipt_number: Some(None),
        receipt_url: Some(None),
        refunded: false,
        refunds: None,
        review: Some(None),
        shipping: Some(None),
        source_transfer: Some(None),
        statement_descriptor: Some(None),
        statement_descriptor_suffix: Some(None),
        status,
        transfer: None,
        transfer_data: Some(None),
        transfer_group: Some(None),
    }
}

/// The `pending` balance transaction of a succeeded charge, available
/// [`AVAILABLE_AFTER_SECS`] after `created`.
pub(crate) fn new_charge_balance_transaction(
    id: &str,
    created: i64,
    amount: i64,
    currency: &str,
    charge: &str,
) -> stripe::BalanceTransaction {
    let fee = card_fee(amount);
    let fee_details = if fee == 0 {
        Vec::new()
    } else {
        vec![stripe::Fee {
            amount: fee,
            application: Some(None),
            currency: currency.to_string(),
            description: Some(Some("Stripe processing fees".to_string())),
            type_: "stripe_fee".to_string(),
        }]
    };
    stripe::BalanceTransaction {
        amount,
        available_on: created + AVAILABLE_AFTER_SECS,
        balance_type: stripe::BalanceTransactionBalanceType::Payments,
        created,
        currency: currency.to_string(),
        description: Some(None),
        exchange_rate: Some(None),
        fee,
        fee_details,
        id: id.to_string(),
        net: amount - fee,
        object: stripe::BalanceTransactionObject::BalanceTransaction,
        reporting_category: "charge".to_string(),
        source: Some(Some(Expandable::Id(charge.to_string()))),
        status: "pending".to_string(),
        type_: stripe::BalanceTransactionType::Charge,
    }
}

/// Inputs for a refund.
pub(crate) struct NewRefund<'a> {
    pub id: &'a str,
    pub created: i64,
    pub amount: i64,
    pub balance_transaction: &'a str,
    /// The refunded charge.
    pub charge: &'a stripe::Charge,
}

/// A `succeeded` card refund of `charge`; its card reference is still `pending`, so it
/// has no `reference` yet.
pub(crate) fn new_refund(r: &NewRefund) -> stripe::Refund {
    let c = r.charge;
    stripe::Refund {
        amount: r.amount,
        application: Some(None),
        balance_transaction: Some(Some(Expandable::Id(r.balance_transaction.to_string()))),
        charge: Some(Some(Expandable::Id(c.id.clone()))),
        created: r.created,
        currency: c.currency.clone(),
        customer: Some(
            c.customer
                .clone()
                .flatten()
                .map(|cu| Expandable::Id(cu.id().to_string())),
        ),
        customer_account: Some(None),
        description: None,
        destination_details: Some(refund_destination_details()),
        failure_balance_transaction: None,
        failure_reason: None,
        id: r.id.to_string(),
        instructions_email: None,
        metadata: Some(Some(BTreeMap::new())),
        next_action: None,
        object: stripe::RefundObject::Refund,
        payment_intent: Some(
            c.payment_intent
                .clone()
                .flatten()
                .map(|p| Expandable::Id(p.id().to_string())),
        ),
        payment_method: Some(c.payment_method.clone().flatten().map(Expandable::Id)),
        pending_reason: None,
        presentment_details: None,
        reason: Some(None),
        receipt_number: Some(None),
        source_transfer_reversal: Some(None),
        status: Some(Some("succeeded".to_string())),
        transfer_reversal: Some(None),
    }
}

fn refund_destination_details() -> stripe::RefundDestinationDetails {
    stripe::RefundDestinationDetails {
        affirm: None,
        afterpay_clearpay: None,
        alipay: None,
        alma: None,
        amazon_pay: None,
        au_bank_transfer: None,
        bizum: None,
        blik: None,
        br_bank_transfer: None,
        card: Some(stripe::RefundDestinationDetailsCard {
            reference: None,
            reference_status: Some("pending".to_string()),
            reference_type: Some("acquirer_reference_number".to_string()),
            type_: stripe::RefundDestinationDetailsCardType::Refund,
        }),
        cashapp: None,
        crypto: None,
        customer_cash_balance: None,
        eps: None,
        eu_bank_transfer: None,
        gb_bank_transfer: None,
        giropay: None,
        grabpay: None,
        jp_bank_transfer: None,
        klarna: None,
        mb_way: None,
        multibanco: None,
        mx_bank_transfer: None,
        nz_bank_transfer: None,
        p24: None,
        paynow: None,
        paypal: None,
        pix: None,
        revolut: None,
        scalapay: None,
        sofort: None,
        swish: None,
        th_bank_transfer: None,
        twint: None,
        type_: "card".to_string(),
        upi: None,
        us_bank_transfer: None,
        wechat_pay: None,
        zip: None,
    }
}

/// Adds the platform's application fee to a charge's balance transaction, before it is
/// created: `fee` grows by `fee`, a second `fee_details` entry names the platform's
/// application (`ca_…`), and `net` follows.
pub(crate) fn add_application_fee(
    txn: &mut stripe::BalanceTransaction,
    fee: i64,
    application: &str,
) {
    txn.fee += fee;
    txn.net -= fee;
    txn.fee_details.push(stripe::Fee {
        amount: fee,
        application: Some(Some(application.to_string())),
        currency: txn.currency.clone(),
        description: Some(Some("Application fee".to_string())),
        type_: "application_fee".to_string(),
    });
}

/// The balance transaction of a refund: `-amount`, no fee, `available` at once.
pub(crate) fn new_refund_balance_transaction(
    id: &str,
    created: i64,
    amount: i64,
    currency: &str,
    refund: &str,
) -> stripe::BalanceTransaction {
    stripe::BalanceTransaction {
        amount: -amount,
        available_on: created,
        balance_type: stripe::BalanceTransactionBalanceType::Payments,
        created,
        currency: currency.to_string(),
        description: Some(None),
        exchange_rate: Some(None),
        fee: 0,
        fee_details: Vec::new(),
        id: id.to_string(),
        net: -amount,
        object: stripe::BalanceTransactionObject::BalanceTransaction,
        reporting_category: "refund".to_string(),
        source: Some(Some(Expandable::Id(refund.to_string()))),
        status: "available".to_string(),
        type_: stripe::BalanceTransactionType::Refund,
    }
}

/// Stripe's dispute fee, in the minor unit, withdrawn with a chargeback and returned
/// when it is won.
pub(crate) const DISPUTE_FEE: i64 = 1500;

/// Seconds from a dispute opening to its evidence being due.
pub(crate) const EVIDENCE_DUE_SECS: i64 = 14 * 86_400;

/// A chargeback's balance transaction (`type: adjustment`, `available` at once, as
/// Stripe's dispute adjustments are): the withdrawal (`-amount`, plus the dispute fee) or,
/// for a won dispute, the reinstatement (`+amount`, the fee returned).
pub(crate) fn new_dispute_balance_transaction(
    id: &str,
    created: i64,
    amount: i64,
    currency: &str,
    dispute: &str,
    charge: &str,
    reinstate: bool,
) -> stripe::BalanceTransaction {
    let (sign, category, description, fee_description) = if reinstate {
        (
            1,
            "dispute_reversal",
            format!("Chargeback reversal for {charge}"),
            "Dispute fee refund",
        )
    } else {
        (
            -1,
            "dispute",
            format!("Chargeback withdrawal for {charge}"),
            "Dispute fee",
        )
    };
    let fee = -sign * DISPUTE_FEE;
    stripe::BalanceTransaction {
        amount: sign * amount,
        available_on: created,
        balance_type: stripe::BalanceTransactionBalanceType::Payments,
        created,
        currency: currency.to_string(),
        description: Some(Some(description)),
        exchange_rate: Some(None),
        fee,
        fee_details: vec![stripe::Fee {
            amount: fee,
            application: Some(None),
            currency: currency.to_string(),
            description: Some(Some(fee_description.to_string())),
            type_: "stripe_fee".to_string(),
        }],
        id: id.to_string(),
        net: sign * amount - fee,
        object: stripe::BalanceTransactionObject::BalanceTransaction,
        reporting_category: category.to_string(),
        source: Some(Some(Expandable::Id(dispute.to_string()))),
        status: "available".to_string(),
        type_: stripe::BalanceTransactionType::Adjustment,
    }
}

/// Inputs for a dispute.
pub(crate) struct NewDispute<'a> {
    pub id: &'a str,
    pub created: i64,
    /// The disputed charge.
    pub charge: &'a stripe::Charge,
    pub reason: &'a str,
    /// An inquiry (`warning_*` statuses, no funds withdrawn) rather than a chargeback.
    pub inquiry: bool,
    /// The withdrawal, for a chargeback.
    pub balance_transactions: Vec<stripe::BalanceTransaction>,
}

/// The Visa reason code the mock reports for a dispute reason; `null` for `general`.
fn network_reason_code(reason: &str) -> Option<String> {
    let code = match reason {
        "fraudulent" => "10.4",
        "product_not_received" => "13.1",
        "duplicate" => "12.6.1",
        "subscription_canceled" => "13.2",
        _ => return None,
    };
    Some(code.to_string())
}

/// No evidence yet: every evidence field `null`.
fn empty_evidence() -> stripe::DisputeEvidence {
    stripe::DisputeEvidence {
        access_activity_log: Some(None),
        appeal: Some(None),
        billing_address: Some(None),
        cancellation_policy: Some(None),
        cancellation_policy_disclosure: Some(None),
        cancellation_rebuttal: Some(None),
        customer_communication: Some(None),
        customer_email_address: Some(None),
        customer_name: Some(None),
        customer_purchase_ip: Some(None),
        customer_signature: Some(None),
        duplicate_charge_documentation: Some(None),
        duplicate_charge_explanation: Some(None),
        duplicate_charge_id: Some(None),
        enhanced_evidence: stripe::DisputeEnhancedEvidence {
            mastercard_compliance: None,
            visa_compelling_evidence_3: None,
            visa_compliance: None,
        },
        product_description: Some(None),
        receipt: Some(None),
        refund_policy: Some(None),
        refund_policy_disclosure: Some(None),
        refund_refusal_explanation: Some(None),
        service_date: Some(None),
        service_documentation: Some(None),
        shipping_address: Some(None),
        shipping_carrier: Some(None),
        shipping_date: Some(None),
        shipping_documentation: Some(None),
        shipping_tracking_number: Some(None),
        uncategorized_file: Some(None),
        uncategorized_text: Some(None),
    }
}

/// A new card dispute on `charge` for its full amount: `needs_response` (a chargeback) or
/// `warning_needs_response` (an inquiry), no evidence, due [`EVIDENCE_DUE_SECS`] later.
pub(crate) fn new_dispute(d: &NewDispute) -> stripe::Dispute {
    let c = d.charge;
    let brand = c
        .payment_method_details
        .clone()
        .flatten()
        .and_then(|p| p.card)
        .and_then(|card| card.brand.flatten())
        .unwrap_or_else(|| BRAND.to_string());
    let (status, case_type) = if d.inquiry {
        (
            stripe::DisputeStatus::WarningNeedsResponse,
            stripe::DisputePaymentMethodDetailsCardCaseType::Inquiry,
        )
    } else {
        (
            stripe::DisputeStatus::NeedsResponse,
            stripe::DisputePaymentMethodDetailsCardCaseType::Chargeback,
        )
    };
    stripe::Dispute {
        amount: c.amount,
        balance_transactions: d.balance_transactions.clone(),
        charge: Expandable::Id(c.id.clone()),
        created: d.created,
        currency: c.currency.clone(),
        enhanced_eligibility_types: Vec::new(),
        evidence: empty_evidence(),
        evidence_details: stripe::DisputeEvidenceDetails {
            due_by: Some(Some(d.created + EVIDENCE_DUE_SECS)),
            enhanced_eligibility: stripe::DisputeEnhancedEligibility {
                mastercard_compliance: None,
                visa_compelling_evidence_3: None,
                visa_compliance: None,
            },
            has_evidence: false,
            past_due: false,
            submission_count: 0,
            submission_method: stripe::DisputeEvidenceDetailsSubmissionMethod::NotSubmitted,
        },
        id: d.id.to_string(),
        intended_submission_method: Some(None),
        // Stripe lets an inquiry's charge be refunded, not a chargeback's.
        is_charge_refundable: d.inquiry,
        livemode: false,
        metadata: BTreeMap::new(),
        object: stripe::DisputeObject::Dispute,
        payment_intent: Some(
            c.payment_intent
                .clone()
                .flatten()
                .map(|p| Expandable::Id(p.id().to_string())),
        ),
        payment_method_details: Some(stripe::DisputePaymentMethodDetails {
            amazon_pay: None,
            card: Some(stripe::DisputePaymentMethodDetailsCard {
                brand: brand.clone(),
                case_type,
                network: brand,
                network_reason_code: Some(network_reason_code(d.reason)),
            }),
            klarna: None,
            paypal: None,
            type_: stripe::DisputePaymentMethodDetailsType::Card,
        }),
        reason: d.reason.to_string(),
        status,
    }
}

/// Evidence submitted by hand: the dispute goes `under_review` (`warning_under_review`
/// for an inquiry).
pub(crate) fn submit_evidence(d: &mut stripe::Dispute) {
    d.status = if d.status == stripe::DisputeStatus::WarningNeedsResponse {
        stripe::DisputeStatus::WarningUnderReview
    } else {
        stripe::DisputeStatus::UnderReview
    };
    d.evidence.uncategorized_text = Some(Some("Evidence submitted by the merchant.".to_string()));
    let e = &mut d.evidence_details;
    e.has_evidence = true;
    e.submission_count += 1;
    e.submission_method = stripe::DisputeEvidenceDetailsSubmissionMethod::Manual;
}

/// A new off-session card setup intent for `customer`, `requires_payment_method`.
pub(crate) fn new_setup_intent(
    id: &str,
    created: i64,
    customer: &str,
    client_secret: &str,
) -> stripe::SetupIntent {
    stripe::SetupIntent {
        allowed_payment_method_types: Some(None),
        application: Some(None),
        attach_to_self: None,
        automatic_payment_methods: Some(None),
        cancellation_reason: Some(None),
        client_secret: Some(Some(client_secret.to_string())),
        created,
        customer: Some(Some(Expandable::Id(customer.to_string()))),
        customer_account: Some(None),
        description: Some(None),
        excluded_payment_method_types: Some(None),
        flow_directions: Some(None),
        id: id.to_string(),
        last_setup_error: Some(None),
        latest_attempt: Some(None),
        livemode: false,
        managed_payments: Some(None),
        mandate: Some(None),
        metadata: Some(Some(BTreeMap::new())),
        next_action: Some(None),
        object: stripe::SetupIntentObject::SetupIntent,
        on_behalf_of: Some(None),
        payment_method: Some(None),
        payment_method_configuration_details: Some(None),
        payment_method_options: Some(Some(setup_card_options())),
        payment_method_types: vec!["card".to_string()],
        single_use_mandate: Some(None),
        status: stripe::SetupIntentStatus::RequiresPaymentMethod,
        usage: "off_session".to_string(),
    }
}

fn setup_card_options() -> stripe::SetupIntentPaymentMethodOptions {
    stripe::SetupIntentPaymentMethodOptions {
        acss_debit: None,
        amazon_pay: None,
        bacs_debit: None,
        bizum: None,
        blik: None,
        card: Some(
            stripe::SetupIntentPaymentMethodOptionsCardUnion::SetupIntentPaymentMethodOptionsCard(
                stripe::SetupIntentPaymentMethodOptionsCard {
                    mandate_options: Some(None),
                    network: Some(None),
                    request_three_d_secure: Some(Some(
                        stripe::SetupIntentPaymentMethodOptionsCardRequestThreeDSecure::Automatic,
                    )),
                    setup_credential_usage: None,
                },
            ),
        ),
        card_present: None,
        klarna: None,
        link: None,
        paypal: None,
        payto: None,
        pix: None,
        sepa_debit: None,
        upi: None,
        us_bank_account: None,
    }
}

/// The `last_setup_error` of a declined card. No payment method was saved, so none is
/// named.
pub(crate) fn setup_decline_error() -> stripe::ApiErrors {
    stripe::ApiErrors {
        advice_code: None,
        charge: None,
        code: Some(DECLINE_CODE.to_string()),
        decline_code: Some("generic_decline".to_string()),
        doc_url: Some("https://docs.stripe.com/error-codes/card-declined".to_string()),
        message: Some(DECLINE_MESSAGE.to_string()),
        network_advice_code: None,
        network_decline_code: None,
        param: None,
        payment_intent: None,
        payment_method: None,
        payment_method_type: None,
        request_log_url: None,
        setup_intent: None,
        source: None,
        type_: stripe::ApiErrorsType::CardError,
    }
}

/// An `open` invoice payment through a payment intent, created at the invoice's first
/// payment attempt; `amount_paid` is null until it is paid.
pub(crate) fn new_invoice_payment(
    id: &str,
    created: i64,
    invoice: &str,
    amount: i64,
    currency: &str,
    payment_intent: &str,
) -> stripe::InvoicePayment {
    stripe::InvoicePayment {
        amount_paid: Some(None),
        amount_requested: amount,
        created,
        currency: currency.to_string(),
        id: id.to_string(),
        invoice: Expandable::Id(invoice.to_string()),
        is_default: true,
        livemode: false,
        object: stripe::InvoicePaymentObject::InvoicePayment,
        payment: stripe::InvoicesPaymentsInvoicePaymentAssociatedPayment {
            charge: None,
            payment_intent: Some(Expandable::Id(payment_intent.to_string())),
            payment_record: None,
            type_: stripe::InvoicesPaymentsInvoicePaymentAssociatedPaymentType::PaymentIntent,
        },
        status: "open".to_string(),
        status_transitions: stripe::InvoicesPaymentsInvoicePaymentStatusTransitions {
            canceled_at: Some(None),
            paid_at: Some(None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn conforms<T: serde::Serialize>(obj: &T) -> Value {
        let v = serde_json::to_value(obj).unwrap();
        stripe::check_payload(&v).unwrap_or_else(|e| panic!("{e}\n{v:#}"));
        v
    }

    fn card() -> CardDraw {
        CardDraw {
            brand: BRAND.to_string(),
            last4: LAST4.to_string(),
            exp_month: 4,
            exp_year: 2030,
            fingerprint: "Fp0123456789abcd".into(),
        }
    }

    #[test]
    fn fee_is_2_9_percent_plus_30_rounded_half_up() {
        assert_eq!(card_fee(0), 0);
        assert_eq!(card_fee(1000), 59);
        // 2.9% of 50 is 1.45 → 1; of 1500 is 43.5 → 44.
        assert_eq!(card_fee(50), 31);
        assert_eq!(card_fee(1500), 74);
    }

    #[test]
    fn payment_objects_conform() {
        let pm = new_card_payment_method("pm_1", 1, Some("cus_1"), &card());
        let v = conforms(&pm);
        assert_eq!(v["card"]["last4"], "4242");
        assert_eq!(v["customer"], "cus_1");
        assert!(v.get("us_bank_account").is_none());

        let pi = new_payment_intent(&NewPaymentIntent {
            id: "pi_1",
            created: 1,
            amount: 1000,
            currency: "usd",
            customer: Some("cus_1"),
            payment_method: "pm_1",
            client_secret: "pi_1_secret_x",
            application_fee_amount: None,
        });
        let v = conforms(&pi);
        assert_eq!(v["status"], "requires_confirmation");
        assert!(v.get("payment_details").is_none());
        assert_eq!(v["latest_charge"], Value::Null);

        for succeeded in [true, false] {
            let ch = new_charge(&NewCharge {
                id: "ch_1",
                created: 1,
                amount: 1000,
                currency: "usd",
                customer: Some("cus_1"),
                payment_intent: "pi_1",
                payment_method: &pm,
                succeeded,
                balance_transaction: succeeded.then_some("txn_1"),
                risk_score: 20,
                network_transaction_id: "123456789012345",
                application_fee: None,
            });
            let v = conforms(&ch);
            assert_eq!(v["paid"], succeeded);
            assert!(v.get("refunds").is_none());
        }
        let mut pi = pi;
        pi.last_payment_error = Some(Some(Box::new(decline_error("ch_1", &pm))));
        conforms(&pi);

        let txn = new_charge_balance_transaction("txn_1", 10, 1000, "usd", "ch_1");
        let v = conforms(&txn);
        assert_eq!(
            (v["fee"].clone(), v["net"].clone()),
            (59.into(), 941.into())
        );
        assert_eq!(v["available_on"], 10 + AVAILABLE_AFTER_SECS);
        let mut with_fee = txn.clone();
        add_application_fee(&mut with_fee, 100, "ca_1");
        let v = conforms(&with_fee);
        assert_eq!(
            (v["fee"].clone(), v["net"].clone()),
            (159.into(), 841.into())
        );
        assert_eq!(v["fee_details"][1]["type"], "application_fee");
        assert_eq!(v["fee_details"][1]["application"], "ca_1");
        let zero = conforms(&new_charge_balance_transaction(
            "txn_2", 10, 0, "usd", "ch_1",
        ));
        assert_eq!(zero["fee_details"], serde_json::json!([]));

        let ch = new_charge(&NewCharge {
            id: "ch_1",
            created: 1,
            amount: 1000,
            currency: "usd",
            customer: None,
            payment_intent: "pi_1",
            payment_method: &pm,
            succeeded: true,
            balance_transaction: Some("txn_1"),
            risk_score: 20,
            network_transaction_id: "123456789012345",
            application_fee: None,
        });
        let re = new_refund(&NewRefund {
            id: "re_1",
            created: 2,
            amount: 400,
            balance_transaction: "txn_3",
            charge: &ch,
        });
        let v = conforms(&re);
        assert_eq!(v["status"], "succeeded");
        assert_eq!(v["customer"], Value::Null);
        assert_eq!(v["payment_method"], "pm_1");
        assert_eq!(v["destination_details"]["card"]["type"], "refund");
        let txn = conforms(&new_refund_balance_transaction(
            "txn_3", 2, 400, "usd", "re_1",
        ));
        assert_eq!(
            (txn["amount"].clone(), txn["net"].clone()),
            ((-400).into(), (-400).into())
        );
        assert_eq!(txn["type"], "refund");

        let withdrawal =
            new_dispute_balance_transaction("txn_4", 3, 1000, "usd", "du_1", "ch_1", false);
        let v = conforms(&withdrawal);
        assert_eq!(
            (v["amount"].clone(), v["fee"].clone(), v["net"].clone()),
            ((-1000).into(), 1500.into(), (-2500).into())
        );
        assert_eq!(v["type"], "adjustment");
        let v = conforms(&new_dispute_balance_transaction(
            "txn_5", 4, 1000, "usd", "du_1", "ch_1", true,
        ));
        assert_eq!(
            (v["amount"].clone(), v["fee"].clone(), v["net"].clone()),
            (1000.into(), (-1500).into(), 2500.into())
        );
        for inquiry in [false, true] {
            let mut du = new_dispute(&NewDispute {
                id: "du_1",
                created: 3,
                charge: &ch,
                reason: "general",
                inquiry,
                balance_transactions: (!inquiry).then(|| withdrawal.clone()).into_iter().collect(),
            });
            let v = conforms(&du);
            assert_eq!(v["is_charge_refundable"], inquiry);
            assert_eq!(
                v["payment_method_details"]["card"]["network_reason_code"],
                Value::Null
            );
            submit_evidence(&mut du);
            let v = conforms(&du);
            let want = if inquiry {
                "warning_under_review"
            } else {
                "under_review"
            };
            assert_eq!(v["status"], want);
            assert_eq!(v["evidence_details"]["submission_count"], 1);
        }

        let amex = CardDraw {
            brand: "amex".into(),
            last4: "0005".into(),
            ..card()
        };
        let v = conforms(&new_card_payment_method("pm_2", 1, Some("cus_1"), &amex));
        assert_eq!(v["card"]["display_brand"], "american_express");
        assert_eq!(v["card"]["networks"]["available"][0], "amex");

        let mut si = new_setup_intent("seti_1", 1, "cus_1", "seti_1_secret_x");
        let v = conforms(&si);
        assert_eq!(v["status"], "requires_payment_method");
        assert_eq!(v["payment_method"], Value::Null);
        assert_eq!(
            v["payment_method_options"]["card"]["request_three_d_secure"],
            "automatic"
        );
        si.last_setup_error = Some(Some(Box::new(setup_decline_error())));
        let v = conforms(&si);
        assert_eq!(v["last_setup_error"]["code"], "card_declined");
        si.last_setup_error = Some(None);
        si.status = stripe::SetupIntentStatus::Succeeded;
        si.payment_method = Some(Some(Expandable::Id("pm_2".into())));
        conforms(&si);

        let ip = conforms(&new_invoice_payment(
            "inpay_1", 1, "in_1", 1000, "usd", "pi_1",
        ));
        assert_eq!(ip["payment"]["payment_intent"], "pi_1");
        assert_eq!(ip["status"], "open");
        assert_eq!(ip["amount_paid"], Value::Null);
        assert!(ip["payment"].get("charge").is_none());
    }
}
