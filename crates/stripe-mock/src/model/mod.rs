//! Stripe object types and their JSON models.

use serde::{Deserialize, Serialize};

pub mod customer;

/// A simulated Stripe object type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Customer,
}

impl ObjectType {
    /// The Stripe `object` name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectType::Customer => "customer",
        }
    }

    /// The ID prefix, without the underscore.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            ObjectType::Customer => "cus",
        }
    }
}

impl std::fmt::Display for ObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
