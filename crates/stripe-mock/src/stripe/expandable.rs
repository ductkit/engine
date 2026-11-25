//! Expandable references: an ID, or the expanded object.

use serde::{Deserialize, Serialize};

/// A field that holds an object's ID, or the object itself when the request expanded it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Expandable<T> {
    Id(String),
    Object(Box<T>),
}

/// An object with an `id`.
pub trait HasId {
    fn id(&self) -> &str;
}

impl<T: HasId> Expandable<T> {
    /// The referenced object's ID, expanded or not.
    pub fn id(&self) -> &str {
        match self {
            Expandable::Id(id) => id,
            Expandable::Object(object) => object.id(),
        }
    }
}
