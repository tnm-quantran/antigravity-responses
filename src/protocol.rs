use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: String,
    pub execution: Option<Value>,
}

pub struct Replay {
    entries: VecDeque<(String, Value, Instant, usize)>,
    bytes: usize,
    limit: usize,
    ttl: Duration,
}

impl Replay {
    pub fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            ttl,
        }
    }

    pub fn insert(&mut self, key: String, part: Value) -> Result<()> {
        let bytes = key.len() + serde_json::to_vec(&part)?.len();
        ensure!(
            bytes <= self.limit,
            "provider part exceeds replay state limit"
        );
        self.prune();
        while self.bytes + bytes > self.limit || self.entries.len() >= 4096 {
            if let Some((_, _, _, removed)) = self.entries.pop_front() {
                self.bytes -= removed;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((key, part, Instant::now(), bytes));
        Ok(())
    }

    pub fn get(&mut self, key: &str) -> Result<Value> {
        self.prune();
        self.entries
            .iter()
            .rev()
            .find(|(id, _, _, _)| id == key)
            .map(|(_, part, _, _)| part.clone())
            .context("provider replay state expired or missing; start a new conversation")
    }

    fn prune(&mut self) {
        while self
            .entries
            .front()
            .is_some_and(|(_, _, time, _)| time.elapsed() >= self.ttl)
        {
            if let Some((_, _, _, bytes)) = self.entries.pop_front() {
                self.bytes -= bytes;
            }
        }
    }
}

pub fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .with_context(|| format!("missing or invalid {field}"))
}

pub fn tools(request: &Value) -> Result<(Vec<Value>, HashMap<String, Tool>)> {
    let mut declarations = Vec::new();
    let mut registry = HashMap::new();
    if let Some(specs) = request.get("tools") {
        for spec in specs.as_array().context("tools must be an array")? {
            if spec["type"] == "namespace" {
                let namespace = required_string(spec, "name")?;
                for tool in spec["tools"]
                    .as_array()
                    .context("namespace tools must be an array")?
                {
                    declare_tool(tool, Some(namespace), &mut declarations, &mut registry)?;
                }
            } else {
                declare_tool(spec, None, &mut declarations, &mut registry)?;
            }
        }
    }
    let uppercase = !request["model"]
        .as_str()
        .unwrap_or_default()
        .contains("claude");
    for declaration in &mut declarations {
        declaration["parameters"] =
            crate::schema::translate(&declaration["parameters"], uppercase)?;
    }
    Ok((declarations, registry))
}

fn declare_tool(
    spec: &Value,
    namespace: Option<&str>,
    declarations: &mut Vec<Value>,
    registry: &mut HashMap<String, Tool>,
) -> Result<()> {
    let kind = required_string(spec, "type")?;
    ensure!(
        matches!(kind, "function" | "custom" | "tool_search"),
        "unsupported tool type: {kind}"
    );
    let name = if kind == "tool_search" {
        "tool_search"
    } else {
        required_string(spec, "name")?
    };
    // Match Codex's flat-tool boundary, where namespace prefixes include their separator.
    let flat = namespace
        .filter(|namespace| *namespace != "functions")
        .map_or_else(|| name.to_owned(), |namespace| format!("{namespace}{name}"));
    ensure!(!registry.contains_key(&flat), "duplicate tool name: {flat}");
    let mut description = spec["description"].as_str().unwrap_or_default().to_owned();
    let parameters = if kind == "custom" {
        description.push_str(
            " Pass the raw tool input verbatim in the input string, without code fences.\n",
        );
        description.push_str(spec["format"]["definition"].as_str().unwrap_or_default());
        json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
    } else {
        spec.get("parameters")
            .cloned()
            .unwrap_or(json!({"type":"object","properties":{}}))
    };
    declarations.push(json!({"name":flat,"description":description,"parameters":parameters}));
    registry.insert(
        flat,
        Tool {
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            kind: kind.to_owned(),
            execution: spec.get("execution").cloned(),
        },
    );
    Ok(())
}

pub fn translate(request: &Value, replay: &mut Replay) -> Result<Value> {
    ensure!(request.is_object(), "request must be an object");
    ensure!(
        request
            .get("previous_response_id")
            .is_none_or(Value::is_null),
        "previous_response_id unsupported; send full history"
    );
    ensure!(
        request.get("background").is_none_or(|value| value == false),
        "background responses unsupported"
    );
    let (declarations, _) = tools(request)?;
    let mut contents = Vec::new();
    let mut system = Vec::new();
    if let Some(instructions) = request.get("instructions").filter(|value| !value.is_null()) {
        system
            .push(json!({"text":instructions.as_str().context("instructions must be a string")?}));
    }
    let items = match request.get("input") {
        Some(Value::String(text)) => vec![json!({"role":"user","content":text})],
        Some(Value::Array(items)) => items.clone(),
        _ => bail!("input must be a string or array"),
    };
    let mut names = HashMap::new();
    for item in &items {
        translate_item(item, replay, &mut contents, &mut system, &mut names)?;
    }
    ensure!(
        !contents.is_empty(),
        "input must contain conversation content"
    );
    let mut result =
        json!({"contents":contents,"generationConfig":{"thinkingConfig":{"includeThoughts":true}}});
    if !system.is_empty() {
        result["systemInstruction"] = json!({"parts":system});
    }
    if !declarations.is_empty() {
        result["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    apply_options(request, &mut result)?;
    Ok(result)
}

fn translate_item(
    item: &Value,
    replay: &mut Replay,
    contents: &mut Vec<Value>,
    system: &mut Vec<Value>,
    names: &mut HashMap<String, Value>,
) -> Result<()> {
    match item["type"].as_str().unwrap_or("message") {
        "message" => {
            let role = required_string(item, "role")?;
            ensure!(
                matches!(role, "user" | "assistant" | "developer" | "system"),
                "unsupported role: {role}"
            );
            if role == "assistant"
                && let Some(id) = item["id"].as_str().filter(|id| id.starts_with("ag_"))
            {
                let parts = replay.get(id)?;
                for part in parts.as_array().context("invalid message replay")? {
                    push_part(contents, "model", part.clone());
                }
                return Ok(());
            }
            let parts = message_parts(&item["content"])?;
            for part in parts {
                if matches!(role, "system" | "developer") {
                    system.push(part);
                } else {
                    push_part(
                        contents,
                        if role == "assistant" { "model" } else { "user" },
                        part,
                    );
                }
            }
        }
        "function_call" | "custom_tool_call" | "tool_search_call" => {
            let call_id = required_string(item, "call_id")?;
            let part = replay.get(call_id)?;
            names.insert(call_id.to_owned(), part["functionCall"].clone());
            push_part(contents, "model", part);
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            let call_id = required_string(item, "call_id")?;
            let call = names
                .get(call_id)
                .context("tool output has no matching call in history")?;
            let output = item
                .get("output")
                .or_else(|| item.get("tools"))
                .context("tool output is missing")?;
            push_part(
                contents,
                "user",
                json!({"functionResponse":{"name":call["name"],"id":call["id"],"response":{"output":output}}}),
            );
        }
        "reasoning" => {
            if let Some(id) = item["id"].as_str() {
                let parts = replay.get(id)?;
                for part in parts.as_array().context("invalid reasoning replay")? {
                    push_part(contents, "model", part.clone());
                }
            } else {
                bail!("reasoning replay requires an item id");
            }
        }
        kind => bail!("unsupported input item: {kind}"),
    }
    Ok(())
}

fn message_parts(content: &Value) -> Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    let mut parts = Vec::new();
    for part in content
        .as_array()
        .context("message content must be a string or array")?
    {
        match required_string(part, "type")? {
            "input_text" | "output_text" => {
                parts.push(json!({"text":required_string(part,"text")?}))
            }
            "input_image" => {
                let url = required_string(part, "image_url")?;
                let (mime, data) = url
                    .strip_prefix("data:")
                    .and_then(|url| url.split_once(";base64,"))
                    .context("only inline base64 images supported")?;
                ensure!(mime.starts_with("image/"), "invalid image mime type");
                parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
            }
            kind => bail!("unsupported content: {kind}"),
        }
    }
    Ok(parts)
}

pub fn push_part(contents: &mut Vec<Value>, role: &str, part: Value) {
    if let Some(last) = contents.last_mut()
        && last["role"] == role
        && let Some(parts) = last["parts"].as_array_mut()
    {
        parts.push(part);
        return;
    }
    contents.push(json!({"role":role,"parts":[part]}));
}

fn apply_options(request: &Value, result: &mut Value) -> Result<()> {
    let mode = match request.get("tool_choice") {
        None | Some(Value::Null) => "AUTO",
        Some(Value::String(choice)) if choice == "auto" => "AUTO",
        Some(Value::String(choice)) if choice == "none" => "NONE",
        Some(Value::String(choice)) if choice == "required" => "ANY",
        _ => bail!("unsupported tool_choice"),
    };
    result["toolConfig"] = json!({"functionCallingConfig":{"mode":mode}});
    if let Some(effort) = request["reasoning"]["effort"].as_str() {
        result["generationConfig"]["thinkingConfig"]["thinkingLevel"] = json!(match effort {
            "none" | "minimal" | "low" => "LOW",
            "medium" => "MEDIUM",
            "high" | "xhigh" | "max" => "HIGH",
            _ => bail!("unsupported reasoning effort"),
        });
    }
    if let Some(max) = request.get("max_output_tokens") {
        ensure!(
            max.as_u64().is_some_and(|value| value > 0),
            "max_output_tokens must be positive"
        );
        result["generationConfig"]["maxOutputTokens"] = max.clone();
    }
    if let Some(format) = request.get("text").and_then(|text| text.get("format")) {
        match format["type"].as_str() {
            Some("text") => {}
            Some("json_schema") => {
                result["generationConfig"]["responseMimeType"] = json!("application/json");
                result["generationConfig"]["responseSchema"] = crate::schema::translate(
                    format.get("schema").context("missing output schema")?,
                    !request["model"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("claude"),
                )?;
            }
            _ => bail!("unsupported text format"),
        }
    }
    Ok(())
}
