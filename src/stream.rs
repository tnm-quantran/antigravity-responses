use crate::protocol::{Replay, Tool, required_string};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub struct Translator {
    pub response: Value,
    pub finished: bool,
    sequence: u64,
    tools: HashMap<String, Tool>,
    active: Option<(Value, Vec<Value>)>,
    show_reasoning: bool,
}

impl Translator {
    pub fn new(model: &str, tools: HashMap<String, Tool>, reasoning: &str) -> Self {
        Self {
            response: json!({"id":format!("resp_{}",Uuid::new_v4()),"object":"response","created_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),"model":model,"status":"in_progress","output":[],"usage":null,"error":null,"incomplete_details":null}),
            sequence: 0,
            tools,
            active: None,
            show_reasoning: reasoning == "raw-thought",
            finished: false,
        }
    }

    pub fn event(&mut self, kind: &str, mut data: Value) -> Value {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        data
    }

    pub fn created(&mut self) -> Value {
        self.event("response.created", json!({"response":self.response}))
    }

    pub fn ingest(&mut self, value: &Value, replay: &mut Replay) -> Result<Vec<Value>> {
        let response = value.get("response").unwrap_or(value);
        ensure!(
            value.get("error").is_none() && response.get("error").is_none(),
            "backend returned a stream error"
        );
        ensure!(
            response["promptFeedback"]["blockReason"]
                .as_str()
                .is_none_or(|reason| reason.is_empty() || reason == "BLOCK_REASON_UNSPECIFIED"),
            "backend blocked the prompt"
        );
        if let Some(usage) = response.get("usageMetadata") {
            self.set_usage(usage);
        }
        let mut events = Vec::new();
        if let Some(candidate) = response["candidates"]
            .as_array()
            .and_then(|values| values.first())
        {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if part.get("functionCall").is_some() {
                        self.call(part, replay, &mut events)?;
                    } else if part.get("text").is_some() || part.get("thoughtSignature").is_some() {
                        self.text(part, replay, &mut events)?;
                    } else {
                        bail!("unsupported backend content part");
                    }
                }
            }
            if let Some(reason) = candidate["finishReason"]
                .as_str()
                .filter(|reason| *reason != "FINISH_REASON_UNSPECIFIED")
            {
                ensure!(
                    matches!(reason, "STOP" | "MAX_TOKENS"),
                    "backend generation failed: {reason}"
                );
                self.close(replay, &mut events)?;
                if reason == "MAX_TOKENS" {
                    self.response["status"] = json!("incomplete");
                    self.response["incomplete_details"] = json!({"reason":"max_output_tokens"});
                } else {
                    self.response["status"] = json!("completed");
                }
                self.finished = true;
            }
        }
        Ok(events)
    }

    fn text(&mut self, part: &Value, replay: &mut Replay, events: &mut Vec<Value>) -> Result<()> {
        let is_thought = part["thought"] == true;
        let kind = if is_thought { "reasoning" } else { "message" };
        if self
            .active
            .as_ref()
            .is_some_and(|(item, _)| item["type"] != kind)
        {
            self.close(replay, events)?;
        }
        if self.active.is_none() {
            self.open(kind, events);
        }
        let (item, parts) = self.active.as_mut().context("missing active output")?;
        parts.push(part.clone());
        let text = part["text"].as_str().unwrap_or_default();
        if text.is_empty() || (is_thought && !self.show_reasoning) {
            return Ok(());
        }
        let id = item["id"].clone();
        let key = if is_thought { "summary" } else { "content" };
        let current = item[key][0]["text"].as_str().unwrap_or_default();
        item[key][0]["text"] = json!(format!("{current}{text}"));
        let index = self.response["output"]
            .as_array()
            .context("invalid output")?
            .len();
        let event = if is_thought {
            self.event(
                "response.reasoning_summary_text.delta",
                json!({"item_id":id,"output_index":index,"summary_index":0,"delta":text}),
            )
        } else {
            self.event("response.output_text.delta", json!({"item_id":id,"output_index":index,"content_index":0,"delta":text,"logprobs":[]}))
        };
        events.push(event);
        Ok(())
    }

    fn open(&mut self, kind: &str, events: &mut Vec<Value>) {
        let id = format!("ag_{}", Uuid::new_v4());
        let item = if kind == "reasoning" {
            json!({"id":id,"type":"reasoning","summary":if self.show_reasoning {json!([{"type":"summary_text","text":""}])} else {json!([])}})
        } else {
            json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]})
        };
        let index = self.response["output"].as_array().map_or(0, Vec::len);
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        ));
        if kind == "message" {
            events.push(self.event("response.content_part.added", json!({"item_id":id,"output_index":index,"content_index":0,"part":item["content"][0]})));
        } else if self.show_reasoning {
            events.push(self.event("response.reasoning_summary_part.added", json!({"item_id":id,"output_index":index,"summary_index":0,"part":item["summary"][0]})));
        }
        self.active = Some((item, Vec::new()));
    }

    fn close(&mut self, replay: &mut Replay, events: &mut Vec<Value>) -> Result<()> {
        let Some((mut item, parts)) = self.active.take() else {
            return Ok(());
        };
        let id = required_string(&item, "id")?.to_owned();
        replay.insert(id.clone(), json!(parts))?;
        let index = self.response["output"]
            .as_array()
            .context("invalid output")?
            .len();
        if item["type"] == "message" {
            item["status"] = json!("completed");
            events.push(self.event("response.output_text.done", json!({"item_id":id,"output_index":index,"content_index":0,"text":item["content"][0]["text"],"logprobs":[]})));
            events.push(self.event("response.content_part.done", json!({"item_id":id,"output_index":index,"content_index":0,"part":item["content"][0]})));
        } else if self.show_reasoning {
            events.push(self.event("response.reasoning_summary_text.done", json!({"item_id":id,"output_index":index,"summary_index":0,"text":item["summary"][0]["text"]})));
            events.push(self.event("response.reasoning_summary_part.done", json!({"item_id":id,"output_index":index,"summary_index":0,"part":item["summary"][0]})));
        }
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        ));
        self.response["output"]
            .as_array_mut()
            .context("invalid output")?
            .push(item);
        Ok(())
    }

    fn call(&mut self, part: &Value, replay: &mut Replay, events: &mut Vec<Value>) -> Result<()> {
        self.close(replay, events)?;
        let call = &part["functionCall"];
        let flat = required_string(call, "name")?;
        let tool = self
            .tools
            .get(flat)
            .context("backend called an undeclared tool")?;
        let call_id = format!("call_{}", Uuid::new_v4());
        let args = call.get("args").cloned().unwrap_or(json!({}));
        ensure!(args.is_object(), "tool arguments must be an object");
        let mut original = part.clone();
        if original["functionCall"].get("id").is_none() {
            original["functionCall"]["id"] = json!(call_id);
        }
        replay.insert(call_id.clone(), original)?;
        let item_kind = match tool.kind.as_str() {
            "custom" => "custom_tool_call",
            "tool_search" => "tool_search_call",
            _ => "function_call",
        };
        let mut item = json!({"type":item_kind,"id":format!("ag_{}",Uuid::new_v4()),"call_id":call_id,"name":tool.name,"status":"completed"});
        if let Some(namespace) = &tool.namespace {
            item["namespace"] = json!(namespace);
        }
        if tool.kind == "custom" {
            item["input"] = json!(required_string(&args, "input")?);
        } else if tool.kind == "tool_search" {
            item["arguments"] = args;
            item["execution"] = tool
                .execution
                .clone()
                .context("missing tool_search execution")?;
        } else {
            item["arguments"] = json!(args.to_string());
        }
        let index = self.response["output"]
            .as_array()
            .context("invalid output")?
            .len();
        let mut added = item.clone();
        if tool.kind == "custom" {
            added["input"] = json!("");
        } else if tool.kind == "function" {
            added["arguments"] = json!("");
        }
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":added}),
        ));
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        ));
        self.response["output"]
            .as_array_mut()
            .context("invalid output")?
            .push(item);
        Ok(())
    }

    fn set_usage(&mut self, usage: &Value) {
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        self.response["usage"] = json!({"input_tokens":count("promptTokenCount"),"output_tokens":count("candidatesTokenCount")+count("thoughtsTokenCount"),"total_tokens":count("totalTokenCount"),"input_tokens_details":{"cached_tokens":count("cachedContentTokenCount")},"output_tokens_details":{"reasoning_tokens":count("thoughtsTokenCount")}});
    }

    pub fn completed(&mut self) -> Result<Value> {
        ensure!(self.finished, "backend stream ended before finishReason");
        let kind = if self.response["status"] == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        Ok(self.event(kind, json!({"response":self.response})))
    }

    pub fn failed(&mut self) -> Value {
        self.response["status"] = json!("failed");
        self.response["error"] = json!({"code":"server_error","message":"Antigravity stream failed; see gateway stderr for request id"});
        self.event("response.failed", json!({"response":self.response}))
    }
}
