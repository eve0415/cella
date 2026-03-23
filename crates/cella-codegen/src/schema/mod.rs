pub mod parse;

use indexmap::IndexMap;

/// A node in the JSON Schema AST. Faithfully represents JSON Schema structure.
#[derive(Debug, Clone, Default)]
pub struct SchemaNode {
    pub description: Option<String>,
    pub deprecated: bool,
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
}
