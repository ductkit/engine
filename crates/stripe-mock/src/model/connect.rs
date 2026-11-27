//! Constructors for the Connect money objects: transfers, application fees, payouts,
//! external accounts and persons. They arrive with the generators that create these
//! objects (phase 8, tasks 3 to 8); this file holds the ones built so far: external
//! accounts (bank accounts and debit cards) and persons of controlled accounts, whose
//! presence follows ruling R1 (spec section 5.4).

use crate::stripe::{self, Expandable};

/// The drawn parts of a bank account.
#[derive(Debug, Clone)]
pub(crate) struct BankAccountDraw {
    pub id: String,
    pub bank_name: String,
    pub last4: String,
    pub fingerprint: String,
}

/// The drawn parts of a debit card.
#[derive(Debug, Clone)]
pub(crate) struct DebitCardDraw {
    pub id: String,
    /// `visa` or `mastercard`.
    pub brand: String,
    pub last4: String,
    pub exp_month: i64,
    pub exp_year: i64,
    pub fingerprint: String,
}

/// The drawn parts of a person.
#[derive(Debug, Clone)]
pub(crate) struct PersonDraw {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
}

impl PersonDraw {
    /// `{first}.{last}@example.com`, lowercase.
    pub(crate) fn email(&self) -> String {
        format!(
            "{}.{}@example.com",
            self.first_name.to_lowercase(),
            self.last_name.to_lowercase()
        )
    }
}

/// An external account's requirements: nothing is due.
fn no_external_requirements() -> stripe::ExternalAccountRequirements {
    stripe::ExternalAccountRequirements {
        currently_due: Some(Some(Vec::new())),
        errors: Some(Some(Vec::new())),
        past_due: Some(Some(Vec::new())),
        pending_verification: Some(Some(Vec::new())),
    }
}

/// A US company bank account of `account`.
pub(crate) fn new_bank_account(
    account: &str,
    d: &BankAccountDraw,
    default_for_currency: bool,
) -> stripe::BankAccount {
    stripe::BankAccount {
        account: Some(Some(Expandable::Id(account.to_string()))),
        account_holder_name: Some(None),
        account_holder_type: Some(Some("company".to_string())),
        account_type: Some(None),
        available_payout_methods: Some(Some(vec![
            stripe::BankAccountAvailablePayoutMethods::Standard,
        ])),
        bank_name: Some(Some(d.bank_name.clone())),
        country: "US".to_string(),
        currency: "usd".to_string(),
        customer: Some(None),
        default_for_currency: Some(Some(default_for_currency)),
        fingerprint: Some(Some(d.fingerprint.clone())),
        future_requirements: Some(Some(no_external_requirements())),
        id: d.id.clone(),
        last4: d.last4.clone(),
        metadata: Some(Some(Default::default())),
        object: stripe::BankAccountObject::BankAccount,
        requirements: Some(Some(no_external_requirements())),
        routing_number: Some(Some("110000000".to_string())),
        status: "new".to_string(),
    }
}

/// A US debit card of `account`. Cards are never the default.
pub(crate) fn new_debit_card(account: &str, d: &DebitCardDraw) -> stripe::Card {
    stripe::Card {
        account: Some(Some(Expandable::Id(account.to_string()))),
        address_city: Some(None),
        address_country: Some(None),
        address_line1: Some(None),
        address_line1_check: Some(None),
        address_line2: Some(None),
        address_state: Some(None),
        address_zip: Some(None),
        address_zip_check: Some(None),
        allow_redisplay: Some(None),
        available_payout_methods: Some(Some(vec![
            stripe::CardAvailablePayoutMethods::Instant,
            stripe::CardAvailablePayoutMethods::Standard,
        ])),
        brand: d.brand.clone(),
        country: Some(Some("US".to_string())),
        currency: Some(Some("usd".to_string())),
        customer: Some(None),
        cvc_check: Some(None),
        default_for_currency: Some(Some(false)),
        dynamic_last4: Some(None),
        exp_month: d.exp_month,
        exp_year: d.exp_year,
        fingerprint: Some(Some(d.fingerprint.clone())),
        funding: "debit".to_string(),
        id: d.id.clone(),
        last4: d.last4.clone(),
        metadata: Some(Some(Default::default())),
        name: Some(None),
        networks: Some(stripe::TokenCardNetworks {
            preferred: Some(None),
        }),
        object: stripe::CardObject::Card,
        regulated_status: Some(None),
        status: Some(None),
        tokenization_method: Some(None),
        wallet: Some(None),
    }
}

/// The tombstone of a deleted bank account.
pub(crate) fn deleted_bank_account(id: &str) -> stripe::DeletedBankAccount {
    stripe::DeletedBankAccount {
        currency: Some(Some("usd".to_string())),
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedBankAccountObject::BankAccount,
    }
}

/// The tombstone of a deleted card.
pub(crate) fn deleted_card(id: &str) -> stripe::DeletedCard {
    stripe::DeletedCard {
        currency: Some(Some("usd".to_string())),
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedCardObject::Card,
    }
}

/// The tombstone of a deleted person.
pub(crate) fn deleted_person(id: &str) -> stripe::DeletedPerson {
    stripe::DeletedPerson {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedPersonObject::Person,
    }
}

fn empty_address() -> stripe::Address {
    stripe::Address {
        city: Some(None),
        country: Some(None),
        line1: Some(None),
        line2: Some(None),
        postal_code: Some(None),
        state: Some(None),
    }
}

fn empty_document() -> stripe::LegalEntityPersonVerificationDocument {
    stripe::LegalEntityPersonVerificationDocument {
        back: Some(None),
        details: Some(None),
        details_code: Some(None),
        front: Some(None),
    }
}

/// A person of `account`. Only the first one is the representative and an executive; every
/// other role is `false`. Nothing is due, and nothing is verified.
pub(crate) fn new_person(
    account: &str,
    created: i64,
    d: &PersonDraw,
    representative: bool,
) -> stripe::Person {
    stripe::Person {
        account: account.to_string(),
        additional_tos_acceptances: Some(stripe::PersonAdditionalTosAcceptances {
            account: Some(Some(stripe::PersonAdditionalTosAcceptance {
                date: Some(None),
                ip: Some(None),
                user_agent: Some(None),
            })),
        }),
        address: Some(empty_address()),
        address_kana: Some(None),
        address_kanji: Some(None),
        created,
        dob: Some(stripe::LegalEntityDob {
            day: Some(None),
            month: Some(None),
            year: Some(None),
        }),
        email: Some(Some(d.email())),
        first_name: Some(Some(d.first_name.clone())),
        first_name_kana: Some(None),
        first_name_kanji: Some(None),
        full_name_aliases: Some(Vec::new()),
        future_requirements: Some(Some(stripe::PersonFutureRequirements {
            alternatives: Some(Some(Vec::new())),
            currently_due: Vec::new(),
            errors: Vec::new(),
            eventually_due: Vec::new(),
            past_due: Vec::new(),
            pending_verification: Vec::new(),
        })),
        gender: Some(None),
        id: d.id.clone(),
        id_number_provided: Some(false),
        id_number_secondary_provided: Some(false),
        last_name: Some(Some(d.last_name.clone())),
        last_name_kana: Some(None),
        last_name_kanji: Some(None),
        maiden_name: Some(None),
        metadata: Some(Default::default()),
        nationality: Some(None),
        object: stripe::PersonObject::Person,
        phone: Some(None),
        political_exposure: Some(stripe::PersonPoliticalExposure::None),
        registered_address: Some(empty_address()),
        relationship: Some(stripe::PersonRelationship {
            authorizer: Some(Some(false)),
            director: Some(Some(false)),
            executive: Some(Some(representative)),
            legal_guardian: Some(Some(false)),
            owner: Some(Some(false)),
            percent_ownership: Some(None),
            representative: Some(Some(representative)),
            title: Some(None),
        }),
        requirements: Some(Some(stripe::PersonRequirements {
            alternatives: Some(Some(Vec::new())),
            currently_due: Vec::new(),
            errors: Vec::new(),
            eventually_due: Vec::new(),
            past_due: Vec::new(),
            pending_verification: Vec::new(),
        })),
        ssn_last_4_provided: Some(false),
        us_cfpb_data: Some(None),
        verification: Some(stripe::LegalEntityPersonVerification {
            additional_document: Some(Some(empty_document())),
            details: Some(None),
            details_code: Some(None),
            document: Some(empty_document()),
            status: "unverified".to_string(),
        }),
    }
}

/// A person's new contact details.
pub(crate) fn apply_person_contact(p: &mut stripe::Person, email: &str, phone: &str) {
    p.email = Some(Some(email.to_string()));
    p.phone = Some(Some(phone.to_string()));
}

/// Whether a person is the account's representative.
pub(crate) fn is_representative(p: &stripe::Person) -> bool {
    matches!(
        &p.relationship,
        Some(stripe::PersonRelationship {
            representative: Some(Some(true)),
            ..
        })
    )
}

/// The account's `requirements` once its last representative is gone: the representative's
/// name is due.
pub(crate) fn representative_due(a: &mut stripe::Account) {
    let due = || {
        Some(Some(vec![
            "representative.first_name".to_string(),
            "representative.last_name".to_string(),
        ]))
    };
    if let Some(r) = &mut a.requirements {
        r.currently_due = due();
        r.eventually_due = due();
    }
}
