//! Checkout sessions, built on the generated `crate::stripe::CheckoutSession`.
//!
//! Presence follows ruling R1 as in `model::payments`: every field is present (`null`,
//! empty or a value) unless it is includable-only (`line_items`) or optional and not
//! nullable and only appears in a situation the mock does not model: Adaptive Pricing
//! (`presentment_details`), embedded and Elements sessions (`redirect_on_completion`,
//! `return_url`), excluded payment method types and name collection. Those are absent.

use std::collections::BTreeMap;

use crate::stripe::{self, Expandable};

/// Seconds from a session's creation to its `expires_at` (Stripe's default, 24 hours).
pub(crate) const EXPIRES_AFTER_SECS: i64 = 86_400;

/// Where a completed session sends the customer. Sessions are hosted, so Stripe requires one.
const SUCCESS_URL: &str = "https://example.com/checkout/success";

/// Inputs for a session.
pub(crate) struct NewSession<'a> {
    pub id: &'a str,
    pub created: i64,
    pub customer: &'a str,
    /// Subscription mode rather than payment mode.
    pub subscription: bool,
    pub amount: i64,
    pub currency: &'a str,
}

fn invoice_creation() -> stripe::PaymentPagesCheckoutSessionInvoiceCreation {
    stripe::PaymentPagesCheckoutSessionInvoiceCreation {
        enabled: false,
        invoice_data: stripe::PaymentPagesCheckoutSessionInvoiceSettings {
            account_tax_ids: Some(None),
            custom_fields: Some(None),
            description: Some(None),
            footer: Some(None),
            issuer: Some(None),
            metadata: Some(Some(BTreeMap::new())),
            rendering_options: Some(None),
        },
    }
}

fn card_options() -> stripe::CheckoutSessionPaymentMethodOptions {
    stripe::CheckoutSessionPaymentMethodOptions {
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
        boleto: None,
        card: Some(stripe::CheckoutCardPaymentMethodOptions {
            capture_method: None,
            installments: None,
            request_extended_authorization: None,
            request_incremental_authorization: None,
            request_multicapture: None,
            request_overcapture: None,
            request_three_d_secure:
                stripe::CheckoutCardPaymentMethodOptionsRequestThreeDSecure::Automatic,
            restrictions: None,
            setup_future_usage: None,
            statement_descriptor_suffix_kana: None,
            statement_descriptor_suffix_kanji: None,
        }),
        cashapp: None,
        customer_balance: None,
        eps: None,
        fpx: None,
        giropay: None,
        grabpay: None,
        ideal: None,
        kakao_pay: None,
        klarna: None,
        konbini: None,
        kr_card: None,
        link: None,
        mobilepay: None,
        multibanco: None,
        naver_pay: None,
        oxxo: None,
        p24: None,
        payco: None,
        paynow: None,
        paypal: None,
        payto: None,
        pix: None,
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
    }
}

/// The hosted page of an open session.
pub(crate) fn session_url(id: &str) -> String {
    format!("https://checkout.stripe.com/c/pay/{id}")
}

/// A new hosted card session for `customer`, `open` and `unpaid`, expiring
/// [`EXPIRES_AFTER_SECS`] after `created`. Nothing is linked yet: the payment intent
/// (payment mode) or subscription and invoice (subscription mode) come with completion.
pub(crate) fn new_checkout_session(s: &NewSession) -> stripe::CheckoutSession {
    let (mode, collection, invoice_creation) = if s.subscription {
        (
            stripe::CheckoutSessionMode::Subscription,
            stripe::CheckoutSessionPaymentMethodCollection::Always,
            None,
        )
    } else {
        (
            stripe::CheckoutSessionMode::Payment,
            stripe::CheckoutSessionPaymentMethodCollection::IfRequired,
            Some(invoice_creation()),
        )
    };
    stripe::CheckoutSession {
        adaptive_pricing: Some(Some(stripe::PaymentPagesCheckoutSessionAdaptivePricing {
            enabled: false,
        })),
        after_expiration: Some(None),
        allow_promotion_codes: Some(None),
        allowed_payment_method_types: Some(None),
        amount_subtotal: Some(Some(s.amount)),
        amount_total: Some(Some(s.amount)),
        automatic_tax: stripe::PaymentPagesCheckoutSessionAutomaticTax {
            address_collection_precision: Some(None),
            enabled: false,
            liability: Some(None),
            provider: Some(None),
            status: Some(None),
        },
        billing_address_collection: Some(None),
        branding_settings: Some(stripe::PaymentPagesCheckoutSessionBrandingSettings {
            background_color: "#ffffff".to_string(),
            border_style: stripe::PaymentPagesCheckoutSessionBrandingSettingsBorderStyle::Rounded,
            button_color: "#0074d4".to_string(),
            display_name: "DUCTKIT MOCK".to_string(),
            font_family: "default".to_string(),
            icon: Some(None),
            logo: Some(None),
        }),
        cancel_url: Some(None),
        client_reference_id: Some(None),
        client_secret: Some(None),
        collected_information: Some(None),
        consent: Some(None),
        consent_collection: Some(None),
        created: s.created,
        currency: Some(Some(s.currency.to_string())),
        currency_conversion: Some(None),
        custom_fields: Vec::new(),
        custom_text: stripe::PaymentPagesCheckoutSessionCustomText {
            after_submit: Some(None),
            shipping_address: Some(None),
            submit: Some(None),
            terms_of_service_acceptance: Some(None),
        },
        customer: Some(Some(Expandable::Id(s.customer.to_string()))),
        customer_account: Some(None),
        // A session for an existing customer creates none.
        customer_creation: Some(None),
        customer_details: Some(None),
        customer_email: Some(None),
        discounts: Some(Some(Vec::new())),
        excluded_payment_method_types: None,
        expires_at: s.created + EXPIRES_AFTER_SECS,
        id: s.id.to_string(),
        integration_identifier: Some(None),
        invoice: Some(None),
        invoice_creation: Some(invoice_creation),
        line_items: None,
        livemode: false,
        locale: Some(None),
        managed_payments: Some(None),
        metadata: Some(Some(BTreeMap::new())),
        mode,
        name_collection: None,
        object: stripe::CheckoutSessionObject::CheckoutSession,
        optional_items: Some(None),
        origin_context: Some(None),
        payment_intent: Some(None),
        payment_link: Some(None),
        payment_method_collection: Some(Some(collection)),
        payment_method_configuration_details: Some(None),
        payment_method_options: Some(Some(card_options())),
        payment_method_types: vec!["card".to_string()],
        payment_status: stripe::CheckoutSessionPaymentStatus::Unpaid,
        permissions: Some(None),
        phone_number_collection: Some(stripe::PaymentPagesCheckoutSessionPhoneNumberCollection {
            enabled: false,
        }),
        presentment_details: None,
        recovered_from: Some(None),
        redirect_on_completion: None,
        return_url: None,
        saved_payment_method_options: Some(Some(
            stripe::PaymentPagesCheckoutSessionSavedPaymentMethodOptions {
                allow_redisplay_filters: Some(Some(vec![
                    stripe::PaymentPagesCheckoutSessionSavedPaymentMethodOptionsAllowRedisplayFilters::Always,
                ])),
                payment_method_remove: Some(Some(
                    stripe::PaymentPagesCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove::Disabled,
                )),
                payment_method_save: Some(None),
            },
        )),
        setup_intent: Some(None),
        shipping_address_collection: Some(None),
        shipping_cost: Some(None),
        shipping_options: Vec::new(),
        status: Some(Some(stripe::CheckoutSessionStatus::Open)),
        submit_type: Some(None),
        subscription: Some(None),
        success_url: Some(Some(SUCCESS_URL.to_string())),
        tax_id_collection: Some(stripe::PaymentPagesCheckoutSessionTaxIdCollection {
            enabled: false,
            required: stripe::PaymentPagesCheckoutSessionTaxIdCollectionRequired::Never,
        }),
        total_details: Some(Some(stripe::PaymentPagesCheckoutSessionTotalDetails {
            amount_discount: 0,
            amount_shipping: Some(Some(0)),
            amount_tax: 0,
            breakdown: None,
        })),
        ui_mode: Some(Some(stripe::CheckoutSessionUiMode::HostedPage)),
        url: Some(Some(session_url(s.id))),
        wallet_options: Some(None),
    }
}

/// What a completed session links: its payment intent (payment mode), or its subscription
/// and first invoice (subscription mode).
pub(crate) enum Completion<'a> {
    Payment {
        payment_intent: &'a str,
    },
    Subscription {
        subscription: &'a str,
        invoice: &'a str,
    },
}

/// The customer's details as the session collected them.
fn customer_details(c: &stripe::Customer) -> stripe::PaymentPagesCheckoutSessionCustomerDetails {
    use stripe::{
        CustomerTaxExempt as C, PaymentPagesCheckoutSessionCustomerDetailsTaxExempt as T,
    };
    let tax_exempt = c.tax_exempt.flatten().map(|t| match t {
        C::Exempt => T::Exempt,
        C::None => T::None,
        C::Reverse => T::Reverse,
    });
    stripe::PaymentPagesCheckoutSessionCustomerDetails {
        address: Some(c.address.clone().flatten()),
        business_name: Some(None),
        email: Some(c.email.clone().flatten()),
        individual_name: Some(None),
        name: Some(c.name.clone().flatten()),
        phone: Some(c.phone.clone().flatten()),
        tax_exempt: Some(Some(tax_exempt.unwrap_or(T::None))),
        tax_ids: Some(Some(Vec::new())),
    }
}

/// `open → complete`: paid (`no_payment_required` for a zero total), the links of `done`
/// set, the customer's details collected and the hosted page gone.
pub(crate) fn complete(
    s: &mut stripe::CheckoutSession,
    customer: &stripe::Customer,
    done: &Completion,
) {
    s.status = Some(Some(stripe::CheckoutSessionStatus::Complete));
    s.payment_status = if s.amount_total.flatten().unwrap_or(0) == 0 {
        stripe::CheckoutSessionPaymentStatus::NoPaymentRequired
    } else {
        stripe::CheckoutSessionPaymentStatus::Paid
    };
    s.customer_details = Some(Some(customer_details(customer)));
    s.url = Some(None);
    match done {
        Completion::Payment { payment_intent } => {
            s.payment_intent = Some(Some(Expandable::Id(payment_intent.to_string())));
        }
        Completion::Subscription {
            subscription,
            invoice,
        } => {
            s.subscription = Some(Some(Expandable::Id(subscription.to_string())));
            s.invoice = Some(Some(Expandable::Id(invoice.to_string())));
        }
    }
}

/// `open → expired`: the hosted page is gone.
pub(crate) fn expire(s: &mut stripe::CheckoutSession) {
    s.status = Some(Some(stripe::CheckoutSessionStatus::Expired));
    s.url = Some(None);
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

    #[test]
    fn sessions_conform_through_their_lifecycle() {
        let customer: stripe::Customer = serde_json::from_value(serde_json::json!({
            "id": "cus_1", "object": "customer", "created": 1, "livemode": false,
            "email": "pat@example.com", "name": "Pat", "address": null, "phone": null,
            "tax_exempt": "none"
        }))
        .unwrap();
        for subscription in [false, true] {
            let mut s = new_checkout_session(&NewSession {
                id: "cs_test_1",
                created: 100,
                customer: "cus_1",
                subscription,
                amount: 1500,
                currency: "usd",
            });
            let v = conforms(&s);
            assert_eq!(v["object"], "checkout.session");
            assert_eq!(v["status"], "open");
            assert_eq!(v["expires_at"], 100 + EXPIRES_AFTER_SECS);
            assert!(v.get("line_items").is_none());
            assert_eq!(v["invoice_creation"].is_null(), subscription);
            let mut e = s.clone();
            expire(&mut e);
            let v = conforms(&e);
            assert_eq!(
                (v["status"].clone(), v["url"].clone()),
                ("expired".into(), Value::Null)
            );
            let done = if subscription {
                Completion::Subscription {
                    subscription: "sub_1",
                    invoice: "in_1",
                }
            } else {
                Completion::Payment {
                    payment_intent: "pi_1",
                }
            };
            complete(&mut s, &customer, &done);
            let v = conforms(&s);
            assert_eq!(v["payment_status"], "paid");
            assert_eq!(v["customer_details"]["email"], "pat@example.com");
            assert_eq!(v["customer_details"]["tax_exempt"], "none");
            assert_eq!(v["payment_intent"].is_null(), subscription);
            assert_eq!(v["subscription"].is_null(), !subscription);
        }
    }
}
