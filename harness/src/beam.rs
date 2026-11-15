//! Lean Beam over MCP, one server per attempt.
//!
//! The server is any stdio command: inside the attempt's sandbox in production,
//! a host install in tests. `workspace` is the attempt root as the server sees
//! it. It is injected into every call, replacing anything the model passed, so
//! the model can only address its own attempt. Beam itself checks versions and
//! handles.
//!
//! The tool list comes from the server when it starts. Extra MCP servers
//! (`--mcp`) use the same client, without the workspace injection.

use anyhow::{Context, Result, anyhow};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Tools that take a workspace but no Lean/Lake operation; not offered to the model.
const HIDDEN: &[&str] = &["beam_version", "beam_stats", "beam_feedback_report"];

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// The server's `readOnlyHint`. Informational only; nothing uses it.
    pub read_only: bool,
}

/// A running MCP server and the tools it advertised. Each attempt starts its own.
pub struct Beam {
    service: RunningService<RoleClient, ()>,
    tools: Vec<ToolSpec>,
    workspace: Option<PathBuf>,
}

impl Beam {
    pub async fn spawn(
        server: tokio::process::Command,
        workspace: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::connect(server, Some(workspace.into())).await
    }

    /// Extra servers use the same transport without Beam's workspace injection.
    pub async fn connect(
        server: tokio::process::Command,
        workspace: Option<PathBuf>,
    ) -> Result<Self> {
        let transport = TokioChildProcess::new(server).context("spawn MCP server")?;
        let service = ().serve(transport).await.context("MCP initialize")?;
        let listed = service.list_all_tools().await.context("MCP tools/list")?;
        let mut tools = Vec::new();
        for tool in listed {
            if workspace.is_some() && HIDDEN.contains(&tool.name.as_ref()) {
                continue;
            }
            let spec = to_spec(&tool, workspace.is_some());
            tools.push(spec);
        }
        Ok(Self {
            service,
            tools,
            workspace,
        })
    }

    /// Common workflow rules moved out of per-tool descriptions in Beam 0.2.
    pub fn instructions(&self) -> String {
        self.service
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .unwrap_or_default()
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Call `name` for this attempt. MCP tool and transport errors return `Err`.
    /// Ordinary Lean diagnostics stay in the successful tool result, including
    /// probes whose Lean code does not compile.
    pub async fn call(&self, name: &str, args: &Value) -> Result<String> {
        let mut arguments: Map<String, Value> = match args {
            Value::Object(m) => m.clone(),
            _ => return Err(anyhow!("arguments must be an object")),
        };
        if let Some(workspace) = &self.workspace {
            // Replaces any `workspace` the model supplied.
            arguments.insert("workspace".into(), json!({"root": workspace}));
        }
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = Some(arguments);
        let result = self
            .service
            .call_tool(params)
            .await
            .with_context(|| format!("MCP call {name}"))?;
        render(&serde_json::to_value(&result)?).with_context(|| format!("MCP tool {name}"))
    }
}

fn to_spec(tool: &Tool, beam: bool) -> ToolSpec {
    // The harness supplies `workspace`; remove it from what the model sees.
    let mut schema = serde_json::to_value(&*tool.input_schema).unwrap_or(json!({}));
    if beam {
        if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            props.remove("workspace");
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|r| r != "workspace");
        }
    }
    schema.as_object_mut().map(|o| o.remove("$schema"));
    ToolSpec {
        name: tool.name.to_string(),
        description: tool.description.as_deref().unwrap_or("").to_owned(),
        input_schema: schema,
        read_only: tool
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false),
    }
}

/// Flatten an MCP tool result into text: structured content when present,
/// otherwise the concatenated text parts. Preserve MCP's error flag across
/// this boundary so callers such as `lean_check` cannot hide a failed tool
/// inside an otherwise successful JSON reply.
fn render(result: &Value) -> Result<String> {
    let out = if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        serde_json::to_string(structured)?
    } else {
        let mut out = String::new();
        if let Some(parts) = result.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
        out
    };
    anyhow::ensure!(result.get("isError") != Some(&Value::Bool(true)), "{out}");
    Ok(out)
}

/// A Beam server from a host install, for tests and local experiments.
/// Production uses runtime::mcp, which runs the server inside the sandbox.
pub fn host_server(binary: &Path) -> tokio::process::Command {
    tokio::process::Command::new(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prefers_structured_and_marks_errors() {
        let ok = json!({"content": [{"type": "text", "text": "{\"ok\":true}"}]});
        assert_eq!(render(&ok).unwrap(), "{\"ok\":true}");
        let structured =
            json!({"content": [{"type": "text", "text": "x"}], "structuredContent": {"goals": []}});
        assert_eq!(render(&structured).unwrap(), "{\"goals\":[]}");
        let err = json!({"isError": true, "content": [{"type": "text", "text": "bad"}]});
        assert_eq!(render(&err).unwrap_err().to_string(), "bad");
        let err = json!({"isError": true, "structuredContent": {
            "code": "internalError", "message": "too many open files"
        }});
        assert!(
            render(&err)
                .unwrap_err()
                .to_string()
                .contains("too many open files")
        );
        // A rejected proof is still a completed probe, with useful diagnostics.
        let rejected = json!({"isError": false, "structuredContent": {
            "success": false, "messages": [{"severity": "error", "text": "unknown constant"}]
        }});
        assert!(render(&rejected).unwrap().contains("unknown constant"));
    }
}

/// An `--mcp` config file. The server is started inside every attempt's
/// sandbox, so `command` must exist in the sandbox image.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}
impl ExtraServer {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        // `name` becomes part of every tool name (`mcp_<name>_<tool>`), so check
        // it now rather than after the agents have started.
        anyhow::ensure!(
            !config.name.is_empty()
                && config
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "MCP name must contain only letters, digits, underscores"
        );
        anyhow::ensure!(!config.command.is_empty(), "MCP command is empty");
        Ok(config)
    }
}
