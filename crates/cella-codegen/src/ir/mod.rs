pub mod lower;
pub mod naming;

/// A Rust-oriented type in the intermediate representation.
#[derive(Debug, Clone)]
pub enum IrType {
    Struct(IrStruct),
    Enum(IrEnum),
    Alias(IrAlias),
}

impl IrType {
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            Self::Struct(s) => &s.name,
            Self::Enum(e) => &e.name,
            Self::Alias(a) => &a.name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IrStruct {
    pub name: String,
    pub doc: Option<String>,
    pub fields: Vec<IrField>,
    pub deny_unknown_fields: bool,
    /// If true, this struct represents an `allOf` composition.
    /// Each field validates the same JSON object (not individual properties).
    pub is_all_of: bool,
}

#[derive(Debug, Clone)]
pub struct IrField {
    pub name: String,
    pub json_name: String,
    pub doc: Option<String>,
    pub ty: IrTypeRef,
    pub required: bool,
    pub deprecated: bool,
}

#[derive(Debug, Clone)]
pub struct IrEnum {
    pub name: String,
    pub doc: Option<String>,
    pub variants: Vec<IrVariant>,
    pub repr: EnumRepr,
}

#[derive(Debug, Clone)]
pub enum EnumRepr {
    /// String values: `enum: ["a", "b"]`
    StringEnum,
    /// Typed variants from `oneOf`/`anyOf` — try each variant's validator.
    TypedVariants,
    /// Multi-type: `type: ["string", "array"]` — dispatch on JSON type.
    MultiType,
    /// Mixed enum with booleans and strings (e.g., `gpu`).
    BoolMixed,
}

#[derive(Debug, Clone)]
pub struct IrVariant {
    pub name: String,
    pub doc: Option<String>,
    /// For string enums: the JSON string value.
    pub json_value: Option<serde_json::Value>,
    /// For typed variants: the inner type.
    pub ty: Option<IrTypeRef>,
}

#[derive(Debug, Clone)]
pub struct IrAlias {
    pub name: String,
    pub doc: Option<String>,
    pub ty: IrTypeRef,
}

/// A reference to a type used in fields, variants, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum IrTypeRef {
    String,
    I64,
    F64,
    Bool,
    Vec(Box<Self>),
    /// Nullable value (JSON null → None).
    Option(Box<Self>),
    Map(Box<Self>, Box<Self>),
    /// Reference to a named generated type.
    Named(std::string::String),
    /// Fallback: `serde_json::Value`.
    Value,
}
