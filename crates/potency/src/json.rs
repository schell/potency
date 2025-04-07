//! Serde JSON storage values.
use snafu::prelude::*;

use crate::{DeserializeFrom, SerializeTo};

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("Error serialize: {source}"))]
    Serialize { source: serde_json::Error },

    #[snafu(display("Error deserializing: {source}"))]
    Deserialize { source: serde_json::Error },
}

/// A wrapper around `T` that allows us to write [`DeserializeFrom`] and
/// [`SerializeTo`] instances.
#[repr(transparent)]
#[derive(Debug, PartialEq)]
pub struct JsonDeserialized<T>(pub T);

impl<T> From<T> for JsonDeserialized<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

/// A wrapper around [`serde_json::Value`] that allows us to write [`DeserializeFrom`] and
/// [`SerializeTo`] instances.
#[repr(transparent)]
#[derive(Debug, PartialEq)]
pub struct JsonSerialized(pub serde_json::Value);

impl From<serde_json::Value> for JsonSerialized {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl<T: serde::de::DeserializeOwned, E: From<Error>> DeserializeFrom<JsonSerialized, E>
    for JsonDeserialized<T>
{
    fn try_from_store_value(stored: JsonSerialized) -> Result<Self, E> {
        let value = serde_json::from_value(stored.0).context(DeserializeSnafu)?;
        Ok(Self(value))
    }
}

impl<T: Clone + serde::Serialize, E: From<Error>> SerializeTo<JsonSerialized, E>
    for JsonDeserialized<T>
{
    fn try_into_store_value(&self) -> Result<JsonSerialized, E> {
        let value = serde_json::to_value(self.0.clone()).context(SerializeSnafu)?;
        Ok(JsonSerialized(value))
    }
}
