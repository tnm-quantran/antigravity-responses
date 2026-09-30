use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

pub fn translate(schema: &Value, uppercase: bool) -> Result<Value> {
    rewrite(schema, schema, uppercase, &mut Vec::new())
}

fn rewrite(
    schema: &Value,
    root: &Value,
    uppercase: bool,
    references: &mut Vec<String>,
) -> Result<Value> {
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
        let result = rewrite(&resolved, root, uppercase, references);
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
                        Ok((name.clone(), rewrite(schema, root, uppercase, references)?))
                    })
                    .collect();
                result.insert(key.clone(), Value::Object(rewritten?));
            }
            "items" => {
                result.insert(key.clone(), rewrite(value, root, uppercase, references)?);
            }
            "anyOf" => {
                let values: Result<Vec<Value>> = value
                    .as_array()
                    .context("anyOf must be an array")?
                    .iter()
                    .map(|schema| rewrite(schema, root, uppercase, references))
                    .collect();
                result.insert(key.clone(), json!(values?));
            }
            "required" | "description" | "enum" | "format" | "minimum" | "maximum" | "minItems"
            | "maxItems" | "minLength" | "maxLength" | "pattern" | "nullable" | "title" => {
                result.insert(key.clone(), value.clone());
            }
            "default" | "examples" => {}
            unsupported => bail!("unsupported schema keyword: {unsupported}"),
        }
    }
    Ok(Value::Object(result))
}
