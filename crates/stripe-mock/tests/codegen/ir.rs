//! The generator's intermediate representation: every Rust type to emit, built from the
//! OpenAPI spec and the root list.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;

use super::names::{field_name, is_identifier, type_name, variant_name};

/// Every type to generate, keyed by Rust name.
#[derive(Clone, Debug, PartialEq)]
pub struct Ir {
    pub types: BTreeMap<String /*rust name*/, TypeDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeDef {
    Struct(StructDef),
    Enum(EnumDef),
    Union(UnionDef),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDef {
    /// The component schema name (`checkout.session`), or for an inline object the path
    /// `{parent schema}/{field}` (`customer/invoice_settings`). Only inline paths contain `/`.
    pub schema: String,
    /// The value of a one-value `object` enum, when the struct has one.
    pub object: Option<String>,
    /// Fields in spec order.
    pub fields: Vec<FieldDef>,
    /// JSON names of the fields that can be expanded (type `Expandable`, or a `Vec` of it).
    pub expandable: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDef {
    pub json: String,
    pub rust: String,
    pub ty: TypeRef,
    pub required: bool,
    pub nullable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeRef {
    String,
    I64,
    F64,
    Bool,
    /// A generated type, by Rust name (a key of `Ir::types`).
    Named(String),
    Vec(Box<TypeRef>),
    /// String-keyed map with this value type.
    Map(Box<TypeRef>),
    /// An expandable reference: an ID or the expanded object.
    Expandable(Box<TypeRef>),
    /// An expandable reference whose targets are all outside the roots: only the ID.
    IdOnly,
    /// A `list` wrapper with this item type.
    List(Box<TypeRef>),
    /// Free-form JSON, where the schema itself is free-form.
    Json,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDef {
    /// The string values, in spec order, without duplicates.
    pub values: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnionDef {
    pub variants: Vec<TypeRef>,
}

const REF_PREFIX: &str = "#/components/schemas/";

/// Builds the IR for `roots` plus the closure over their non-expandable `$ref`s.
///
/// - A field is expandable when it is in the parent's `x-expandableFields` and its schema,
///   or its array `items`, is an `anyOf` containing `{type: string}`. Root targets give
///   `Expandable` (over a `{Parent}{Field}` union when there are several); no root target
///   gives `IdOnly`. Non-root targets are not followed.
/// - Single-element `allOf`/`anyOf`/`oneOf` are wrappers for their element.
/// - Inline objects, enums and unions are named `{Parent}{Field}`.
/// - Two types with the same Rust name are an error naming both sources.
pub fn build_ir(spec: &Value, roots: &[&str]) -> Result<Ir, String> {
    let schemas = spec
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .ok_or("spec has no components.schemas")?;
    let mut b = Builder {
        schemas,
        roots: roots.iter().copied().collect(),
        types: BTreeMap::new(),
        origins: BTreeMap::new(),
        queue: VecDeque::new(),
        seen: BTreeSet::new(),
    };
    for root in roots {
        b.reference(root)?;
    }
    while let Some(name) = b.queue.pop_front() {
        let rust = type_name(name);
        let def = b.build_struct(&rust, name, &schemas[name])?;
        b.insert(rust, name.to_string(), TypeDef::Struct(def))?;
    }
    Ok(Ir { types: b.types })
}

struct Builder<'a> {
    schemas: &'a serde_json::Map<String, Value>,
    roots: BTreeSet<&'a str>,
    types: BTreeMap<String, TypeDef>,
    /// Rust name -> where it came from (schema name or inline path), for collision errors.
    origins: BTreeMap<String, String>,
    queue: VecDeque<&'a str>,
    seen: BTreeSet<&'a str>,
}

impl<'a> Builder<'a> {
    fn insert(&mut self, rust: String, origin: String, def: TypeDef) -> Result<(), String> {
        if !is_identifier(&rust) {
            return Err(format!(
                "`{origin}` gives type name `{rust}`, which is not a Rust identifier"
            ));
        }
        if let Some(prev) = self.origins.get(&rust) {
            return Err(format!(
                "type name `{rust}` is generated for both `{prev}` and `{origin}`"
            ));
        }
        self.origins.insert(rust.clone(), origin);
        self.types.insert(rust, def);
        Ok(())
    }

    /// A reference to a named schema; queues it for generation.
    fn reference(&mut self, name: &str) -> Result<TypeRef, String> {
        let Some((key, _)) = self.schemas.get_key_value(name) else {
            return Err(format!("schema `{name}` is referenced but not defined"));
        };
        if self.seen.insert(key.as_str()) {
            self.queue.push_back(key.as_str());
        }
        Ok(TypeRef::Named(type_name(name)))
    }

    fn build_struct(&mut self, rust: &str, label: &str, s: &'a Value) -> Result<StructDef, String> {
        let props = s
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("`{label}` has no properties"))?;
        let required = string_set(s, "required");
        let expandable_fields = string_set(s, "x-expandableFields");
        let mut fields = Vec::with_capacity(props.len());
        let mut expandable = Vec::new();
        let mut object = None;
        let mut rust_names = BTreeSet::new();
        for (json, p) in props {
            let rust_field = field_name(json);
            if !is_identifier(&rust_field) {
                return Err(format!(
                    "field `{label}/{json}` gives `{rust_field}`, which is not a Rust identifier"
                ));
            }
            if !rust_names.insert(rust_field.clone()) {
                return Err(format!(
                    "two fields of `{label}` are both named `{rust_field}` in Rust"
                ));
            }
            let object_value = if json == "object" {
                single_string_enum(p)
            } else {
                None
            };
            let ty = match object_value {
                Some(value) => {
                    object = Some(value.to_string());
                    let name = format!("{rust}Object");
                    self.insert_enum(&name, &format!("{label}/object"), &[value])?;
                    TypeRef::Named(name)
                }
                None => self.field_type(
                    rust,
                    label,
                    json,
                    p,
                    expandable_fields.contains(json.as_str()),
                )?,
            };
            if is_expandable(&ty) {
                expandable.push(json.clone());
            }
            fields.push(FieldDef {
                json: json.clone(),
                rust: rust_field,
                ty,
                required: required.contains(json.as_str()),
                nullable: nullable_through(p),
            });
        }
        Ok(StructDef {
            schema: label.to_string(),
            object,
            fields,
            expandable,
        })
    }

    fn field_type(
        &mut self,
        parent: &str,
        parent_label: &str,
        field: &str,
        p: &'a Value,
        in_expandable_fields: bool,
    ) -> Result<TypeRef, String> {
        let name = format!("{parent}{}", type_name(field));
        let origin = format!("{parent_label}/{field}");
        if in_expandable_fields {
            let s = unwrap(p);
            if let Some(t) = self.expandable(&name, &origin, s)? {
                return Ok(t);
            }
            if s.get("type").and_then(Value::as_str) == Some("array")
                && let Some(items) = s.get("items")
                && let Some(t) = self.expandable(&name, &origin, unwrap(items))?
            {
                return Ok(TypeRef::Vec(Box::new(t)));
            }
        }
        self.schema_type(&name, &origin, p)
    }

    /// `Some` when `s` is an `anyOf` containing `{type: string}`.
    fn expandable(
        &mut self,
        name: &str,
        origin: &str,
        s: &'a Value,
    ) -> Result<Option<TypeRef>, String> {
        let Some(alts) = s.get("anyOf").and_then(Value::as_array) else {
            return Ok(None);
        };
        if !alts.iter().any(is_plain_string) {
            return Ok(None);
        }
        let mut targets = Vec::new();
        for alt in alts {
            match ref_target(alt)? {
                Some(t) if self.roots.contains(t) => targets.push(t),
                Some(_) => {}
                None if is_plain_string(alt) => {}
                None => {
                    return Err(format!(
                        "expandable `{origin}` has an alternative that is neither a string nor a $ref"
                    ));
                }
            }
        }
        let inner = match targets.as_slice() {
            [] => return Ok(Some(TypeRef::IdOnly)),
            [one] => self.reference(one)?,
            many => {
                let variants = many
                    .iter()
                    .map(|t| self.reference(t))
                    .collect::<Result<Vec<_>, _>>()?;
                self.insert(
                    name.to_string(),
                    origin.to_string(),
                    TypeDef::Union(UnionDef { variants }),
                )?;
                TypeRef::Named(name.to_string())
            }
        };
        Ok(Some(TypeRef::Expandable(Box::new(inner))))
    }

    /// The type of any schema; a generated type is named `name`.
    fn schema_type(&mut self, name: &str, origin: &str, s: &'a Value) -> Result<TypeRef, String> {
        let s = unwrap(s);
        if let Some(target) = ref_target(s)? {
            return self.reference(target);
        }
        if s.get("allOf").is_some() {
            return Err(format!(
                "`{origin}`: allOf with several elements is not supported"
            ));
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(alts) = s.get(key).and_then(Value::as_array) {
                // Suffixed: Stripe names the schema of one alternative `{parent}_{field}`
                // (`payment_intent_payment_method_options.card` is an anyOf over
                // `payment_intent_payment_method_options_card` and a client variant), so a
                // bare `{Parent}{Field}` would collide with it.
                let union = format!("{name}Union");
                let mut variants = Vec::with_capacity(alts.len());
                for (i, alt) in alts.iter().enumerate() {
                    variants.push(self.schema_type(
                        &format!("{union}Variant{i}"),
                        &format!("{origin}/{key}/{i}"),
                        alt,
                    )?);
                }
                self.insert(
                    union.clone(),
                    origin.to_string(),
                    TypeDef::Union(UnionDef { variants }),
                )?;
                return Ok(TypeRef::Named(union));
            }
        }
        match s.get("type").and_then(Value::as_str) {
            Some("string") => match s.get("enum") {
                Some(values) => {
                    let values = values
                        .as_array()
                        .and_then(|v| v.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
                        .ok_or_else(|| format!("`{origin}`: string enum with non-string values"))?;
                    self.insert_enum(name, origin, &values)?;
                    Ok(TypeRef::Named(name.to_string()))
                }
                None => Ok(TypeRef::String),
            },
            Some("integer") => Ok(TypeRef::I64),
            Some("number") => Ok(TypeRef::F64),
            Some("boolean") => Ok(TypeRef::Bool),
            Some("array") => {
                let items = s
                    .get("items")
                    .ok_or_else(|| format!("`{origin}`: array without items"))?;
                Ok(TypeRef::Vec(Box::new(
                    self.schema_type(name, origin, items)?,
                )))
            }
            Some("object") | None => self.object_type(name, origin, s),
            Some(other) => Err(format!("`{origin}`: unsupported type `{other}`")),
        }
    }

    fn object_type(&mut self, name: &str, origin: &str, s: &'a Value) -> Result<TypeRef, String> {
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            let is_list = props
                .get("object")
                .and_then(single_string_enum)
                .is_some_and(|v| v == "list");
            let data = props
                .get("data")
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("array"));
            if let (true, Some(data)) = (is_list, data) {
                let items = data
                    .get("items")
                    .ok_or_else(|| format!("`{origin}/data`: array without items"))?;
                let item = self.schema_type(name, &format!("{origin}/data"), items)?;
                return Ok(TypeRef::List(Box::new(item)));
            }
            let def = self.build_struct(name, origin, s)?;
            self.insert(name.to_string(), origin.to_string(), TypeDef::Struct(def))?;
            return Ok(TypeRef::Named(name.to_string()));
        }
        match s.get("additionalProperties") {
            Some(Value::Bool(true)) => Ok(TypeRef::Map(Box::new(TypeRef::Json))),
            Some(value @ Value::Object(_)) => Ok(TypeRef::Map(Box::new(
                self.schema_type(name, origin, value)?,
            ))),
            _ => Ok(TypeRef::Json),
        }
    }

    fn insert_enum(&mut self, name: &str, origin: &str, values: &[&str]) -> Result<(), String> {
        let mut unique: Vec<String> = Vec::with_capacity(values.len());
        let mut variants: BTreeMap<String, &str> = BTreeMap::new();
        for value in values {
            if unique.iter().any(|u| u == value) {
                continue;
            }
            let variant = variant_name(value);
            if !is_identifier(&variant) {
                return Err(format!(
                    "`{origin}`: enum value {value:?} gives `{variant}`, which is not a Rust identifier"
                ));
            }
            if let Some(prev) = variants.insert(variant.clone(), value) {
                return Err(format!(
                    "`{origin}`: enum values {prev:?} and {value:?} are both named `{variant}`"
                ));
            }
            unique.push(value.to_string());
        }
        self.insert(
            name.to_string(),
            origin.to_string(),
            TypeDef::Enum(EnumDef { values: unique }),
        )
    }
}

fn string_set<'v>(s: &'v Value, key: &str) -> BTreeSet<&'v str> {
    s.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn ref_target(s: &Value) -> Result<Option<&str>, String> {
    match s.get("$ref") {
        None => Ok(None),
        Some(r) => r
            .as_str()
            .and_then(|r| r.strip_prefix(REF_PREFIX))
            .map(Some)
            .ok_or_else(|| format!("unsupported $ref {r}")),
    }
}

/// The element of a single-element `allOf`/`anyOf`/`oneOf`, repeatedly.
fn unwrap(mut s: &Value) -> &Value {
    while let Some(one) = single_wrapped(s) {
        s = one;
    }
    s
}

fn single_wrapped(s: &Value) -> Option<&Value> {
    ["allOf", "anyOf", "oneOf"]
        .iter()
        .find_map(|k| match s.get(*k)?.as_array()?.as_slice() {
            [one] => Some(one),
            _ => None,
        })
}

/// `nullable: true` on the property or on any single-element wrapper inside it.
fn nullable_through(mut s: &Value) -> bool {
    loop {
        if s.get("nullable") == Some(&Value::Bool(true)) {
            return true;
        }
        match single_wrapped(s) {
            Some(one) => s = one,
            None => return false,
        }
    }
}

fn is_plain_string(s: &Value) -> bool {
    s.get("$ref").is_none() && s.get("type").and_then(Value::as_str) == Some("string")
}

fn single_string_enum(s: &Value) -> Option<&str> {
    if s.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    match s.get("enum")?.as_array()?.as_slice() {
        [one] => one.as_str(),
        _ => None,
    }
}

fn is_expandable(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Expandable(_) => true,
        TypeRef::Vec(inner) => is_expandable(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec_of(schemas: Value) -> Value {
        json!({ "components": { "schemas": schemas } })
    }

    fn strukt<'a>(ir: &'a Ir, name: &str) -> &'a StructDef {
        match ir.types.get(name) {
            Some(TypeDef::Struct(s)) => s,
            other => panic!("{name} is not a struct: {other:?}"),
        }
    }

    fn field<'a>(s: &'a StructDef, json: &str) -> &'a FieldDef {
        s.fields
            .iter()
            .find(|f| f.json == json)
            .unwrap_or_else(|| panic!("no field {json}"))
    }

    fn named(name: &str) -> TypeRef {
        TypeRef::Named(name.to_string())
    }

    #[test]
    fn nullable_required_and_optional_fields_map_to_the_four_shapes() {
        let spec = spec_of(json!({
            "thing": {
                "type": "object",
                "required": ["req", "req_null"],
                "properties": {
                    "req": { "type": "string" },
                    "req_null": { "type": "string", "nullable": true },
                    "opt": { "type": "integer" },
                    "opt_null": { "type": "boolean", "nullable": true }
                }
            }
        }));
        let ir = build_ir(&spec, &["thing"]).unwrap();
        let s = strukt(&ir, "Thing");
        let shapes: Vec<_> = s
            .fields
            .iter()
            .map(|f| (f.json.as_str(), f.ty.clone(), f.required, f.nullable))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("req", TypeRef::String, true, false),
                ("req_null", TypeRef::String, true, true),
                ("opt", TypeRef::I64, false, false),
                ("opt_null", TypeRef::Bool, false, true),
            ]
        );
    }

    #[test]
    fn expandable_field_to_root_is_expandable_and_to_non_root_is_id_only() {
        let spec = spec_of(json!({
            "parent": {
                "type": "object",
                "x-expandableFields": ["to_root", "to_other"],
                "properties": {
                    "to_root": {
                        "nullable": true,
                        "anyOf": [{ "type": "string", "maxLength": 5000 }, { "$ref": "#/components/schemas/target" }],
                        "x-expansionResources": { "oneOf": [{ "$ref": "#/components/schemas/target" }] }
                    },
                    "to_other": {
                        "anyOf": [{ "type": "string" }, { "$ref": "#/components/schemas/other" }]
                    }
                }
            },
            "target": { "type": "object", "properties": { "id": { "type": "string" } } },
            "other": { "type": "object", "properties": { "id": { "type": "string" } } }
        }));
        let ir = build_ir(&spec, &["parent", "target"]).unwrap();
        let p = strukt(&ir, "Parent");
        let to_root = field(p, "to_root");
        assert_eq!(to_root.ty, TypeRef::Expandable(Box::new(named("Target"))));
        assert!(to_root.nullable);
        assert_eq!(field(p, "to_other").ty, TypeRef::IdOnly);
        assert_eq!(p.expandable, vec!["to_root".to_string()]);
        assert!(!ir.types.contains_key("Other"));
    }

    #[test]
    fn expandable_union_keeps_only_root_targets() {
        let spec = spec_of(json!({
            "charge": {
                "type": "object",
                "x-expandableFields": ["customer"],
                "properties": {
                    "customer": {
                        "anyOf": [
                            { "type": "string" },
                            { "$ref": "#/components/schemas/customer" },
                            { "$ref": "#/components/schemas/deleted_customer" },
                            { "$ref": "#/components/schemas/not_a_root" }
                        ]
                    }
                }
            },
            "customer": { "type": "object", "properties": { "id": { "type": "string" } } },
            "deleted_customer": { "type": "object", "properties": { "id": { "type": "string" } } },
            "not_a_root": { "type": "object", "properties": { "id": { "type": "string" } } }
        }));
        let ir = build_ir(&spec, &["charge", "customer", "deleted_customer"]).unwrap();
        let c = strukt(&ir, "Charge");
        assert_eq!(
            field(c, "customer").ty,
            TypeRef::Expandable(Box::new(named("ChargeCustomer")))
        );
        assert_eq!(
            ir.types["ChargeCustomer"],
            TypeDef::Union(UnionDef {
                variants: vec![named("Customer"), named("DeletedCustomer")]
            })
        );
        assert!(!ir.types.contains_key("NotARoot"));
    }

    #[test]
    fn expandable_array_items_are_expandable() {
        let spec = spec_of(json!({
            "invoice": {
                "type": "object",
                "x-expandableFields": ["discounts", "tax_rates"],
                "properties": {
                    "discounts": {
                        "type": "array",
                        "items": { "anyOf": [{ "type": "string" }, { "$ref": "#/components/schemas/discount" }] }
                    },
                    "tax_rates": {
                        "type": "array",
                        "items": { "anyOf": [{ "type": "string" }, { "$ref": "#/components/schemas/tax_rate" }] }
                    }
                }
            },
            "discount": { "type": "object", "properties": { "id": { "type": "string" } } },
            "tax_rate": { "type": "object", "properties": { "id": { "type": "string" } } }
        }));
        let ir = build_ir(&spec, &["invoice", "discount"]).unwrap();
        let i = strukt(&ir, "Invoice");
        assert_eq!(
            field(i, "discounts").ty,
            TypeRef::Vec(Box::new(TypeRef::Expandable(Box::new(named("Discount")))))
        );
        assert_eq!(
            field(i, "tax_rates").ty,
            TypeRef::Vec(Box::new(TypeRef::IdOnly))
        );
        assert_eq!(i.expandable, vec!["discounts".to_string()]);
        assert!(!ir.types.contains_key("TaxRate"));
    }

    #[test]
    fn list_wrapper_becomes_list() {
        let spec = spec_of(json!({
            "customer": {
                "type": "object",
                "properties": {
                    "tax_ids": {
                        "type": "object",
                        "title": "TaxIDsList",
                        "required": ["data", "has_more", "object", "url"],
                        "properties": {
                            "data": { "type": "array", "items": { "$ref": "#/components/schemas/tax_id" } },
                            "has_more": { "type": "boolean" },
                            "object": { "type": "string", "enum": ["list"] },
                            "url": { "type": "string" }
                        }
                    }
                }
            },
            "tax_id": { "type": "object", "properties": { "id": { "type": "string" } } }
        }));
        let ir = build_ir(&spec, &["customer"]).unwrap();
        assert_eq!(
            field(strukt(&ir, "Customer"), "tax_ids").ty,
            TypeRef::List(Box::new(named("TaxId")))
        );
        assert!(!ir.types.contains_key("CustomerTaxIds"));
        assert!(ir.types.contains_key("TaxId"));
    }

    #[test]
    fn inline_object_gets_parent_field_name() {
        let spec = spec_of(json!({
            "checkout.session": {
                "type": "object",
                "required": ["object"],
                "properties": {
                    "object": { "type": "string", "enum": ["checkout.session"] },
                    "invoice_settings": {
                        "type": "object",
                        "properties": {
                            "footer": { "type": "string" },
                            "status": { "type": "string", "enum": ["open", "3d_secure", ""] }
                        }
                    },
                    "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                    "free": { "type": "object" },
                    "amount": { "type": "number" },
                    "when": { "type": "integer", "format": "unix-time" },
                    "qty": { "type": "string", "format": "decimal" }
                }
            }
        }));
        let ir = build_ir(&spec, &["checkout.session"]).unwrap();
        let s = strukt(&ir, "CheckoutSession");
        assert_eq!(s.schema, "checkout.session");
        assert_eq!(s.object.as_deref(), Some("checkout.session"));
        assert_eq!(field(s, "object").ty, named("CheckoutSessionObject"));
        assert_eq!(
            ir.types["CheckoutSessionObject"],
            TypeDef::Enum(EnumDef {
                values: vec!["checkout.session".to_string()]
            })
        );
        assert_eq!(
            field(s, "invoice_settings").ty,
            named("CheckoutSessionInvoiceSettings")
        );
        assert_eq!(
            field(s, "metadata").ty,
            TypeRef::Map(Box::new(TypeRef::String))
        );
        assert_eq!(field(s, "free").ty, TypeRef::Json);
        assert_eq!(field(s, "amount").ty, TypeRef::F64);
        assert_eq!(field(s, "when").ty, TypeRef::I64);
        assert_eq!(field(s, "qty").ty, TypeRef::String);
        let inline = strukt(&ir, "CheckoutSessionInvoiceSettings");
        assert_eq!(inline.schema, "checkout.session/invoice_settings");
        assert_eq!(inline.object, None);
        assert_eq!(
            field(inline, "status").ty,
            named("CheckoutSessionInvoiceSettingsStatus")
        );
        assert_eq!(
            ir.types["CheckoutSessionInvoiceSettingsStatus"],
            TypeDef::Enum(EnumDef {
                values: vec!["open".into(), "3d_secure".into(), "".into()]
            })
        );
    }

    #[test]
    fn colliding_names_are_an_error() {
        let spec = spec_of(json!({
            "foo": {
                "type": "object",
                "properties": {
                    "bar": { "type": "object", "properties": { "x": { "type": "string" } } },
                    "other": { "$ref": "#/components/schemas/foo_bar" }
                }
            },
            "foo_bar": { "type": "object", "properties": { "y": { "type": "string" } } }
        }));
        let err = build_ir(&spec, &["foo"]).unwrap_err();
        assert!(err.contains("FooBar"), "{err}");
        assert!(err.contains("`foo_bar`"), "{err}");
        assert!(err.contains("`foo/bar`"), "{err}");

        let dotted = spec_of(json!({
            "a.b": { "type": "object", "properties": { "x": { "type": "string" } } },
            "a_b": { "type": "object", "properties": { "x": { "type": "string" } } }
        }));
        let err = build_ir(&dotted, &["a.b", "a_b"]).unwrap_err();
        assert!(err.contains("`a.b`") && err.contains("`a_b`"), "{err}");
    }

    #[test]
    fn closure_follows_non_expandable_refs_only() {
        let spec = spec_of(json!({
            "root": {
                "type": "object",
                "x-expandableFields": ["exp"],
                "properties": {
                    "direct": { "$ref": "#/components/schemas/direct" },
                    "wrapped": { "nullable": true, "anyOf": [{ "$ref": "#/components/schemas/wrapped" }] },
                    "all_of": { "allOf": [{ "$ref": "#/components/schemas/all_of" }] },
                    "items": { "type": "array", "items": { "$ref": "#/components/schemas/item" } },
                    "map": { "type": "object", "additionalProperties": { "$ref": "#/components/schemas/value" } },
                    "exp": { "anyOf": [{ "type": "string" }, { "$ref": "#/components/schemas/expanded" }] },
                    "either": { "anyOf": [{ "$ref": "#/components/schemas/left" }, { "$ref": "#/components/schemas/right" }] }
                }
            },
            "direct": { "type": "object", "properties": { "next": { "$ref": "#/components/schemas/transitive" } } },
            "transitive": { "type": "object", "properties": { "id": { "type": "string" } } },
            "wrapped": { "type": "object", "properties": { "id": { "type": "string" } } },
            "all_of": { "type": "object", "properties": { "id": { "type": "string" } } },
            "item": { "type": "object", "properties": { "id": { "type": "string" } } },
            "value": { "type": "object", "properties": { "id": { "type": "string" } } },
            "expanded": { "type": "object", "properties": { "id": { "type": "string" } } },
            "left": { "type": "object", "properties": { "id": { "type": "string" } } },
            "right": { "type": "object", "properties": { "id": { "type": "string" } } },
            "unreferenced": { "type": "object", "properties": { "id": { "type": "string" } } }
        }));
        let ir = build_ir(&spec, &["root"]).unwrap();
        let mut named_schemas: Vec<&str> = ir
            .types
            .values()
            .filter_map(|t| match t {
                TypeDef::Struct(s) if !s.schema.contains('/') => Some(s.schema.as_str()),
                _ => None,
            })
            .collect();
        named_schemas.sort();
        assert_eq!(
            named_schemas,
            vec![
                "all_of",
                "direct",
                "item",
                "left",
                "right",
                "root",
                "transitive",
                "value",
                "wrapped"
            ]
        );
        let r = strukt(&ir, "Root");
        assert_eq!(field(r, "wrapped").ty, named("Wrapped"));
        assert!(field(r, "wrapped").nullable);
        assert_eq!(field(r, "all_of").ty, named("AllOf"));
        assert_eq!(field(r, "exp").ty, TypeRef::IdOnly);
        assert_eq!(field(r, "either").ty, named("RootEitherUnion"));
        assert_eq!(
            ir.types["RootEitherUnion"],
            TypeDef::Union(UnionDef {
                variants: vec![named("Left"), named("Right")]
            })
        );
    }

    #[test]
    fn phase_7_roots_resolve() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi/spec3.json");
        let bytes = std::fs::read(path).expect("vendored spec is missing");
        let spec: Value = serde_json::from_slice(&bytes).unwrap();
        let schemas = spec["components"]["schemas"].as_object().unwrap();
        for root in super::super::ROOTS {
            assert!(
                schemas.contains_key(*root),
                "root {root} is not in the spec"
            );
        }
        let ir = build_ir(&spec, super::super::ROOTS).unwrap();
        let named_schemas = ir
            .types
            .values()
            .filter(|t| matches!(t, TypeDef::Struct(s) if !s.schema.contains('/')))
            .count();
        assert_eq!(named_schemas, 754);
    }
}
