pub mod parse;
pub mod resolve;

use indexmap::IndexMap;

/// A node in the JSON Schema AST. Faithfully represents JSON Schema structure.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct SchemaNode {
    pub title: Option<String>,
    pub description: Option<String>,
    pub deprecated: bool,
    pub deprecation_message: Option<String>,
    pub schema_type: Option<SchemaType>,
    pub properties: IndexMap<String, Self>,
    pub required: Vec<String>,
    pub additional_properties: Option<AdditionalProperties>,
    pub pattern_properties: IndexMap<String, Self>,
    pub items: Option<Box<Self>>,
    pub one_of: Vec<Self>,
    pub all_of: Vec<Self>,
    pub any_of: Vec<Self>,
    pub enum_values: Vec<serde_json::Value>,
    pub r#ref: Option<String>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub pattern: Option<String>,
    pub format: Option<String>,
    pub default_value: Option<serde_json::Value>,
    pub unevaluated_properties: Option<bool>,
    pub definitions: IndexMap<String, Self>,
}

#[derive(Debug, Clone)]
pub enum SchemaType {
    Single(PrimitiveType),
    Multi(Vec<PrimitiveType>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
    Null,
}

#[derive(Debug, Clone)]
pub enum AdditionalProperties {
    Bool(bool),
    Schema(Box<SchemaNode>),
}

impl SchemaNode {
    /// Returns true if this node has `additionalProperties: false`.
    pub const fn denies_additional(&self) -> bool {
        matches!(
            self.additional_properties,
            Some(AdditionalProperties::Bool(false))
        )
    }

    /// Returns true if this node is a simple object (has properties, no composition keywords).
    #[allow(dead_code)]
    pub fn is_simple_object(&self) -> bool {
        !self.properties.is_empty()
            && self.one_of.is_empty()
            && self.all_of.is_empty()
            && self.any_of.is_empty()
    }
}
