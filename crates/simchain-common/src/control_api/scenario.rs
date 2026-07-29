use serde::{Deserialize, Serialize};

/// One field of a scenario step or nested object.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioFieldSchema {
    pub name: String,
    /// `integer`, `float`, `boolean`, `string`, `amount`, `choice`,
    /// `string_map`, `object`, or `object_list`.
    pub value_type: String,
    /// Accepted values when `value_type` is `choice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    /// Name of the entry in `objects` describing this field's shape, when
    /// `value_type` is `object` or `object_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// `required`, `optional`, `exactly_one_of`, `at_least_one_of`, or
    /// `required_when`.
    pub requirement: String,
    /// Group label for `exactly_one_of` and `at_least_one_of`; the governing
    /// condition for `required_when`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_group: Option<String>,
    /// Value applied when the field is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub help: String,
}

/// One variant of a tagged nested object.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioVariantSchema {
    /// Value of the parent object's discriminator key.
    pub tag_value: String,
    pub summary: String,
    pub fields: Vec<ScenarioFieldSchema>,
}

/// A nested object reachable from a step field.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioObjectSchema {
    pub name: String,
    pub summary: String,
    /// Discriminator key for a tagged union.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ScenarioVariantSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ScenarioFieldSchema>,
}

/// One step type of the scenario language.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioStepSchema {
    /// Value of the step's `type` key.
    pub kind: String,
    pub summary: String,
    /// `control-plane` or `network-agents`.
    pub requires: String,
    /// Smallest Compose profile that can run the step.
    pub minimum_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ScenarioFieldSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The complete, self-describing scenario language contract.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ScenarioSchemaResponse {
    /// Only value accepted by a scenario file's `version` key.
    pub version: u64,
    /// Height every scenario waits for before its first step.
    pub bootstrap_height: u64,
    pub steps: Vec<ScenarioStepSchema>,
    pub objects: Vec<ScenarioObjectSchema>,
}
