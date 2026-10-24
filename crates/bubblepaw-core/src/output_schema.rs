use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A named JSON schema for provider-native structured output.
/// This checks closed object schemas, not each provider's full schema subset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonSchema {
    pub name: String,
    pub schema: Value,
}
impl JsonSchema {
    pub fn new(name: impl Into<String>, schema: Value) -> Result<Self> {
        let schema = Self {
            name: name.into(),
            schema,
        };
        schema.validate()?;
        Ok(schema)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && self
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "output schema name must be 1-64 ASCII letters, digits, underscores or dashes"
        );
        ensure!(
            self.schema.is_object(),
            "output schema must be a JSON schema object"
        );
        validate_closed_objects(&self.schema, "$")
    }
}

fn validate_closed_objects(schema: &Value, path: &str) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    let is_object = schema["type"] == "object"
        || schema["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|kind| kind == "object"))
        || object.contains_key("properties")
        || object.contains_key("patternProperties")
        || object.contains_key("additionalProperties");
    if is_object {
        ensure!(
            schema["additionalProperties"] == false,
            "output schema object at {path} must set additionalProperties: false"
        );
    }
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = schema[keyword].as_object() {
            for (name, child) in children {
                validate_closed_objects(child, &format!("{path}/{keyword}/{name}"))?;
            }
        }
    }
    for keyword in [
        "items",
        "additionalItems",
        "unevaluatedItems",
        "contains",
        "contentSchema",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedProperties",
        "additionalProperties",
    ] {
        if let Some(child) = object.get(keyword) {
            if let Some(children) = child.as_array() {
                for (index, child) in children.iter().enumerate() {
                    validate_closed_objects(child, &format!("{path}/{keyword}/{index}"))?;
                }
            } else {
                validate_closed_objects(child, &format!("{path}/{keyword}"))?;
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = schema[keyword].as_array() {
            for (index, child) in children.iter().enumerate() {
                validate_closed_objects(child, &format!("{path}/{keyword}/{index}"))?;
            }
        }
    }
    Ok(())
}
