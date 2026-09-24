//! Read-only MCP (Model Context Protocol) server for inspecting podseq.
//!
//! `podseq mcp` exposes podseq's own state to LLM clients over the MCP
//! streamable HTTP transport. The server is stateless: clients POST JSON-RPC
//! 2.0 messages to `/mcp` and receive a single `application/json` response
//! (no SSE stream, no session id).
//!
//! Tools inspect three sources, all read-only:
//! - the node's data directory (blocks, chain state, pending markers), shared
//!   with a running node through atomic file writes
//! - the Sui settlement registry (settled height, per-height blob commitments)
//! - the Walrus aggregator (blob contents)
//!
//! No keys are loaded, nothing is signed or sent, and the Engine API is never
//! touched.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{error, info};

use crate::config::Config;
use crate::store::{BlockStore, ChainState, PendingStore, StateStore, StoreError};

/// Latest MCP protocol version understood by this server.
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
/// Protocol versions accepted over the streamable HTTP transport.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];
/// Requests with a larger body are rejected.
const MAX_BODY_BYTES: u64 = 1 << 20;
/// Requests with a larger header block are rejected.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Bound for each Sui settlement read so a stalled RPC cannot hang a tool.
const SUI_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Default bind address for `podseq mcp`.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9101";

/// Text result of a tool call, shown to the LLM client.
struct ToolOutcome {
    text: String,
    is_error: bool,
}

/// Read-only view over one podseq node: its data directory, its Sui
/// settlement registry, its Walrus aggregator, and its bridge vault.
pub struct McpServer {
    mode: String,
    data_dir: PathBuf,
    registry_id: Option<String>,
    settlement_package_id: Option<String>,
    bridge_vault_id: Option<String>,
    sui: podseq_sui::Client,
    /// Cached commitments-table UID. Fixed at contract `initialize`, so it is
    /// fetched once and reused for every `commitment_at` lookup.
    table_uid: std::sync::Mutex<Option<sui_sdk_types::Address>>,
}

impl McpServer {
    /// Builds a server inspecting the locations named in the node config.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let sui = podseq_sui::Client::new(podseq_sui::Config {
            publisher_url: config.walrus.publisher_url.clone(),
            aggregator_url: config.walrus.aggregator_url.clone(),
            epochs: config.walrus.epochs,
            sui_rpc_url: config.sui.rpc_url.clone(),
            publisher_auth_token: config.walrus.publisher_auth_token.clone(),
        })?;
        Ok(Self {
            mode: config.mode.clone(),
            data_dir: config.data_dir.clone(),
            registry_id: config.sui.registry_id.clone(),
            settlement_package_id: config.sui.settlement_package_id.clone(),
            bridge_vault_id: config.bridge.vault_id.clone(),
            sui,
            table_uid: std::sync::Mutex::new(None),
        })
    }

    /// Handles one JSON-RPC message. Returns `None` for notifications
    /// (messages without an `id`), which are acknowledged with 202 and no body.
    async fn dispatch(&self, message: &Value) -> Option<Value> {
        let id = match message.get("id") {
            Some(id) => id.clone(),
            None => return None, // notification
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Some(err_response(id, -32600, "missing method"));
        };

        match method {
            "initialize" => {
                let requested = message
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str);
                let negotiated = requested
                    .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                    .unwrap_or(LATEST_PROTOCOL_VERSION);
                Some(ok_response(
                    &id,
                    json!({
                        "protocolVersion": negotiated,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {
                            "name": "podseq",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": "Read-only inspection of a podseq node: local store, Sui settlement, Walrus data availability, and the bridge. Nothing is signed or sent.",
                    }),
                ))
            }
            "ping" => Some(ok_response(&id, json!({}))),
            "tools/list" => Some(ok_response(&id, json!({"tools": tools_metadata()}))),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.call_tool(name, &arguments).await {
                    Some(outcome) => Some(ok_response(
                        &id,
                        json!({
                            "content": [{"type": "text", "text": outcome.text}],
                            "isError": outcome.is_error,
                        }),
                    )),
                    None => Some(err_response(id, -32602, &format!("unknown tool: {name}"))),
                }
            }
            _ => Some(err_response(
                id,
                -32601,
                &format!("method not found: {method}"),
            )),
        }
    }

    /// Runs a tool by name. Returns `None` when the tool does not exist.
    async fn call_tool(&self, name: &str, args: &Value) -> Option<ToolOutcome> {
        match name {
            "status" => Some(self.tool_status().await),
            "get_block" => Some(self.tool_get_block(args).await),
            "get_blob" => Some(self.tool_get_blob(args).await),
            "settlement" => Some(self.tool_settlement(args).await),
            "bridge" => Some(self.tool_bridge().await),
            _ => None,
        }
    }

    /// Node status: chain state, store stats, pending finalization, and
    /// settlement progress (settled height and lag behind the local head).
    async fn tool_status(&self) -> ToolOutcome {
        let state = StateStore::new(&self.data_dir).load();
        let pending = PendingStore::new(&self.data_dir).pending();
        let (chain, pending_heights) = match (state, pending) {
            (Ok(state), Ok(pending)) => (state.map(|s| chain_json(&s)), pending),
            (Err(e), _) => return error_outcome(&format!("reading store: {e}")),
            (Ok(_), Err(e)) => return error_outcome(&format!("reading pending markers: {e}")),
        };

        let block_store = BlockStore::new(&self.data_dir);
        let latest_block = block_store.latest_height();
        let stored_blocks = stored_block_count(&self.data_dir);

        let settlement = match &self.registry_id {
            None => json!({"configured": false}),
            Some(registry) => {
                let mut section = json!({
                    "configured": true,
                    "registry_id": registry,
                    "package_id": self.settlement_package_id,
                });
                match timeout_ok(podseq_sui::settlement::latest_height(
                    self.sui.rpc_url(),
                    registry,
                ))
                .await
                {
                    Ok(settled) => {
                        section["settled_height"] = json!(settled);
                        section["lag"] = latest_block
                            .map(|latest| json!(latest.saturating_sub(settled)))
                            .unwrap_or(Value::Null);
                    }
                    Err(e) => section["error"] = json!(e),
                }
                section
            }
        };

        let status = json!({
            "mode": self.mode,
            "data_dir": self.data_dir.display().to_string(),
            "chain": chain,
            "latest_block": latest_block,
            "stored_blocks": stored_blocks,
            "pending_heights": pending_heights,
            "settlement": settlement,
            "bridge": {
                "configured": self.bridge_vault_id.is_some(),
                "vault_id": self.bridge_vault_id,
            },
        });
        ToolOutcome {
            text: pretty(&status),
            is_error: false,
        }
    }

    /// A stored podseq block by height, with its finalization state and, when
    /// the registry is configured, the Walrus blob id it was settled into.
    async fn tool_get_block(&self, args: &Value) -> ToolOutcome {
        let Some(height) = args.get("height").and_then(Value::as_u64) else {
            return error_outcome("missing required argument: height (integer)");
        };
        let block = match BlockStore::new(&self.data_dir).get(height) {
            Ok(block) => block,
            Err(StoreError::BlockNotFound(_)) => {
                return ToolOutcome {
                    text: format!("Block {height} not found in the local store"),
                    is_error: false,
                }
            }
            Err(e) => return error_outcome(&format!("reading block: {e}")),
        };

        let pending_finalization = PendingStore::new(&self.data_dir)
            .pending()
            .map(|heights| heights.contains(&height))
            .unwrap_or(false);

        let (settled_blob_id, settlement_error) = self.blob_id_for_height(height).await;

        let mut result = json!({
            "height": block.header.height,
            "parent_hash": hex0x(&block.header.parent_hash),
            "state_root": hex0x(&block.header.state_root),
            "timestamp": block.header.timestamp,
            "data_len": block.data.len(),
            "signed": block.signature.is_some(),
            "pending_finalization": pending_finalization,
            "settled_blob_id": settled_blob_id,
        });
        if let Some(e) = settlement_error {
            result["settlement_error"] = json!(e);
        }
        ToolOutcome {
            text: pretty(&result),
            is_error: false,
        }
    }

    /// A Walrus blob by id: size plus the podseq block batch it carries,
    /// decoded from the on-blob wire format.
    async fn tool_get_blob(&self, args: &Value) -> ToolOutcome {
        let Some(blob_id) = args.get("blob_id").and_then(Value::as_str) else {
            return error_outcome("missing required argument: blob_id (base64url)");
        };
        let id = match podseq_sui::blob_id::decode(blob_id) {
            Ok(id) => id,
            Err(e) => return error_outcome(&format!("invalid blob_id: {e}")),
        };
        let bytes = match self.sui.fetch_blob(&id).await {
            Ok(bytes) => bytes,
            Err(e) => return error_outcome(&format!("fetching blob from Walrus: {e}")),
        };
        match podseq_sui::wire::decode(&bytes) {
            Ok(blocks) => {
                let result = json!({
                    "blob_id": blob_id,
                    "size": bytes.len(),
                    "block_count": blocks.len(),
                    "blocks": blocks.iter().map(block_brief).collect::<Vec<_>>(),
                });
                ToolOutcome {
                    text: pretty(&result),
                    is_error: false,
                }
            }
            Err(_) => ToolOutcome {
                text: pretty(&json!({
                    "blob_id": blob_id,
                    "size": bytes.len(),
                    "decoded": false,
                })),
                is_error: false,
            },
        }
    }

    /// Settlement progress: the latest settled height, and optionally the
    /// blob commitment for one height.
    async fn tool_settlement(&self, args: &Value) -> ToolOutcome {
        let Some(registry) = self.registry_id.clone() else {
            return error_outcome("settlement not configured (set sui.registry_id in the config)");
        };
        let latest = match timeout_ok(podseq_sui::settlement::latest_height(
            self.sui.rpc_url(),
            &registry,
        ))
        .await
        {
            Ok(latest) => latest,
            Err(e) => return error_outcome(&format!("reading settlement registry: {e}")),
        };

        let mut result = json!({
            "registry_id": registry,
            "package_id": self.settlement_package_id,
            "latest_settled_height": latest,
        });
        if let Some(height) = args.get("height").and_then(Value::as_u64) {
            result["commitment"] = match self.blob_id_for_height(height).await {
                (Value::Null, None) => json!({"height": height, "blob_id": Value::Null}),
                (blob_id, _) => json!({"height": height, "blob_id": blob_id}),
            };
        }
        ToolOutcome {
            text: pretty(&result),
            is_error: false,
        }
    }

    /// Bridge vault state: the next deposit and withdraw nonce on Sui.
    async fn tool_bridge(&self) -> ToolOutcome {
        let Some(vault) = self.bridge_vault_id.clone() else {
            return error_outcome("bridge not configured (set bridge.vault_id in the config)");
        };
        match timeout_ok(podseq_sui::bridge::vault_status(self.sui.rpc_url(), &vault)).await {
            Ok(status) => {
                let result = json!({
                    "vault_id": vault,
                    "next_deposit_nonce": status.deposit_nonce,
                    "next_withdraw_nonce": status.withdraw_nonce,
                });
                ToolOutcome {
                    text: pretty(&result),
                    is_error: false,
                }
            }
            Err(e) => error_outcome(&format!("reading bridge vault: {e}")),
        }
    }

    /// Looks up the settled blob id for `height`. Returns `(null, None)` when
    /// settlement is not configured or the height is not settled; an error
    /// message is returned only when the lookup itself failed.
    async fn blob_id_for_height(&self, height: u64) -> (Value, Option<String>) {
        if self.registry_id.is_none() {
            return (Value::Null, None);
        }
        match self.commitments_table_uid().await {
            Ok(uid) => {
                match timeout_ok(podseq_sui::settlement::commitment_at(
                    self.sui.rpc_url(),
                    &uid,
                    height,
                ))
                .await
                {
                    Ok(Some(blob)) => (json!(podseq_sui::blob_id::encode(&blob)), None),
                    Ok(None) => (Value::Null, None),
                    Err(e) => (Value::Null, Some(e)),
                }
            }
            Err(e) => (Value::Null, Some(e)),
        }
    }

    /// The registry's commitments-table UID, cached after the first read.
    async fn commitments_table_uid(&self) -> Result<sui_sdk_types::Address, String> {
        if let Some(uid) = *self.table_uid.lock().expect("table uid lock") {
            return Ok(uid);
        }
        let registry = self
            .registry_id
            .clone()
            .ok_or("settlement not configured")?;
        let uid = timeout_ok(podseq_sui::settlement::table_uid(
            self.sui.rpc_url(),
            &registry,
        ))
        .await?;
        *self.table_uid.lock().expect("table uid lock") = Some(uid);
        Ok(uid)
    }
}

/// Runs the endpoint until Ctrl+C. Owns its tokio runtime, mirroring how the
/// `status` command builds a one-shot runtime for its queries.
pub fn run(server: McpServer, addr: SocketAddr) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(serve(Arc::new(server), addr, Arc::clone(&shutdown)));
        tokio::signal::ctrl_c().await?;
        shutdown.notify_waiters();
        let _ = task.await;
        Ok::<(), anyhow::Error>(())
    })
}

/// Serves the MCP endpoint on a TCP listener until `shutdown` is signalled.
pub async fn serve(server: Arc<McpServer>, addr: SocketAddr, shutdown: Arc<Notify>) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to bind mcp endpoint");
            return;
        }
    };
    info!(addr = %addr, "mcp endpoint listening");
    serve_on(listener, server, shutdown).await;
}

/// Accept loop core, separated from `serve` so tests can pass their own
/// listener and inspect the bound port.
async fn serve_on(listener: TcpListener, server: Arc<McpServer>, shutdown: Arc<Notify>) {
    loop {
        let (stream, _peer) = match tokio::select! {
            result = listener.accept() => result,
            _ = shutdown.notified() => break,
        } {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "mcp: accept failed");
                continue;
            }
        };

        let server = Arc::clone(&server);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &server).await {
                error!(error = %e, "mcp: connection handler failed");
            }
        });
    }
    info!("mcp endpoint stopped");
}

/// An HTTP request as needed for routing: method, path, and lowercase headers.
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Reads one HTTP request from the connection and writes the MCP response.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    server: &McpServer,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let Some(request) = read_request(&mut reader).await? else {
        return Ok(()); // connection closed before a request arrived
    };

    if request.path != "/mcp" {
        write_response(
            &mut writer,
            "404 Not Found",
            "text/plain",
            None,
            &[],
            b"not found",
        )
        .await?;
        return Ok(());
    }
    if request.method != "POST" {
        write_response(
            &mut writer,
            "405 Method Not Allowed",
            "text/plain",
            None,
            &[("Allow", "POST")],
            b"POST JSON-RPC messages only",
        )
        .await?;
        return Ok(());
    }

    // Stateless server: no sessions, so version negotiation happens per request.
    let client_version = request.header("mcp-protocol-version");
    if let Some(version) = client_version {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            write_response(
                &mut writer,
                "400 Bad Request",
                "text/plain",
                None,
                &[],
                format!("unsupported MCP-Protocol-Version: {version}").as_bytes(),
            )
            .await?;
            return Ok(());
        }
    }

    if request.header("transfer-encoding").is_some() {
        write_response(
            &mut writer,
            "400 Bad Request",
            "text/plain",
            None,
            &[],
            b"chunked bodies are not supported",
        )
        .await?;
        return Ok(());
    }
    let Some(content_length) = request.content_length else {
        write_response(
            &mut writer,
            "400 Bad Request",
            "text/plain",
            None,
            &[],
            b"content-length required",
        )
        .await?;
        return Ok(());
    };
    if content_length > MAX_BODY_BYTES {
        write_response(
            &mut writer,
            "413 Content Too Large",
            "text/plain",
            None,
            &[],
            b"body too large",
        )
        .await?;
        return Ok(());
    }

    let mut body = Vec::with_capacity(content_length as usize);
    let read = (&mut reader)
        .take(content_length)
        .read_to_end(&mut body)
        .await?;
    if (read as u64) < content_length {
        anyhow::bail!("client closed the connection mid-body");
    }

    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        write_response(
            &mut writer,
            "400 Bad Request",
            "text/plain",
            None,
            &[],
            b"invalid JSON",
        )
        .await?;
        return Ok(());
    };

    let (responses, was_array) = match message {
        Value::Array(entries) => {
            let mut responses = Vec::with_capacity(entries.len());
            for entry in &entries {
                match entry {
                    Value::Object(_) => responses.extend(server.dispatch(entry).await),
                    _ => responses.push(err_response(Value::Null, -32600, "invalid request")),
                }
            }
            (responses, true)
        }
        Value::Object(_) => {
            let responses = server
                .dispatch(&message)
                .await
                .into_iter()
                .collect::<Vec<_>>();
            (responses, false)
        }
        _ => {
            write_response(
                &mut writer,
                "400 Bad Request",
                "text/plain",
                None,
                &[],
                b"expected a JSON object or array",
            )
            .await?;
            return Ok(());
        }
    };

    // Version advertised on every response: the client's when supported,
    // otherwise the latest this server speaks.
    let version_header = client_version
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    if responses.is_empty() {
        // Notifications only: acknowledged, nothing to return.
        write_response(
            &mut writer,
            "202 Accepted",
            "text/plain",
            Some(version_header),
            &[],
            b"",
        )
        .await?;
    } else {
        let response = if was_array {
            Value::Array(responses)
        } else {
            responses.into_iter().next().expect("non-empty")
        };
        write_response(
            &mut writer,
            "200 OK",
            "application/json",
            Some(version_header),
            &[],
            response.to_string().as_bytes(),
        )
        .await?;
    }
    Ok(())
}

/// Reads the request line and headers. Returns `None` when the peer closes
/// before sending anything.
async fn read_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> anyhow::Result<Option<HttpRequest>> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("connection closed mid-headers");
        }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            anyhow::bail!("header block too large");
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" {
                content_length = value.trim().parse::<u64>().ok();
            }
            headers.push((name, value.trim().to_string()));
        }
    }

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        content_length,
    }))
}

/// Writes an HTTP/1.1 response with `Connection: close` semantics.
async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    status: &str,
    content_type: &str,
    protocol_version: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> anyhow::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(version) = protocol_version {
        response.push_str("MCP-Protocol-Version: ");
        response.push_str(version);
        response.push_str("\r\n");
    }
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("Connection: close\r\n\r\n");

    writer.write_all(response.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Tool catalogue advertised through `tools/list`.
fn tools_metadata() -> Value {
    json!([
        {
            "name": "status",
            "description": "Podseq node status: mode, chain head from the local store, stored block count, heights pending finalization, and settlement progress (settled height, lag).",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "get_block",
            "description": "Fetch a podseq block from the local store by height: header hashes, timestamp, payload size, signature presence, whether it is still pending finalization, and the Walrus blob id it settled into (when configured).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "height": {
                        "description": "Block height.",
                        "type": "integer",
                    },
                },
                "required": ["height"],
            },
        },
        {
            "name": "get_blob",
            "description": "Fetch a Walrus blob by id from the aggregator and decode the podseq block batch it carries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "blob_id": {
                        "description": "Walrus blob id, base64url (as logged by the node and reported by get_block).",
                        "type": "string",
                    },
                },
                "required": ["blob_id"],
            },
        },
        {
            "name": "settlement",
            "description": "Sui settlement registry: latest settled height, and optionally the Walrus blob commitment for one height.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "height": {
                        "description": "Height to look up the settled blob id for. Optional.",
                        "type": "integer",
                    },
                },
            },
        },
        {
            "name": "bridge",
            "description": "Bridge vault state on Sui: next deposit and withdraw nonce.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ])
}

/// `ChainState` as reported by the `status` tool.
fn chain_json(state: &ChainState) -> Value {
    json!({
        "height": state.height,
        "head": state.head,
        "safe": state.safe,
        "finalized": state.finalized,
        "timestamp": state.timestamp,
    })
}

/// One-line block summary used inside blob listings.
fn block_brief(block: &podseq_core::Block) -> Value {
    json!({
        "height": block.header.height,
        "timestamp": block.header.timestamp,
        "data_len": block.data.len(),
        "signed": block.signature.is_some(),
    })
}

/// Counts block files under `data_dir/blocks`; 0 when the directory is absent.
fn stored_block_count(data_dir: &Path) -> usize {
    std::fs::read_dir(data_dir.join("blocks"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().parse::<u64>().is_ok())
                .count()
        })
        .unwrap_or(0)
}

/// Formats a 32-byte hash as 0x-prefixed hex.
fn hex0x(bytes: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// Bounded wrapper for Sui reads: maps transport and settlement errors to a
/// message and fails after [`SUI_READ_TIMEOUT`] instead of hanging the tool.
async fn timeout_ok<T, E: std::fmt::Display, F>(fut: F) -> Result<T, String>
where
    F: Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(SUI_READ_TIMEOUT, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "sui rpc timed out after {}s",
            SUI_READ_TIMEOUT.as_secs()
        )),
    }
}

/// Builds a JSON-RPC success response.
fn ok_response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Builds a JSON-RPC error response.
fn err_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Pretty-serializes a tool payload for the text content block.
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Wraps a message into a failing tool outcome.
fn error_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        text: message.to_string(),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // dispatch: protocol surface
    // ------------------------------------------------------------------

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-mcp-test-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_server(data_dir: &Path) -> McpServer {
        let sui = podseq_sui::Client::new(podseq_sui::Config {
            publisher_url: "http://127.0.0.1:1".into(),
            aggregator_url: "http://127.0.0.1:1".into(),
            epochs: 53,
            sui_rpc_url: "http://127.0.0.1:1".into(),
            publisher_auth_token: None,
        })
        .unwrap();
        McpServer {
            mode: "sequencer".into(),
            data_dir: data_dir.to_path_buf(),
            registry_id: None,
            settlement_package_id: None,
            bridge_vault_id: None,
            sui,
            table_uid: std::sync::Mutex::new(None),
        }
    }

    fn sample_block(height: u64) -> podseq_core::Block {
        podseq_core::Block {
            header: podseq_core::Header {
                height,
                parent_hash: [height as u8; 32],
                state_root: [(height + 1) as u8; 32],
                timestamp: 1_700_000_000 + height,
            },
            data: vec![0u8; (height * 10) as usize],
            signature: Some([7u8; 64]),
        }
    }

    /// Two stored blocks, chain state at height 2, height 2 pending.
    fn seed(dir: &Path) {
        let blocks = BlockStore::new(dir);
        blocks.put(&sample_block(1)).unwrap();
        blocks.put(&sample_block(2)).unwrap();
        StateStore::new(dir)
            .save(&ChainState {
                head: "0xaa".into(),
                safe: "0xaa".into(),
                finalized: "0x99".into(),
                height: 2,
                timestamp: 1_700_000_002,
            })
            .unwrap();
        PendingStore::new(dir).mark(2).unwrap();
    }

    fn request(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    async fn call(s: &McpServer, name: &str, args: &Value) -> ToolOutcome {
        s.call_tool(name, args).await.expect("tool exists")
    }

    fn parse_text(outcome: &ToolOutcome) -> Value {
        serde_json::from_str(&outcome.text).expect("tool output is JSON")
    }

    #[tokio::test]
    async fn initialize_echoes_supported_version() {
        let dir = tmp_dir("init");
        let s = test_server(&dir);
        let resp = s
            .dispatch(&request(
                "initialize",
                json!({"protocolVersion": "2025-03-26"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
    }

    #[tokio::test]
    async fn initialize_falls_back_to_latest_for_unknown_version() {
        let dir = tmp_dir("init2");
        let s = test_server(&dir);
        let resp = s
            .dispatch(&request(
                "initialize",
                json!({"protocolVersion": "1999-01-01"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn initialize_result_shape() {
        let dir = tmp_dir("init3");
        let s = test_server(&dir);
        let resp = s
            .dispatch(&request(
                "initialize",
                json!({"protocolVersion": LATEST_PROTOCOL_VERSION}),
            ))
            .await
            .unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "podseq");
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let dir = tmp_dir("ping");
        let s = test_server(&dir);
        let resp = s.dispatch(&request("ping", json!({}))).await.unwrap();
        assert!(resp["result"].as_object().unwrap().is_empty());
        assert!(resp.get("error").is_none());
    }

    #[tokio::test]
    async fn unknown_method_returns_32601() {
        let dir = tmp_dir("unknown");
        let s = test_server(&dir);
        let resp = s
            .dispatch(&request("no/such/method", json!({})))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn message_without_id_is_a_notification() {
        let dir = tmp_dir("notif");
        let s = test_server(&dir);
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(s.dispatch(&msg).await.is_none());
    }

    #[tokio::test]
    async fn message_without_method_is_invalid_request() {
        let dir = tmp_dir("nomethod");
        let s = test_server(&dir);
        let msg = json!({"jsonrpc": "2.0", "id": 3});
        let resp = s.dispatch(&msg).await.unwrap();
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn tools_list_advertises_expected_names() {
        let dir = tmp_dir("tools");
        let s = test_server(&dir);
        let resp = s.dispatch(&request("tools/list", json!({}))).await.unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["status", "get_block", "get_blob", "settlement", "bridge"]
        );
        for tool in resp["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn unknown_tool_returns_32602() {
        let dir = tmp_dir("unknowntool");
        let s = test_server(&dir);
        let resp = s
            .dispatch(&request(
                "tools/call",
                json!({"name": "steal_keys", "arguments": {}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn responses_carry_jsonrpc_version_and_id() {
        let dir = tmp_dir("shape");
        let s = test_server(&dir);
        let resp = s.dispatch(&request("ping", json!({}))).await.unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
    }

    // ------------------------------------------------------------------
    // status
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn status_reflects_store_and_reports_settlement_unconfigured() {
        let dir = tmp_dir("status");
        seed(&dir);
        let s = test_server(&dir);
        let outcome = call(&s, "status", &json!({})).await;
        assert!(!outcome.is_error);
        let status = parse_text(&outcome);
        assert_eq!(status["mode"], "sequencer");
        assert_eq!(status["chain"]["height"], 2);
        assert_eq!(status["chain"]["finalized"], "0x99");
        assert_eq!(status["latest_block"], 2);
        assert_eq!(status["stored_blocks"], 2);
        assert_eq!(status["pending_heights"], json!([2]));
        assert_eq!(status["settlement"]["configured"], false);
        assert_eq!(status["bridge"]["configured"], false);
    }

    #[tokio::test]
    async fn status_on_empty_data_dir_is_valid() {
        let dir = tmp_dir("status-empty");
        let s = test_server(&dir);
        let outcome = call(&s, "status", &json!({})).await;
        assert!(!outcome.is_error);
        let status = parse_text(&outcome);
        assert_eq!(status["chain"], Value::Null);
        assert_eq!(status["latest_block"], Value::Null);
        assert_eq!(status["stored_blocks"], 0);
        assert_eq!(status["pending_heights"], json!([]));
    }

    #[tokio::test]
    async fn status_reports_settlement_read_errors_as_data() {
        let dir = tmp_dir("status-err");
        seed(&dir);
        let mut s = test_server(&dir);
        s.registry_id =
            Some("0xbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbad".into());
        let outcome = call(&s, "status", &json!({})).await;
        assert!(!outcome.is_error);
        let status = parse_text(&outcome);
        assert_eq!(status["settlement"]["configured"], true);
        assert!(status["settlement"]["error"].is_string());
        assert!(status["settlement"]["settled_height"].is_null());
    }

    // ------------------------------------------------------------------
    // get_block
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn get_block_returns_header_and_finalization_state() {
        let dir = tmp_dir("block");
        seed(&dir);
        let s = test_server(&dir);
        let outcome = call(&s, "get_block", &json!({"height": 1})).await;
        assert!(!outcome.is_error);
        let block = parse_text(&outcome);
        assert_eq!(block["height"], 1);
        assert_eq!(
            block["parent_hash"],
            format!("0x{}", hex::encode([1u8; 32]))
        );
        assert_eq!(block["state_root"], format!("0x{}", hex::encode([2u8; 32])));
        assert_eq!(block["timestamp"], 1_700_000_001);
        assert_eq!(block["data_len"], 10);
        assert_eq!(block["signed"], true);
        assert_eq!(block["pending_finalization"], false);
        assert_eq!(block["settled_blob_id"], Value::Null);
    }

    #[tokio::test]
    async fn get_block_marks_pending_height() {
        let dir = tmp_dir("block-pending");
        seed(&dir);
        let s = test_server(&dir);
        let outcome = call(&s, "get_block", &json!({"height": 2})).await;
        assert!(!outcome.is_error);
        assert_eq!(parse_text(&outcome)["pending_finalization"], true);
    }

    #[tokio::test]
    async fn get_block_missing_is_not_an_error() {
        let dir = tmp_dir("block-missing");
        seed(&dir);
        let s = test_server(&dir);
        let outcome = call(&s, "get_block", &json!({"height": 99})).await;
        assert!(!outcome.is_error);
        assert!(outcome.text.contains("not found"));
    }

    #[tokio::test]
    async fn get_block_requires_integer_height() {
        let dir = tmp_dir("block-arg");
        let s = test_server(&dir);
        let outcome = call(&s, "get_block", &json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("height"));
    }

    // ------------------------------------------------------------------
    // get_blob
    // ------------------------------------------------------------------

    /// Spawns a fake Walrus aggregator serving `body` for every GET.
    async fn spawn_mock_aggregator(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                // Drain request headers (GET has no body).
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    if line.trim_end().is_empty() {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                writer.write_all(response.as_bytes()).await.unwrap();
                writer.write_all(&body).await.unwrap();
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn get_blob_decodes_block_batch() {
        let batch = vec![sample_block(5), sample_block(6)];
        let body = podseq_sui::wire::encode(&batch).unwrap();
        let url = spawn_mock_aggregator(body).await;

        let dir = tmp_dir("blob");
        let mut s = test_server(&dir);
        s.sui = podseq_sui::Client::new(podseq_sui::Config {
            publisher_url: "http://127.0.0.1:1".into(),
            aggregator_url: url,
            epochs: 53,
            sui_rpc_url: "http://127.0.0.1:1".into(),
            publisher_auth_token: None,
        })
        .unwrap();

        let id = podseq_sui::blob_id::encode(&podseq_core::BlobId([9u8; 32]));
        let outcome = call(&s, "get_blob", &json!({"blob_id": id})).await;
        assert!(!outcome.is_error);
        let blob = parse_text(&outcome);
        assert_eq!(blob["block_count"], 2);
        assert_eq!(blob["blocks"][0]["height"], 5);
        assert_eq!(blob["blocks"][0]["data_len"], 50);
        assert_eq!(blob["blocks"][1]["height"], 6);
    }

    #[tokio::test]
    async fn get_blob_reports_undecodable_payload() {
        let url = spawn_mock_aggregator(b"garbage".to_vec()).await;
        let dir = tmp_dir("blob-raw");
        let mut s = test_server(&dir);
        s.sui = podseq_sui::Client::new(podseq_sui::Config {
            publisher_url: "http://127.0.0.1:1".into(),
            aggregator_url: url,
            epochs: 53,
            sui_rpc_url: "http://127.0.0.1:1".into(),
            publisher_auth_token: None,
        })
        .unwrap();

        let id = podseq_sui::blob_id::encode(&podseq_core::BlobId([9u8; 32]));
        let outcome = call(&s, "get_blob", &json!({"blob_id": id})).await;
        assert!(!outcome.is_error);
        let blob = parse_text(&outcome);
        assert_eq!(blob["decoded"], false);
        assert_eq!(blob["size"], 7);
    }

    #[tokio::test]
    async fn get_blob_rejects_invalid_blob_id() {
        let dir = tmp_dir("blob-bad");
        let s = test_server(&dir);
        let outcome = call(&s, "get_blob", &json!({"blob_id": "!!!not-base64!!!"})).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("blob_id"));
    }

    #[tokio::test]
    async fn get_blob_requires_blob_id() {
        let dir = tmp_dir("blob-arg");
        let s = test_server(&dir);
        let outcome = call(&s, "get_blob", &json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("blob_id"));
    }

    // ------------------------------------------------------------------
    // settlement / bridge: unconfigured paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn settlement_reports_unconfigured() {
        let dir = tmp_dir("settle");
        let s = test_server(&dir);
        let outcome = call(&s, "settlement", &json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("not configured"));
    }

    #[tokio::test]
    async fn bridge_reports_unconfigured() {
        let dir = tmp_dir("bridge");
        let s = test_server(&dir);
        let outcome = call(&s, "bridge", &json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("not configured"));
    }

    // ------------------------------------------------------------------
    // HTTP layer
    // ------------------------------------------------------------------

    async fn exchange(addr: SocketAddr, raw: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn post(path: &str, body: &str, extra_headers: &[(&str, &str)]) -> String {
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in extra_headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        request
    }

    async fn spawn_test_server() -> (SocketAddr, Arc<Notify>, PathBuf) {
        let dir = tmp_dir("http");
        seed(&dir);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(Notify::new());
        tokio::spawn(serve_on(
            listener,
            Arc::new(test_server(&dir)),
            Arc::clone(&shutdown),
        ));
        (addr, shutdown, dir)
    }

    #[tokio::test]
    async fn http_post_tools_list_returns_200_and_tools() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let response = exchange(addr, &post("/mcp", body, &[])).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("application/json"), "{response}");
        assert!(response.contains("MCP-Protocol-Version:"), "{response}");
        let json_part = response.split("\r\n\r\n").nth(1).unwrap();
        let value: Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(value["result"]["tools"].as_array().unwrap().len(), 5);
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_post_status_tool_serves_stored_state() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let response = exchange(addr, &post("/mcp", body, &[])).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        let value: Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let text = value["result"]["content"][0]["text"].as_str().unwrap();
        let status: Value = serde_json::from_str(text).unwrap();
        assert_eq!(status["latest_block"], 2);
        assert_eq!(status["pending_heights"], json!([2]));
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_get_returns_405_with_allow_post() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let response = exchange(addr, "GET /mcp HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        assert!(
            response.to_lowercase().contains("allow: post"),
            "{response}"
        );
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_post_wrong_path_returns_404() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let response = exchange(addr, &post("/elsewhere", "{}", &[])).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_invalid_json_returns_400() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let response = exchange(addr, &post("/mcp", "not json", &[])).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_non_object_json_returns_400() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let response = exchange(addr, &post("/mcp", "42", &[])).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_missing_content_length_returns_400() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let response = exchange(addr, "POST /mcp HTTP/1.1\r\nHost: localhost\r\n\r\n{}").await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_unsupported_protocol_version_header_returns_400() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let response = exchange(
            addr,
            &post("/mcp", body, &[("MCP-Protocol-Version", "1999-01-01")]),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_notification_returns_202_with_empty_body() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let response = exchange(addr, &post("/mcp", body, &[])).await;
        assert!(response.starts_with("HTTP/1.1 202"), "{response}");
        let json_part = response.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(json_part, "");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_batch_of_notifications_returns_202() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"[{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}]"#;
        let response = exchange(addr, &post("/mcp", body, &[])).await;
        assert!(response.starts_with("HTTP/1.1 202"), "{response}");
        shutdown.notify_waiters();
    }

    #[tokio::test]
    async fn http_batch_with_invalid_entry_reports_it() {
        let (addr, shutdown, _dir) = spawn_test_server().await;
        let body = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},7]"#;
        let response = exchange(addr, &post("/mcp", body, &[])).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let json_part = response.split("\r\n\r\n").nth(1).unwrap();
        let value: Value = serde_json::from_str(json_part).unwrap();
        let entries = value.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0]["result"].is_object());
        assert_eq!(entries[1]["error"]["code"], -32600);
        shutdown.notify_waiters();
    }
}
