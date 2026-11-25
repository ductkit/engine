//! Rust names for schemas, fields and enum values.

/// Rust keywords (strict and reserved, all editions). A field named like one gets a `_` suffix.
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "try", "type",
    "typeof", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Splits on every character that is not ASCII alphanumeric and upper-cases the first
/// character of each word, keeping the rest as written.
fn camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    out
}

/// `checkout.session` -> `CheckoutSession`, `v2.core.account` -> `V2CoreAccount`.
pub fn type_name(schema: &str) -> String {
    camel(schema)
}

/// Snake case, with Rust keywords suffixed `_` (`type` -> `type_`).
pub fn field_name(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 1);
    let mut prev_lower = false;
    for c in json.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_lower = true;
        } else {
            out.push('_');
            prev_lower = false;
        }
    }
    if KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// CamelCase. A value starting with a digit gets a `V` prefix (`3d_secure` -> `V3dSecure`),
/// and the empty string becomes `Empty`.
pub fn variant_name(value: &str) -> String {
    if value.is_empty() {
        return "Empty".to_string();
    }
    let mut out = camel(value);
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'V');
    }
    if KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// Whether `s` can be used as a plain Rust identifier.
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s != "_"
        && !KEYWORDS.contains(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names_are_camel_case() {
        assert_eq!(type_name("checkout.session"), "CheckoutSession");
        assert_eq!(type_name("v2.core.account"), "V2CoreAccount");
        assert_eq!(type_name("line_item"), "LineItem");
        assert_eq!(type_name("customer"), "Customer");
    }

    #[test]
    fn field_names_are_snake_case_with_keywords_suffixed() {
        assert_eq!(field_name("type"), "type_");
        assert_eq!(field_name("ref"), "ref_");
        assert_eq!(field_name("self"), "self_");
        assert_eq!(field_name("amount_due"), "amount_due");
        assert_eq!(field_name("amountDue"), "amount_due");
    }

    #[test]
    fn variant_names_are_camel_case_and_valid() {
        assert_eq!(
            variant_name("requires_payment_method"),
            "RequiresPaymentMethod"
        );
        assert_eq!(variant_name("3d_secure"), "V3dSecure");
        assert_eq!(variant_name(""), "Empty");
        assert_eq!(variant_name("1.0.2"), "V102");
        assert_eq!(variant_name("visa-checkout"), "VisaCheckout");
        assert_eq!(variant_name("AC"), "AC");
        assert_eq!(variant_name("self"), "Self_");
    }
}
