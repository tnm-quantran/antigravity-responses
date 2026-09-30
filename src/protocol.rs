use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REPLAY_HEADER: &[u8] = b"antigravity-replay-v1\n";

#[derive(Clone)]
pub struct Tool {
    pub flat: String,
    pub name: String,
    pub namespace: Option<String>,
    pub kind: String,
    pub execution: Option<Value>,
    pub strict: bool,
}

pub struct Replay {
    entries: VecDeque<(String, Value, Instant, usize)>,
    bytes: usize,
    limit: usize,
    ttl: Duration,
    path: Option<PathBuf>,
    pending: HashSet<String>,
    disk_bytes: usize,
    needs_compaction: bool,
}

impl Replay {
    pub fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            ttl,
            path: None,
            pending: HashSet::new(),
            disk_bytes: 0,
            needs_compaction: false,
        }
    }

    pub fn open(limit: usize, ttl: Duration, path: PathBuf) -> Result<Self> {
        let mut replay = Self::new(limit, ttl);
        replay.path = Some(path.clone());
        if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read replay store {}", path.display()))?;
            ensure!(bytes.len() <= limit, "replay store exceeds state limit");
            replay.disk_bytes = bytes.len();
            let entries = replay.read_replay_entries(&bytes, &path)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            for (key, value, expires) in entries {
                if expires > now {
                    replay.insert_with_ttl(key, value, Duration::from_secs(expires - now))?;
                }
            }
        }
        replay.pending.clear();
        Ok(replay)
    }

    fn read_replay_entries(
        &mut self,
        bytes: &[u8],
        path: &Path,
    ) -> Result<Vec<(String, Value, u64)>> {
        let Some(journal) = bytes.strip_prefix(REPLAY_HEADER) else {
            self.needs_compaction = true;
            return serde_json::from_slice(bytes)
                .with_context(|| format!("parse replay store {}", path.display()));
        };
        let mut entries = Vec::new();
        let mut records = journal.split(|byte| *byte == b'\n');
        if !records.next_back().unwrap_or_default().is_empty() {
            eprintln!(
                "replay store {}: discard incomplete final batch",
                path.display()
            );
            self.needs_compaction = true;
        }
        for record in records {
            let batch: Vec<(String, Value, u64)> = serde_json::from_slice(record)
                .with_context(|| format!("parse replay batch {}", path.display()))?;
            entries.extend(batch);
        }
        Ok(entries)
    }

    pub fn insert(&mut self, key: String, part: Value) -> Result<()> {
        self.insert_with_ttl(key, part, self.ttl)
    }

    fn insert_with_ttl(&mut self, key: String, part: Value, ttl: Duration) -> Result<()> {
        let bytes = if self.path.is_some() {
            serde_json::to_vec(&(&key, &part, u64::MAX))?.len() + 1
        } else {
            key.len() + serde_json::to_vec(&part)?.len()
        };
        let capacity = if self.path.is_some() {
            self.limit.saturating_sub(REPLAY_HEADER.len() + 3)
        } else {
            self.limit
        };
        ensure!(
            bytes <= capacity,
            "provider part exceeds replay state limit"
        );
        self.prune();
        while self.bytes + bytes > capacity || self.entries.len() >= 4096 {
            if let Some((key, _, _, removed)) = self.entries.pop_front() {
                self.bytes -= removed;
                self.pending.remove(&key);
            }
        }
        self.bytes += bytes;
        let elapsed = self.ttl.saturating_sub(ttl.min(self.ttl));
        let inserted = Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(Instant::now);
        if self.path.is_some() {
            self.pending.insert(key.clone());
        }
        self.entries.push_back((key, part, inserted, bytes));
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

    pub fn response_history(&mut self, id: &str) -> Result<Vec<Value>> {
        let mut history = Vec::new();
        let mut current = Some(id.to_owned());
        while let Some(id) = current {
            let response = self
                .get(&format!("response:{id}"))
                .with_context(|| format!("previous response {id} expired or is unknown"))?;
            current = response["parent"].as_str().map(str::to_owned);
            history.push(
                response["items"]
                    .as_array()
                    .context("invalid previous response history")?
                    .clone(),
            );
        }
        history.reverse();
        Ok(history.into_iter().flatten().collect())
    }

    pub fn store_response(
        &mut self,
        id: &str,
        parent: Option<&str>,
        items: Vec<Value>,
    ) -> Result<()> {
        self.insert(
            format!("response:{id}"),
            json!({"parent":parent,"items":items}),
        )?;
        self.persist()
    }

    fn persist(&mut self) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        self.prune();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let pending = self
            .entries
            .iter()
            .filter(|(key, _, _, _)| self.pending.contains(key))
            .map(|(key, value, inserted, _)| {
                (
                    key,
                    value,
                    now.saturating_add(self.ttl.saturating_sub(inserted.elapsed()).as_secs()),
                )
            })
            .collect::<Vec<_>>();
        let mut bytes = serde_json::to_vec(&pending)?;
        bytes.push(b'\n');
        if self.disk_bytes == 0
            || self.needs_compaction
            || self.disk_bytes.saturating_add(bytes.len()) > self.limit
        {
            bytes = self.compact_bytes()?;
            crate::storage::write_private(&path, &bytes)
                .context("compact response replay store")?;
            self.disk_bytes = bytes.len();
            self.needs_compaction = false;
        } else {
            crate::storage::append_private(&path, &bytes)
                .context("append response replay batch")?;
            self.disk_bytes += bytes.len();
        }
        self.pending.clear();
        Ok(())
    }

    fn compact_bytes(&self) -> Result<Vec<u8>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let records = self
            .entries
            .iter()
            .map(|(key, value, inserted, _)| {
                let remaining = self.ttl.saturating_sub(inserted.elapsed()).as_secs();
                (key, value, now.saturating_add(remaining))
            })
            .collect::<Vec<_>>();
        let mut bytes = REPLAY_HEADER.to_vec();
        serde_json::to_writer(&mut bytes, &records)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= self.limit,
            "replay store exceeds state limit"
        );
        Ok(bytes)
    }

    fn prune(&mut self) {
        while self
            .entries
            .front()
            .is_some_and(|(_, _, time, _)| time.elapsed() >= self.ttl)
        {
            if let Some((key, _, _, bytes)) = self.entries.pop_front() {
                self.bytes -= bytes;
                self.pending.remove(&key);
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
    let (mut declarations, registry) = declare_tools(request)?;
    translate_declarations(request, &mut declarations, false)?;
    Ok((declarations, registry))
}

fn declare_tools(request: &Value) -> Result<(Vec<Value>, HashMap<String, Tool>)> {
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
    Ok((declarations, registry))
}

fn translate_declarations(
    request: &Value,
    declarations: &mut [Value],
    reject_lossy: bool,
) -> Result<()> {
    let uppercase = !request["model"]
        .as_str()
        .unwrap_or_default()
        .contains("claude");
    for declaration in declarations {
        let (schema, dropped) =
            crate::schema::translate_tool_with_report(&declaration["parameters"], uppercase)?;
        ensure!(
            !reject_lossy || dropped.is_empty(),
            "tool schema loses unsupported constraints: {}",
            dropped.join(", ")
        );
        crate::schema::warn_tool_schema(required_string(declaration, "name")?, &dropped)?;
        declaration["parameters"] = schema;
    }
    Ok(())
}

fn declare_tool(
    spec: &Value,
    namespace: Option<&str>,
    declarations: &mut Vec<Value>,
    registry: &mut HashMap<String, Tool>,
) -> Result<()> {
    let kind = required_string(spec, "type")?;
    if matches!(kind, "web_search" | "web_search_preview") {
        if spec["external_web_access"] == false {
            return Ok(());
        }
        let flat = "gateway_web_search".to_owned();
        ensure!(!registry.contains_key(&flat), "duplicate tool name: {flat}");
        declarations.push(json!({"name":flat,"description":"Search the web or inspect a user-provided URL. Provide the URL or a concise search query.","parameters":{"type":"OBJECT","properties":{"query":{"type":"STRING"}},"required":["query"]}}));
        registry.insert(
            flat.clone(),
            Tool {
                flat: flat.clone(),
                name: flat,
                namespace: None,
                kind: "web_search".to_owned(),
                execution: None,
                strict: false,
            },
        );
        return Ok(());
    }
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
    ensure!(
        !registry.values().any(|tool| tool.flat == flat),
        "duplicate tool name: {flat}"
    );
    let wire_name = google_tool_name(&flat, registry);
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
    declarations.push(json!({"name":wire_name,"description":description,"parameters":parameters}));
    registry.insert(
        wire_name,
        Tool {
            flat,
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            kind: kind.to_owned(),
            execution: spec.get("execution").cloned(),
            strict: spec["strict"] == true,
        },
    );
    Ok(())
}

pub fn translate(request: &Value, replay: &mut Replay) -> Result<Value> {
    translate_with_tools(request, replay, false).map(|(result, _)| result)
}

pub(crate) fn model_reasoning_effort(model: &str) -> Option<&'static str> {
    if !model.starts_with("gemini-") {
        return None;
    }
    match model.rsplit_once('-')?.1 {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        _ => None,
    }
}

pub(crate) fn translate_with_tools(
    request: &Value,
    replay: &mut Replay,
    reject_lossy_schema: bool,
) -> Result<(Value, HashMap<String, Tool>)> {
    ensure!(request.is_object(), "request must be an object");
    ensure!(
        request.get("background").is_none_or(|value| value == false),
        "background responses unsupported"
    );
    let (declarations, registry) = declare_tools(request)?;
    let mut contents = Vec::new();
    let mut system = Vec::new();
    if let Some(instructions) = request.get("instructions").filter(|value| !value.is_null()) {
        system
            .push(json!({"text":instructions.as_str().context("instructions must be a string")?}));
    }
    let mut items = match request.get("input") {
        Some(Value::String(text)) => vec![json!({"role":"user","content":text})],
        Some(Value::Array(items)) => items.clone(),
        _ => bail!("input must be a string or array"),
    };
    if let Some(previous) = request
        .get("previous_response_id")
        .filter(|value| !value.is_null())
    {
        let previous = previous
            .as_str()
            .filter(|id| !id.is_empty())
            .context("previous_response_id must be a non-empty string")?;
        let mut history = replay.response_history(previous)?;
        history.append(&mut items);
        items = history;
    }
    let mut names = HashMap::new();
    for item in &items {
        translate_item(
            item,
            replay,
            &registry,
            &mut contents,
            &mut system,
            &mut names,
        )?;
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
    apply_options(request, &mut result, &registry)?;
    if let Some(declarations) = result
        .pointer_mut("/tools/0/functionDeclarations")
        .and_then(Value::as_array_mut)
    {
        translate_declarations(request, declarations, reject_lossy_schema)?;
    }
    Ok((result, registry))
}

fn translate_item(
    item: &Value,
    replay: &mut Replay,
    registry: &HashMap<String, Tool>,
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
                if replay_tool_group(id, replay, contents, names)? {
                    return Ok(());
                }
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
            let restored_group = replay_tool_group(call_id, replay, contents, names)?;
            let part = replay
                .get(call_id)
                .or_else(|_| replay_call(item, call_id, registry))?;
            names.insert(call_id.to_owned(), part["functionCall"].clone());
            if !restored_group {
                push_part(contents, "model", part);
            }
        }
        // Codex's Antigravity adapter ignores hosted WebSearch history items.
        "web_search_call" => {}
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            let call_id = required_string(item, "call_id")?;
            let call = names
                .get(call_id)
                .cloned()
                .or_else(|| {
                    let part = replay.get(call_id).ok()?;
                    contents
                        .iter()
                        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
                        .any(|history| {
                            history["functionCall"].is_object()
                                && history["functionCall"] == part["functionCall"]
                        })
                        .then(|| part["functionCall"].clone())
                })
                .context("tool output has no matching call in history")?;
            let output = item
                .get("output")
                .or_else(|| item.get("tools"))
                .context("tool output is missing")?;
            push_part(contents, "user", function_response(&call, output)?);
        }
        "reasoning" => {
            if let Some(id) = item["id"].as_str() {
                if replay_tool_group(id, replay, contents, names)? {
                    return Ok(());
                }
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

fn replay_call(item: &Value, call_id: &str, registry: &HashMap<String, Tool>) -> Result<Value> {
    let name = required_string(item, "name")?;
    let namespace = item["namespace"].as_str();
    let (wire_name, tool) = registry
        .iter()
        .find(|(_, tool)| tool.name == name && tool.namespace.as_deref() == namespace)
        .context("tool replay is missing and cannot be reconstructed")?;
    let args = match tool.kind.as_str() {
        "custom" => json!({"input":required_string(item, "input")?}),
        "tool_search" => item.get("arguments").cloned().unwrap_or(json!({})),
        _ => serde_json::from_str(required_string(item, "arguments")?)
            .context("invalid function call arguments in replay")?,
    };
    let mut call = json!({"functionCall":{"name":wire_name,"id":call_id,"args":args}});
    if let Some(signature) = item.pointer("/extra_content/google/thought_signature") {
        call["thoughtSignature"] = signature.clone();
    }
    Ok(call)
}

fn replay_tool_group(
    call_id: &str,
    replay: &mut Replay,
    contents: &mut Vec<Value>,
    names: &mut HashMap<String, Value>,
) -> Result<bool> {
    let Ok(group) = replay.get(&format!("tool_group:{call_id}")) else {
        return Ok(false);
    };
    let id = group.as_str().context("invalid tool group id")?;
    let marker = format!("group:{id}");
    if names.contains_key(&marker) {
        return Ok(true);
    }
    let parts = replay.get(id)?;
    let parts = parts.as_array().context("invalid tool group parts")?;
    let previous = contents
        .last()
        .filter(|content| content["role"] == "model")
        .and_then(|content| content["parts"].as_array());
    let repeated = previous.map_or(0, |previous| {
        (0..=previous.len().min(parts.len()))
            .rev()
            .find(|count| previous[previous.len() - count..] == parts[..*count])
            .unwrap_or(0)
    });
    for part in &parts[repeated..] {
        push_part(contents, "model", part.clone());
    }
    let outputs = replay.get(&format!("group_outputs:{id}"))?;
    for output in outputs.as_array().context("invalid tool group outputs")? {
        let call = replay.get(required_string(output, "call_id")?)?;
        push_part(
            contents,
            "user",
            function_response(&call["functionCall"], &output["output"])?,
        );
    }
    names.insert(marker, json!(true));
    Ok(true)
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
                parts.push(json!({"inlineData":image_data(part)?}));
            }
            kind => bail!("unsupported content: {kind}"),
        }
    }
    Ok(parts)
}

fn image_data(part: &Value) -> Result<Value> {
    let (mime, data) = if part["type"] == "image" {
        (
            required_string(part, "mimeType")?,
            required_string(part, "data")?,
        )
    } else {
        required_string(part, "image_url")?
            .strip_prefix("data:")
            .and_then(|url| url.split_once(";base64,"))
            .context("only inline base64 images supported")?
    };
    ensure!(
        mime.starts_with("image/") && mime.len() > 6,
        "invalid image mime type"
    );
    ensure!(!data.is_empty(), "image data is empty");
    let mut buffer = [0_u8; 3072];
    let mut chunks = data.as_bytes().chunks(4096).peekable();
    while let Some(chunk) = chunks.next() {
        ensure!(
            chunks.peek().is_none() || !chunk.contains(&b'='),
            "invalid image base64 padding"
        );
        STANDARD
            .decode_slice(chunk, &mut buffer)
            .context("invalid image base64 data")?;
    }
    Ok(json!({"mimeType":mime,"data":data}))
}

fn function_response(call: &Value, output: &Value) -> Result<Value> {
    let (converted_output, parts) =
        if let Some(blocks) = output.as_array().or_else(|| output["content"].as_array()) {
            let (converted, parts) = convert_image_blocks(blocks)?;
            let converted_output = if output.is_array() {
                Value::Array(converted)
            } else {
                let mut fields = output
                    .as_object()
                    .context("invalid tool output")?
                    .iter()
                    .filter(|(key, _)| key.as_str() != "content")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<serde_json::Map<_, _>>();
                fields.insert("content".into(), Value::Array(converted));
                Value::Object(fields)
            };
            (converted_output, parts)
        } else {
            (output.clone(), Vec::new())
        };
    let mut response = json!({"name":call["name"],"id":call["id"],"response":{"output":null}});
    response["response"]["output"] = converted_output;
    if !parts.is_empty() {
        response["parts"] = Value::Array(parts);
    }
    let mut part = json!({"functionResponse":null});
    part["functionResponse"] = response;
    Ok(part)
}

fn convert_image_blocks(blocks: &[Value]) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut parts = Vec::new();
    let mut converted = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        if matches!(block["type"].as_str(), Some("input_image" | "image")) {
            let name = format!("tool_image_{index}");
            let mut data = image_data(block)?;
            data["displayName"] = json!(name);
            let mut part = json!({"inlineData":null});
            part["inlineData"] = data;
            parts.push(part);
            converted.push(json!({"$ref":name}));
        } else {
            converted.push(block.clone());
        }
    }
    Ok((converted, parts))
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

fn apply_options(
    request: &Value,
    result: &mut Value,
    registry: &HashMap<String, Tool>,
) -> Result<()> {
    let function_config = match request.get("tool_choice") {
        None | Some(Value::Null) => registry
            .values()
            .any(|tool| tool.strict)
            .then(|| json!({"mode":"VALIDATED"})),
        Some(Value::String(choice)) if choice == "auto" => registry
            .values()
            .any(|tool| tool.strict)
            .then(|| json!({"mode":"VALIDATED"})),
        Some(Value::String(choice)) if choice == "none" => Some(json!({"mode":"NONE"})),
        Some(Value::String(choice)) if choice == "required" => Some(json!({"mode":"ANY"})),
        Some(Value::Object(choice))
            if matches!(choice["type"].as_str(), Some("function" | "custom")) =>
        {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .context("tool_choice function requires a name")?;
            let flat = resolve_function_tool(name, registry)?;
            Some(json!({"mode":"ANY","allowedFunctionNames":[flat]}))
        }
        Some(Value::Object(choice)) if choice["type"] == "web_search" => {
            let flat = registry
                .iter()
                .find(|(_, tool)| tool.kind == "web_search")
                .map(|(flat, _)| flat)
                .context("tool_choice web_search requires a declared web_search tool")?;
            Some(json!({"mode":"ANY","allowedFunctionNames":[flat]}))
        }
        Some(Value::Object(choice)) if choice["type"] == "allowed_tools" => {
            let mode = match choice.get("mode").and_then(Value::as_str) {
                Some("auto") => "VALIDATED",
                Some("required") => "ANY",
                _ => bail!("tool_choice allowed_tools mode must be auto or required"),
            };
            let tools = choice
                .get("tools")
                .and_then(Value::as_array)
                .context("tool_choice allowed_tools requires a tools array")?;
            ensure!(
                !tools.is_empty(),
                "tool_choice allowed_tools cannot be empty"
            );
            let mut allowed = Vec::new();
            for tool in tools {
                let kind = tool["type"]
                    .as_str()
                    .context("tool_choice allowed_tools entries require a type")?;
                let flat = if matches!(kind, "function" | "custom") {
                    resolve_function_tool(
                        tool.get("name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .context("tool_choice allowed function requires a name")?,
                        registry,
                    )?
                } else if matches!(kind, "web_search" | "web_search_preview") {
                    registry
                        .iter()
                        .find(|(_, registered)| registered.kind == "web_search")
                        .map(|(flat, _)| flat.clone())
                        .context(
                            "tool_choice allowed web_search requires a declared web_search tool",
                        )?
                } else {
                    bail!("unsupported tool_choice allowed_tools type: {kind}");
                };
                if !allowed.contains(&flat) {
                    allowed.push(flat);
                }
            }
            Some(json!({"mode":mode,"allowedFunctionNames":allowed}))
        }
        _ => bail!("unsupported tool_choice"),
    };
    if let Some(function_config) = function_config {
        if request["tool_choice"]["type"] == "allowed_tools" {
            let allowed = function_config["allowedFunctionNames"]
                .as_array()
                .context("invalid allowed tool names")?;
            if let Some(declarations) = result["tools"][0]["functionDeclarations"].as_array_mut() {
                declarations
                    .retain(|declaration| allowed.iter().any(|name| name == &declaration["name"]));
            }
        }
        result["toolConfig"] = json!({"functionCallingConfig":function_config});
    }
    for (source, target, min, max) in [
        ("temperature", "temperature", 0.0, 2.0),
        ("top_p", "topP", 0.0, 1.0),
    ] {
        if let Some(value) = request.get(source) {
            let number = value
                .as_f64()
                .with_context(|| format!("{source} must be a number"))?;
            ensure!(
                (min..=max).contains(&number),
                "{source} must be between {min} and {max}"
            );
            result["generationConfig"][target] = json!(number);
        }
    }
    let effort = request["model"]
        .as_str()
        .and_then(model_reasoning_effort)
        .or_else(|| request["reasoning"]["effort"].as_str());
    if let Some(effort) = effort {
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

fn resolve_function_tool(name: &str, registry: &HashMap<String, Tool>) -> Result<String> {
    if registry.contains_key(name) {
        return Ok(name.to_owned());
    }
    let matching = registry
        .iter()
        .filter(|(_, tool)| tool.name == name || tool.flat == name)
        .map(|(flat, _)| flat.as_str())
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1,
        "tool_choice function is missing or ambiguous: {name}"
    );
    Ok(matching[0].to_owned())
}

fn google_tool_name(name: &str, registry: &HashMap<String, Tool>) -> String {
    let bytes = name.as_bytes();
    if bytes.len() <= 64
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
        && !registry.contains_key(name)
    {
        return name.to_owned();
    }
    let cleaned = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let prefix = cleaned
        .trim_start_matches(|character: char| character == '-' || character.is_ascii_digit());
    let prefix = if prefix.is_empty() { "tool" } else { prefix };
    let prefix = &prefix[..prefix.len().min(55)];
    for salt in 0_u32.. {
        let input = if salt == 0 {
            name.to_owned()
        } else {
            format!("{name}#{salt}")
        };
        let digest = Sha256::digest(input.as_bytes());
        let suffix = digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = format!("{prefix}_{suffix}");
        if !registry.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("tool name collision space exhausted")
}
