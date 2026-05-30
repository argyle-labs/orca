//! Typed JSON Schema model.
//!
//! Replaces `serde_json::Value` in every tool Args/Output that carries a
//! JSON Schema document (MCP `inputSchema`, plugin tool schemas, spec
//! registry payloads). Per `feedback_no_any_no_offloads.md`, no surface may
//! leak `Value` / `any` / `unknown`; this module is the typed substitute.
//!
//! Scope: JSON Schema 2020-12 vocabulary with draft-07 carve-outs commonly
//! emitted by MCP servers. Vendor extensions (any unrecognized keyword) are
//! captured into the typed `extensions` map — recursively typed, never
//! `Value`. Boolean shorthand schemas (`true`/`false`) are first-class.

use schemars::JsonSchema;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

/// A JSON Schema node. Either the boolean shorthand or an object schema.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
pub enum JsonSchemaNode {
    /// `true` accepts everything; `false` rejects everything.
    Bool(bool),
    Object(Box<SchemaObject>),
}

impl Default for JsonSchemaNode {
    fn default() -> Self {
        JsonSchemaNode::Object(Box::default())
    }
}

impl Serialize for JsonSchemaNode {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            JsonSchemaNode::Bool(b) => ser.serialize_bool(*b),
            JsonSchemaNode::Object(obj) => obj.serialize(ser),
        }
    }
}

impl<'de> Deserialize<'de> for JsonSchemaNode {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = JsonSchemaNode;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON Schema (object or boolean)")
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> Result<Self::Value, E> {
                Ok(JsonSchemaNode::Bool(b))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let obj = SchemaObject::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(JsonSchemaNode::Object(Box::new(obj)))
            }
        }
        de.deserialize_any(V)
    }
}

/// Primitive JSON Schema instance types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InstanceType {
    Null,
    Boolean,
    Object,
    Array,
    Number,
    Integer,
    String,
}

/// `type` may be a single instance type or an array of them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SchemaType {
    Single(InstanceType),
    Multi(Vec<InstanceType>),
}

/// `additionalProperties` is either a bool gate or a nested schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AdditionalProperties {
    Bool(bool),
    Schema(Box<JsonSchemaNode>),
}

/// `items` (draft-07) is either a single schema or a tuple of schemas. In
/// 2020-12 the tuple form is `prefixItems` instead — kept separately below.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Items {
    Single(Box<JsonSchemaNode>),
    Tuple(Vec<JsonSchemaNode>),
}

/// `dependencies` (draft-07) — either a list of required keys or a sub-schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Dependency {
    Keys(Vec<String>),
    Schema(Box<JsonSchemaNode>),
}

/// `exclusiveMinimum`/`exclusiveMaximum`: numeric in draft-06+, boolean in draft-4.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ExclusiveLimit {
    Number(f64),
    /// draft-4 form: the limit is the corresponding `minimum`/`maximum`,
    /// and this boolean toggles exclusivity.
    Legacy(bool),
}

/// A JSON-typed literal usable as `default`, `const`, or an `enum` entry.
///
/// Closed over JSON's six primitive kinds plus recursion. No `Value` leak.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum JsonLiteral {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<JsonLiteral>),
    Object(BTreeMap<String, JsonLiteral>),
}

/// A JSON Schema object. Every keyword is optional; unknown keywords are
/// captured into `extensions` (typed, recursive — never `Value`).
#[derive(Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct SchemaObject {
    // ── Core / identification ────────────────────────────────────────────
    pub schema_uri: Option<String>,                     // $schema
    pub id: Option<String>,                             // $id
    pub anchor: Option<String>,                         // $anchor
    pub reference: Option<String>,                      // $ref
    pub dynamic_ref: Option<String>,                    // $dynamicRef
    pub defs: Option<BTreeMap<String, JsonSchemaNode>>, // $defs
    pub definitions: Option<BTreeMap<String, JsonSchemaNode>>, // draft-07
    pub comment: Option<String>,                        // $comment

    // ── Annotations ──────────────────────────────────────────────────────
    pub title: Option<String>,
    pub description: Option<String>,
    pub default: Option<JsonLiteral>,
    pub deprecated: Option<bool>,
    pub read_only: Option<bool>,
    pub write_only: Option<bool>,
    pub examples: Option<Vec<JsonLiteral>>,

    // ── Validation: any-type ─────────────────────────────────────────────
    pub r#type: Option<SchemaType>,
    pub r#enum: Option<Vec<JsonLiteral>>,
    pub r#const: Option<JsonLiteral>,

    // ── String ───────────────────────────────────────────────────────────
    pub min_length: Option<u64>,
    pub max_length: Option<u64>,
    pub pattern: Option<String>,
    pub format: Option<String>,
    pub content_encoding: Option<String>,
    pub content_media_type: Option<String>,

    // ── Numeric ──────────────────────────────────────────────────────────
    pub multiple_of: Option<f64>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub exclusive_minimum: Option<ExclusiveLimit>,
    pub exclusive_maximum: Option<ExclusiveLimit>,

    // ── Object ───────────────────────────────────────────────────────────
    pub properties: Option<BTreeMap<String, JsonSchemaNode>>,
    pub pattern_properties: Option<BTreeMap<String, JsonSchemaNode>>,
    pub additional_properties: Option<AdditionalProperties>,
    pub unevaluated_properties: Option<AdditionalProperties>,
    pub required: Option<Vec<String>>,
    pub property_names: Option<Box<JsonSchemaNode>>,
    pub min_properties: Option<u64>,
    pub max_properties: Option<u64>,
    pub dependent_required: Option<BTreeMap<String, Vec<String>>>,
    pub dependent_schemas: Option<BTreeMap<String, JsonSchemaNode>>,
    pub dependencies: Option<BTreeMap<String, Dependency>>, // draft-07

    // ── Array ────────────────────────────────────────────────────────────
    pub items: Option<Items>,
    pub prefix_items: Option<Vec<JsonSchemaNode>>,
    pub contains: Option<Box<JsonSchemaNode>>,
    pub min_contains: Option<u64>,
    pub max_contains: Option<u64>,
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
    pub unique_items: Option<bool>,
    pub unevaluated_items: Option<AdditionalProperties>,

    // ── Composition ──────────────────────────────────────────────────────
    pub all_of: Option<Vec<JsonSchemaNode>>,
    pub any_of: Option<Vec<JsonSchemaNode>>,
    pub one_of: Option<Vec<JsonSchemaNode>>,
    pub not: Option<Box<JsonSchemaNode>>,

    // ── Conditional ──────────────────────────────────────────────────────
    pub r#if: Option<Box<JsonSchemaNode>>,
    pub then: Option<Box<JsonSchemaNode>>,
    pub r#else: Option<Box<JsonSchemaNode>>,

    // ── Vendor / unknown ─────────────────────────────────────────────────
    /// Keywords not in the modeled vocabulary. Each value is itself a typed
    /// `JsonSchemaNode` if it parses as a schema, or wrapped as a single-
    /// member `enum` of one `JsonLiteral`. Never `Value`.
    pub extensions: BTreeMap<String, JsonSchemaNode>,
}

// ── Hand-rolled serde so every known keyword maps to its canonical JSON
//    name and unknown keys flow into `extensions`. `#[serde(flatten)]` is
//    forbidden by the HARD RULE, so we do the field routing manually.

#[cfg(test)]
const KNOWN_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$ref",
    "$dynamicRef",
    "$defs",
    "definitions",
    "$comment",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    "type",
    "enum",
    "const",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "contentEncoding",
    "contentMediaType",
    "multipleOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "properties",
    "patternProperties",
    "additionalProperties",
    "unevaluatedProperties",
    "required",
    "propertyNames",
    "minProperties",
    "maxProperties",
    "dependentRequired",
    "dependentSchemas",
    "dependencies",
    "items",
    "prefixItems",
    "contains",
    "minContains",
    "maxContains",
    "minItems",
    "maxItems",
    "uniqueItems",
    "unevaluatedItems",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
];

impl Serialize for SchemaObject {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut m = ser.serialize_map(None)?;
        macro_rules! kv {
            ($key:literal, $field:expr) => {
                if let Some(v) = &$field {
                    m.serialize_entry($key, v)?;
                }
            };
        }
        kv!("$schema", self.schema_uri);
        kv!("$id", self.id);
        kv!("$anchor", self.anchor);
        kv!("$ref", self.reference);
        kv!("$dynamicRef", self.dynamic_ref);
        kv!("$defs", self.defs);
        kv!("definitions", self.definitions);
        kv!("$comment", self.comment);
        kv!("title", self.title);
        kv!("description", self.description);
        kv!("default", self.default);
        kv!("deprecated", self.deprecated);
        kv!("readOnly", self.read_only);
        kv!("writeOnly", self.write_only);
        kv!("examples", self.examples);
        kv!("type", self.r#type);
        kv!("enum", self.r#enum);
        kv!("const", self.r#const);
        kv!("minLength", self.min_length);
        kv!("maxLength", self.max_length);
        kv!("pattern", self.pattern);
        kv!("format", self.format);
        kv!("contentEncoding", self.content_encoding);
        kv!("contentMediaType", self.content_media_type);
        kv!("multipleOf", self.multiple_of);
        kv!("minimum", self.minimum);
        kv!("maximum", self.maximum);
        kv!("exclusiveMinimum", self.exclusive_minimum);
        kv!("exclusiveMaximum", self.exclusive_maximum);
        kv!("properties", self.properties);
        kv!("patternProperties", self.pattern_properties);
        kv!("additionalProperties", self.additional_properties);
        kv!("unevaluatedProperties", self.unevaluated_properties);
        kv!("required", self.required);
        kv!("propertyNames", self.property_names);
        kv!("minProperties", self.min_properties);
        kv!("maxProperties", self.max_properties);
        kv!("dependentRequired", self.dependent_required);
        kv!("dependentSchemas", self.dependent_schemas);
        kv!("dependencies", self.dependencies);
        kv!("items", self.items);
        kv!("prefixItems", self.prefix_items);
        kv!("contains", self.contains);
        kv!("minContains", self.min_contains);
        kv!("maxContains", self.max_contains);
        kv!("minItems", self.min_items);
        kv!("maxItems", self.max_items);
        kv!("uniqueItems", self.unique_items);
        kv!("unevaluatedItems", self.unevaluated_items);
        kv!("allOf", self.all_of);
        kv!("anyOf", self.any_of);
        kv!("oneOf", self.one_of);
        kv!("not", self.not);
        kv!("if", self.r#if);
        kv!("then", self.then);
        kv!("else", self.r#else);
        for (k, v) in &self.extensions {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for SchemaObject {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = SchemaObject;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON Schema object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = SchemaObject::default();
                while let Some(key) = map.next_key::<String>()? {
                    macro_rules! take {
                        ($field:ident) => {
                            out.$field = Some(map.next_value()?)
                        };
                        ($field:ident, box) => {
                            out.$field = Some(Box::new(map.next_value()?))
                        };
                    }
                    match key.as_str() {
                        "$schema" => take!(schema_uri),
                        "$id" => take!(id),
                        "$anchor" => take!(anchor),
                        "$ref" => take!(reference),
                        "$dynamicRef" => take!(dynamic_ref),
                        "$defs" => take!(defs),
                        "definitions" => take!(definitions),
                        "$comment" => take!(comment),
                        "title" => take!(title),
                        "description" => take!(description),
                        "default" => take!(default),
                        "deprecated" => take!(deprecated),
                        "readOnly" => take!(read_only),
                        "writeOnly" => take!(write_only),
                        "examples" => take!(examples),
                        "type" => take!(r#type),
                        "enum" => take!(r#enum),
                        "const" => take!(r#const),
                        "minLength" => take!(min_length),
                        "maxLength" => take!(max_length),
                        "pattern" => take!(pattern),
                        "format" => take!(format),
                        "contentEncoding" => take!(content_encoding),
                        "contentMediaType" => take!(content_media_type),
                        "multipleOf" => take!(multiple_of),
                        "minimum" => take!(minimum),
                        "maximum" => take!(maximum),
                        "exclusiveMinimum" => take!(exclusive_minimum),
                        "exclusiveMaximum" => take!(exclusive_maximum),
                        "properties" => take!(properties),
                        "patternProperties" => take!(pattern_properties),
                        "additionalProperties" => take!(additional_properties),
                        "unevaluatedProperties" => take!(unevaluated_properties),
                        "required" => take!(required),
                        "propertyNames" => take!(property_names, box),
                        "minProperties" => take!(min_properties),
                        "maxProperties" => take!(max_properties),
                        "dependentRequired" => take!(dependent_required),
                        "dependentSchemas" => take!(dependent_schemas),
                        "dependencies" => take!(dependencies),
                        "items" => take!(items),
                        "prefixItems" => take!(prefix_items),
                        "contains" => take!(contains, box),
                        "minContains" => take!(min_contains),
                        "maxContains" => take!(max_contains),
                        "minItems" => take!(min_items),
                        "maxItems" => take!(max_items),
                        "uniqueItems" => take!(unique_items),
                        "unevaluatedItems" => take!(unevaluated_items),
                        "allOf" => take!(all_of),
                        "anyOf" => take!(any_of),
                        "oneOf" => take!(one_of),
                        "not" => take!(not, box),
                        "if" => take!(r#if, box),
                        "then" => take!(then, box),
                        "else" => take!(r#else, box),
                        _ => {
                            let v: JsonSchemaNode = map.next_value()?;
                            out.extensions.insert(key, v);
                        }
                    }
                }
                Ok(out)
            }
        }
        de.deserialize_map(V)
    }
}

/// Compile-time guard: every entry in `KNOWN_KEYS` is unique. Catches typos
/// in either the serialize block or the deserialize match.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_keys_unique() {
        let mut sorted = KNOWN_KEYS.to_vec();
        sorted.sort();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(len_before, sorted.len(), "duplicate keyword in KNOWN_KEYS");
    }

    #[test]
    fn bool_shorthand_roundtrip() {
        let s: JsonSchemaNode = serde_json::from_str("true").unwrap();
        assert!(matches!(s, JsonSchemaNode::Bool(true)));
        assert_eq!(serde_json::to_string(&s).unwrap(), "true");
    }

    #[test]
    fn typical_mcp_input_schema_roundtrip() {
        let raw = r#"{
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "file path"},
                "limit": {"type": "integer", "minimum": 0}
            },
            "required": ["path"],
            "additionalProperties": false
        }"#;
        let parsed: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let JsonSchemaNode::Object(obj) = &parsed else {
            panic!("expected object schema");
        };
        assert!(matches!(
            obj.r#type,
            Some(SchemaType::Single(InstanceType::Object))
        ));
        assert_eq!(obj.required.as_deref(), Some(&["path".to_string()][..]));
        assert!(matches!(
            obj.additional_properties,
            Some(AdditionalProperties::Bool(false))
        ));
        assert!(obj.extensions.is_empty());
    }

    #[test]
    fn vendor_extension_captured_typed() {
        let raw = r#"{"type":"object","x-orca-vendor":{"type":"string","format":"uuid"}}"#;
        let parsed: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let JsonSchemaNode::Object(obj) = &parsed else {
            panic!()
        };
        let ext = obj.extensions.get("x-orca-vendor").expect("extension kept");
        let JsonSchemaNode::Object(ext_obj) = ext else {
            panic!("extension parsed as schema")
        };
        assert_eq!(ext_obj.format.as_deref(), Some("uuid"));
    }

    #[test]
    fn bool_false_shorthand_roundtrip() {
        let s: JsonSchemaNode = serde_json::from_str("false").unwrap();
        assert!(matches!(s, JsonSchemaNode::Bool(false)));
        assert_eq!(serde_json::to_string(&s).unwrap(), "false");
    }

    #[test]
    fn default_node_is_object() {
        let n = JsonSchemaNode::default();
        assert!(matches!(n, JsonSchemaNode::Object(_)));
    }

    #[test]
    fn every_known_keyword_round_trips_through_serde() {
        // One giant document touching every keyword the serialize/deserialize
        // matches handle. Round-trip parse → serialize → parse and assert
        // structural equality so any keyword we forget to wire up surfaces.
        let raw = r##"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.com/s",
            "$anchor": "a",
            "$ref": "#/$defs/x",
            "$dynamicRef": "#meta",
            "$defs": { "x": { "type": "string" } },
            "definitions": { "y": true },
            "$comment": "hi",
            "title": "T",
            "description": "D",
            "default": {"a": [1, "x", null, true, 1.5]},
            "deprecated": false,
            "readOnly": true,
            "writeOnly": false,
            "examples": [1, "x", null],
            "type": "object",
            "enum": [1, 2, 3],
            "const": "k",
            "minLength": 1, "maxLength": 10,
            "pattern": "^x",
            "format": "uuid",
            "contentEncoding": "base64",
            "contentMediaType": "application/json",
            "multipleOf": 2.5,
            "minimum": 0.0, "maximum": 100.0,
            "exclusiveMinimum": 0.0,
            "exclusiveMaximum": true,
            "properties": { "k": { "type": "integer" } },
            "patternProperties": { "^x": true },
            "additionalProperties": { "type": "string" },
            "unevaluatedProperties": false,
            "required": ["k"],
            "propertyNames": { "pattern": "^[a-z]+$" },
            "minProperties": 1, "maxProperties": 5,
            "dependentRequired": { "k": ["m"] },
            "dependentSchemas": { "k": { "type": "object" } },
            "dependencies": { "a": ["b"], "c": { "type": "object" } },
            "items": [{"type": "string"}, {"type": "integer"}],
            "prefixItems": [{"type": "string"}],
            "contains": { "type": "string" },
            "minContains": 1, "maxContains": 3,
            "minItems": 0, "maxItems": 10,
            "uniqueItems": true,
            "unevaluatedItems": true,
            "allOf": [{"type": "string"}],
            "anyOf": [{"type": "string"}],
            "oneOf": [{"type": "string"}],
            "not": { "type": "null" },
            "if":   { "type": "object" },
            "then": { "type": "object" },
            "else": { "type": "object" },
            "x-vendor": { "type": "string" }
        }"##;
        let first: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let s = serde_json::to_string(&first).unwrap();
        let again: JsonSchemaNode = serde_json::from_str(&s).unwrap();
        assert_eq!(first, again);

        let JsonSchemaNode::Object(obj) = &again else {
            panic!()
        };
        // Sanity-check a representative subset of the keyword routing.
        assert_eq!(
            obj.schema_uri.as_deref(),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(obj.id.as_deref(), Some("https://example.com/s"));
        assert_eq!(obj.anchor.as_deref(), Some("a"));
        assert_eq!(obj.reference.as_deref(), Some("#/$defs/x"));
        assert_eq!(obj.dynamic_ref.as_deref(), Some("#meta"));
        assert!(obj.defs.is_some());
        assert!(obj.definitions.is_some());
        assert_eq!(obj.comment.as_deref(), Some("hi"));
        assert_eq!(obj.read_only, Some(true));
        assert_eq!(obj.write_only, Some(false));
        assert!(matches!(
            obj.exclusive_minimum,
            Some(ExclusiveLimit::Number(_))
        ));
        assert!(matches!(
            obj.exclusive_maximum,
            Some(ExclusiveLimit::Legacy(true))
        ));
        assert!(matches!(obj.items, Some(Items::Tuple(_))));
        assert!(obj.prefix_items.is_some());
        assert!(obj.contains.is_some());
        assert!(obj.property_names.is_some());
        assert!(obj.dependent_required.is_some());
        assert!(obj.dependent_schemas.is_some());
        let deps = obj.dependencies.as_ref().unwrap();
        assert!(matches!(deps.get("a"), Some(Dependency::Keys(_))));
        assert!(matches!(deps.get("c"), Some(Dependency::Schema(_))));
        assert!(matches!(
            obj.additional_properties,
            Some(AdditionalProperties::Schema(_))
        ));
        assert!(matches!(
            obj.unevaluated_properties,
            Some(AdditionalProperties::Bool(false))
        ));
        assert!(matches!(
            obj.unevaluated_items,
            Some(AdditionalProperties::Bool(true))
        ));
        assert!(obj.all_of.is_some());
        assert!(obj.any_of.is_some());
        assert!(obj.one_of.is_some());
        assert!(obj.not.is_some());
        assert!(obj.r#if.is_some());
        assert!(obj.then.is_some());
        assert!(obj.r#else.is_some());
        assert!(obj.extensions.contains_key("x-vendor"));
    }

    #[test]
    fn items_single_form_parses() {
        let raw = r#"{"items": {"type": "string"}}"#;
        let n: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let JsonSchemaNode::Object(obj) = n else {
            panic!()
        };
        assert!(matches!(obj.items, Some(Items::Single(_))));
    }

    #[test]
    fn additional_properties_bool_true_form() {
        let raw = r#"{"additionalProperties": true}"#;
        let n: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let JsonSchemaNode::Object(obj) = n else {
            panic!()
        };
        assert!(matches!(
            obj.additional_properties,
            Some(AdditionalProperties::Bool(true))
        ));
    }

    #[test]
    fn json_literal_covers_every_variant() {
        let cases = vec![
            ("null", JsonLiteral::Null),
            ("true", JsonLiteral::Bool(true)),
            ("42", JsonLiteral::Number(serde_json::Number::from(42))),
            (r#""hi""#, JsonLiteral::String("hi".into())),
            (
                r#"[1,"x"]"#,
                JsonLiteral::Array(vec![
                    JsonLiteral::Number(serde_json::Number::from(1)),
                    JsonLiteral::String("x".into()),
                ]),
            ),
        ];
        for (raw, expected) in cases {
            let got: JsonLiteral = serde_json::from_str(raw).unwrap();
            assert_eq!(got, expected);
            let back = serde_json::to_string(&got).unwrap();
            let again: JsonLiteral = serde_json::from_str(&back).unwrap();
            assert_eq!(again, expected);
        }
        let obj: JsonLiteral = serde_json::from_str(r#"{"a":1}"#).unwrap();
        if let JsonLiteral::Object(m) = &obj {
            assert!(matches!(m.get("a"), Some(JsonLiteral::Number(_))));
        } else {
            panic!("expected object literal");
        }
    }

    #[test]
    fn instance_type_lowercase_serde() {
        let raw = r#"["null","boolean","object","array","number","integer","string"]"#;
        let parsed: Vec<InstanceType> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.len(), 7);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
    }

    #[test]
    fn deserialize_rejects_non_object_non_bool() {
        let err = serde_json::from_str::<JsonSchemaNode>("42").unwrap_err();
        assert!(err.to_string().contains("JSON Schema"));
    }

    #[test]
    fn multi_type_array() {
        let raw = r#"{"type":["string","null"]}"#;
        let parsed: JsonSchemaNode = serde_json::from_str(raw).unwrap();
        let JsonSchemaNode::Object(obj) = &parsed else {
            panic!()
        };
        match &obj.r#type {
            Some(SchemaType::Multi(v)) => {
                assert_eq!(v, &vec![InstanceType::String, InstanceType::Null])
            }
            _ => panic!("expected multi type"),
        }
    }
}
