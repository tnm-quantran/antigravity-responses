use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

const GOOGLE_TOOL_SCHEMA_KEYS: &[&str] = &[
    "type",
    "nullable",
    "required",
    "format",
    "description",
    "properties",
    "items",
    "enum",
    "anyOf",
    "$ref",
    "$defs",
    "definitions",
    "pattern",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
];
const GOOGLE_TOOL_SCHEMA_ANNOTATIONS: &[&str] = &[
    "title",
    "default",
    "examples",
    "example",
    "$comment",
    "$schema",
    "$id",
    "deprecated",
    "readOnly",
    "writeOnly",
];
const GOOGLE_UNSUPPORTED_CONSTRAINTS: &[&str] = &[
    "allOf",
    "oneOf",
    "not",
    "const",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "additionalProperties",
    "additionalItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
    "patternProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "if",
    "then",
    "else",
    "prefixItems",
    "$dynamicRef",
    "$recursiveRef",
];
const MAX_SCHEMA_DEPTH: usize = 24;
const MAX_SCHEMA_NODES: usize = 1_024;
const MAX_SCHEMA_BYTES: usize = 256 * 1024;
type SchemaWarning = (String, Vec<String>);

pub fn translate(schema: &Value, uppercase: bool) -> Result<Value> {
    let mut validation_nodes = MAX_SCHEMA_NODES;
    validate_schema(schema, 0, &mut validation_nodes)?;
    ensure!(
        serde_json::to_vec(schema)?.len() <= MAX_SCHEMA_BYTES,
        "tool schema exceeds byte limit"
    );
    let mut expansion_nodes = MAX_SCHEMA_NODES;
    rewrite(
        schema,
        schema,
        uppercase,
        &mut Vec::new(),
        0,
        &mut expansion_nodes,
    )
}

pub fn translate_tool(schema: &Value, uppercase: bool) -> Result<Value> {
    let (translated, dropped) = translate_tool_with_report(schema, uppercase)?;
    warn_tool_schema("<unnamed>", &dropped)?;
    Ok(translated)
}

pub(crate) fn warn_tool_schema(tool: &str, dropped: &[String]) -> Result<()> {
    if dropped.is_empty() {
        return Ok(());
    }
    static WARNED: OnceLock<Mutex<HashSet<SchemaWarning>>> = OnceLock::new();
    let should_warn = record_schema_warning(
        &mut *WARNED
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("schema warning cache lock poisoned"))?,
        (tool.to_owned(), dropped.to_vec()),
    );
    if should_warn {
        eprintln!(
            "Google tool {tool} schema omitted unsupported constraints: {}",
            dropped.join(", ")
        );
    }
    Ok(())
}

fn record_schema_warning(warned: &mut HashSet<SchemaWarning>, warning: SchemaWarning) -> bool {
    if warned.contains(&warning) {
        return false;
    }
    // ponytail: cache 1024 warning groups; beyond that, log uncached groups normally.
    if warned.len() < 1_024 {
        warned.insert(warning);
    }
    true
}

pub fn translate_tool_with_report(schema: &Value, uppercase: bool) -> Result<(Value, Vec<String>)> {
    let mut schema = schema.clone();
    let mut validation_nodes = MAX_SCHEMA_NODES;
    validate_schema(&schema, 0, &mut validation_nodes)?;
    ensure!(
        serde_json::to_vec(&schema)?.len() <= MAX_SCHEMA_BYTES,
        "tool schema exceeds byte limit"
    );
    let mut dropped = Vec::new();
    clean_tool_schema(&mut schema, &mut dropped)?;
    dropped.sort_unstable();
    dropped.dedup();
    Ok((translate(&schema, uppercase)?, dropped))
}

fn validate_schema(value: &Value, depth: usize, remaining: &mut usize) -> Result<()> {
    ensure!(depth <= MAX_SCHEMA_DEPTH, "tool schema exceeds depth limit");
    ensure!(*remaining > 0, "tool schema exceeds node limit");
    *remaining -= 1;
    match value {
        Value::Array(values) => {
            for value in values {
                validate_schema(value, depth + 1, remaining)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_schema(value, depth + 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn clean_tool_schema(value: &mut Value, dropped: &mut Vec<String>) -> Result<()> {
    let Some(map) = value.as_object_mut() else {
        bail!("tool schema must be an object");
    };
    if map.get("const").is_some_and(Value::is_string) {
        let constant = map.remove("const").context("missing const")?;
        ensure!(
            map.get("type").is_none_or(|kind| kind == "string"
                || kind
                    .as_array()
                    .is_some_and(|types| types.contains(&json!("string")))),
            "const conflicts with type"
        );
        if let Some(values) = map.get("enum") {
            ensure!(
                values
                    .as_array()
                    .is_some_and(|values| values.contains(&constant)),
                "const conflicts with enum"
            );
        }
        map.insert("enum".into(), json!([constant]));
        map.insert("type".into(), json!("string"));
        map.remove("nullable");
    }
    for key in map.keys() {
        if GOOGLE_UNSUPPORTED_CONSTRAINTS.contains(&key.as_str()) {
            dropped.push(key.clone());
            continue;
        }
        ensure!(
            GOOGLE_TOOL_SCHEMA_KEYS.contains(&key.as_str())
                || GOOGLE_TOOL_SCHEMA_ANNOTATIONS.contains(&key.as_str()),
            "unsupported tool schema keyword: {key}"
        );
    }
    map.retain(|key, _| {
        GOOGLE_TOOL_SCHEMA_KEYS.contains(&key.as_str())
            && !GOOGLE_UNSUPPORTED_CONSTRAINTS.contains(&key.as_str())
    });
    for key in ["properties", "$defs", "definitions"] {
        if let Some(properties) = map.get_mut(key).and_then(Value::as_object_mut) {
            for schema in properties.values_mut() {
                clean_tool_schema(schema, dropped)?;
            }
        }
    }
    for key in ["items"] {
        if let Some(schema) = map.get_mut(key) {
            clean_tool_schema(schema, dropped)?;
        }
    }
    if let Some(variants) = map.get_mut("anyOf").and_then(Value::as_array_mut) {
        for schema in variants {
            clean_tool_schema(schema, dropped)?;
        }
    }
    Ok(())
}

fn rewrite(
    schema: &Value,
    root: &Value,
    uppercase: bool,
    references: &mut Vec<String>,
    depth: usize,
    remaining: &mut usize,
) -> Result<Value> {
    ensure!(
        depth <= MAX_SCHEMA_DEPTH,
        "tool schema exceeds expanded depth limit"
    );
    ensure!(*remaining > 0, "tool schema exceeds expanded node limit");
    *remaining -= 1;
    let map = schema
        .as_object()
        .context("tool schema must be an object")?;
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        ensure!(
            references.len() < 32 && !references.iter().any(|seen| seen == reference),
            "cyclic or excessively nested tool schema reference"
        );
        let pointer = reference
            .strip_prefix('#')
            .context("external schema references unsupported")?;
        let mut resolved = root
            .pointer(pointer)
            .context("unresolved schema reference")?
            .clone();
        let resolved_map = resolved
            .as_object_mut()
            .context("schema reference must resolve to an object")?;
        for (key, value) in map {
            if key != "$ref" {
                resolved_map.insert(key.clone(), value.clone());
            }
        }
        references.push(reference.to_owned());
        let result = rewrite(&resolved, root, uppercase, references, depth + 1, remaining);
        references.pop();
        return result;
    }
    let mut result = Map::new();
    for (key, value) in map {
        match key.as_str() {
            "$defs" | "definitions" | "$schema" | "$id" | "$comment" | "additionalProperties" => {}
            "type" => {
                let kind = match value {
                    Value::String(kind) => json!(if uppercase {
                        kind.to_ascii_uppercase()
                    } else {
                        kind.clone()
                    }),
                    Value::Array(types)
                        if types.len() == 2 && types.iter().any(|kind| kind == "null") =>
                    {
                        result.insert("nullable".into(), json!(true));
                        let kind = types
                            .iter()
                            .find_map(|kind| kind.as_str().filter(|kind| *kind != "null"))
                            .context("invalid nullable type")?;
                        json!(if uppercase {
                            kind.to_ascii_uppercase()
                        } else {
                            kind.to_owned()
                        })
                    }
                    _ => bail!("unsupported schema type union"),
                };
                result.insert(key.clone(), kind);
            }
            "properties" => {
                let properties = value
                    .as_object()
                    .context("schema properties must be an object")?;
                let rewritten: Result<Map<String, Value>> = properties
                    .iter()
                    .map(|(name, schema)| {
                        Ok((
                            name.clone(),
                            rewrite(schema, root, uppercase, references, depth + 1, remaining)?,
                        ))
                    })
                    .collect();
                result.insert(key.clone(), Value::Object(rewritten?));
            }
            "items" => {
                result.insert(
                    key.clone(),
                    rewrite(value, root, uppercase, references, depth + 1, remaining)?,
                );
            }
            "anyOf" => {
                let values: Result<Vec<Value>> = value
                    .as_array()
                    .context("anyOf must be an array")?
                    .iter()
                    .map(|schema| {
                        rewrite(schema, root, uppercase, references, depth + 1, remaining)
                    })
                    .collect();
                result.insert(key.clone(), json!(values?));
            }
            "required" | "description" | "enum" | "format" | "minimum" | "maximum" | "minItems"
            | "maxItems" | "minLength" | "maxLength" | "minProperties" | "maxProperties"
            | "pattern" | "nullable" | "title" => {
                result.insert(key.clone(), value.clone());
            }
            "default" | "examples" => {}
            unsupported => bail!("unsupported schema keyword: {unsupported}"),
        }
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_warnings_are_deduplicated_without_hiding_new_groups() {
        let mut warned = HashSet::new();
        let warning = ("search".into(), vec!["additionalProperties".into()]);
        assert!(record_schema_warning(&mut warned, warning.clone()));
        assert!(!record_schema_warning(&mut warned, warning.clone()));
        assert!(record_schema_warning(
            &mut warned,
            ("search".into(), vec!["oneOf".into()])
        ));
        for index in 0..1_024 {
            record_schema_warning(&mut warned, (format!("tool-{index}"), vec![]));
        }
        assert_eq!(warned.len(), 1_024);
        assert!(!record_schema_warning(&mut warned, warning));
        assert!(record_schema_warning(
            &mut warned,
            ("uncached".into(), vec!["additionalProperties".into()])
        ));
        assert_eq!(warned.len(), 1_024);
    }

    #[test]
    fn repeated_translation_still_rejects_lossy_schemas() {
        let request = json!({"model":"gemini-test","input":"inspect","tools":[{
            "type":"function","name":"search","parameters":{
                "type":"object","properties":{},"additionalProperties":false
            }
        }]});
        let mut replay = crate::protocol::Replay::new(4096, std::time::Duration::from_secs(60));
        for _ in 0..2 {
            assert!(crate::protocol::translate_with_tools(&request, &mut replay, false).is_ok());
            assert!(crate::protocol::translate_with_tools(&request, &mut replay, true).is_err());
        }
    }
}
