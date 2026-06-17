//! SynCode 进程内 LSP 客户端 (架构 §4 代码智能 / §6.2 机制③)。
//!
//! 机制③「持久服务协议 RPC」: 启一个**常驻**语言服务器 (rust-analyzer 等), 用 JSON-RPC 反复对话,
//! 复用其**持久、增量维护的语义索引** —— 这才是 code agent 真正甩开纯文本 agent 的地方
//! (符号全工程引用 / 改完拉诊断自校验 / 改 X 影响谁), 而非对字节 grep 猜。
//!
//! 选型 (与 SynCode「完全掌控 round-trip、不引 SDK」原则一致, 同 DeepSeek 走 raw API):
//! **自建** —— `tokio::process` 驱动服务器 + 手写 Content-Length JSON-RPC 帧, 不走 async-lsp 框架。
//!
//! 设计: [`LspClient`] 拥有子进程; 一个**后台 reader 任务**读取消息并解复用——
//!   - **response** (有 id 无 method) → 按 id 唤醒等待中的请求 (`oneshot`), 支持并发 in-flight;
//!   - **server→client request** (有 id 有 method, 如 `workspace/configuration`) → 最小回复, 不死锁;
//!   - **notification** (有 method 无 id) → `publishDiagnostics` 入诊断表 / `serverStatus` 置就绪。
//! 索引相关查询 (definition/references) 先 [`wait_until_ready`](LspClient::wait_until_ready);
//! 语法级查询 (documentSymbol) 不需要。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("lsp transport io error: {0}")]
    Io(#[from] io::Error),
    #[error("lsp protocol error: {0}")]
    Protocol(String),
    #[error("language server returned an error: {0}")]
    Server(String),
}

// ---- 传输层: Content-Length 帧 (LSP base protocol) ----

/// 写一条 JSON-RPC 消息: `Content-Length: N\r\n\r\n` + body。
pub async fn write_message<W>(w: &mut W, msg: &Value) -> Result<(), LspError>
where
    W: AsyncWriteExt + Unpin,
{
    let body = serde_json::to_vec(msg).map_err(|e| LspError::Protocol(e.to_string()))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// 读一条 JSON-RPC 消息: 先读 header (直到空行) 取 `Content-Length`, 再精确读 body。
pub async fn read_message<R>(r: &mut R) -> Result<Value, LspError>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Err(LspError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed the stream",
            )));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                v.trim()
                    .parse()
                    .map_err(|_| LspError::Protocol(format!("bad Content-Length: {v:?}")))?,
            );
        }
    }
    let len = content_length.ok_or_else(|| LspError::Protocol("missing Content-Length".into()))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| LspError::Protocol(e.to_string()))
}

/// 本地路径 → `file://` URI (v1: ASCII 路径, `\`→`/`, Windows 盘符补三斜杠)。
/// TODO: percent-encode 空格/非 ASCII。
pub fn path_to_file_uri(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

// ---- 持久客户端 ----

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type Diagnostics = Arc<Mutex<HashMap<String, Vec<Value>>>>;

/// 常驻语言服务器的持久客户端。`spawn` 启动并完成 initialize 握手后即可并发查询。
pub struct LspClient {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    diagnostics: Diagnostics,
    next_id: AtomicI64,
    ready_rx: watch::Receiver<bool>,
    child: Child,
}

impl LspClient {
    /// 启动 `server` (如 `"rust-analyzer"`), 以 `root_uri` 为工程根做 initialize/initialized 握手。
    pub async fn spawn(server: &str, root_uri: &str) -> Result<Self, LspError> {
        let mut child = Command::new(server)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdin = Arc::new(AsyncMutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let (ready_tx, ready_rx) = watch::channel(false);

        // 后台 reader: 解复用 response / server-request / notification。
        {
            let pending = pending.clone();
            let diagnostics = diagnostics.clone();
            let stdin = stdin.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader).await {
                        Ok(msg) => {
                            handle_incoming(msg, &pending, &diagnostics, &stdin, &ready_tx).await
                        }
                        Err(_) => break, // 服务器关流 → 退出
                    }
                }
            });
        }

        let client = Self {
            stdin,
            pending,
            diagnostics,
            next_id: AtomicI64::new(1),
            ready_rx,
            child,
        };
        client.initialize(root_uri).await?;
        Ok(client)
    }

    async fn write(&self, msg: Value) -> Result<(), LspError> {
        let mut s = self.stdin.lock().await;
        write_message(&mut *s, &msg).await
    }

    /// 发请求, 等到对应 id 的响应才返回。`result` 直出; `error` → `LspError::Server`。可并发调用。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // 必须**先**登记再发送: 否则极快的响应可能在登记前到达而丢失。
        self.pending.lock().unwrap().insert(id, tx);
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let resp = rx
            .await
            .map_err(|_| LspError::Protocol("response channel closed".into()))?;
        if let Some(err) = resp.get("error") {
            return Err(LspError::Server(err.to_string()));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.write(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn initialize(&self, root_uri: &str) -> Result<(), LspError> {
        self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "definition": {}, "references": {}, "hover": {}
                    },
                    "window": { "workDoneProgress": true }
                }
            }),
        )
        .await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// 告知服务器一个文档的当前内容 (语义查询的前提)。
    pub async fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text }
            }),
        )
        .await
    }

    /// 全量替换某文档内容 (TextDocumentSyncKind.Full)。模型改文件后用它让服务器看到最新内容。
    pub async fn did_change_full(&self, uri: &str, version: i64, text: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
        .await
    }

    /// 等服务器索引就绪 (rust-analyzer `serverStatus.quiescent==true`)。definition/references 这类
    /// **需索引**的查询前调; documentSymbol 不需要。超时返回 false。
    pub async fn wait_until_ready(&self, timeout: Duration) -> bool {
        let mut rx = self.ready_rx.clone();
        let res = tokio::time::timeout(timeout, async {
            loop {
                if *rx.borrow() {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        res.is_ok() && *self.ready_rx.borrow()
    }

    // ---- 语义查询 (位置参数为 0-based, 工具层从 1-based 转入) ----

    pub async fn document_symbol(&self, uri: &str) -> Result<Value, LspError> {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await
    }

    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
        .await
    }

    pub async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Value, LspError> {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": include_declaration }
            }),
        )
        .await
    }

    pub async fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
        .await
    }

    /// 某文件最近一次 `publishDiagnostics` 推送的诊断 (改完即时拉诊断自校验, §4)。
    pub fn diagnostics_for(&self, uri: &str) -> Vec<Value> {
        self.diagnostics
            .lock()
            .unwrap()
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    /// 优雅关闭: shutdown 请求 + exit 通知 + kill。
    pub async fn shutdown(mut self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = self.child.kill().await;
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // 兜底: 没显式 shutdown 也别泄漏 ra 进程 (start_kill 是同步的)。
        let _ = self.child.start_kill();
    }
}

/// 跨工具调用复用的 LSP 客户端管理器: 按工程根**惰性启动并缓存**常驻 [`LspClient`] (v1 = rust-analyzer),
/// 跟踪每个文档的打开/版本以做全量同步。放进 `ToolCtx`, 与文件缓存一样跨工具调用共享 (持久活状态的前提)。
pub struct LspManager {
    server: String,
    clients: AsyncMutex<HashMap<PathBuf, Arc<LspClient>>>,
    versions: AsyncMutex<HashMap<String, i64>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            server: "rust-analyzer".to_string(),
            clients: AsyncMutex::new(HashMap::new()),
            versions: AsyncMutex::new(HashMap::new()),
        }
    }

    /// 取 (或惰性启动并缓存) 某工程根的客户端。第一次会 spawn + initialize, 之后复用同一个常驻进程。
    pub async fn client_for_root(&self, root: &Path) -> Result<Arc<LspClient>, LspError> {
        let mut map = self.clients.lock().await;
        if let Some(c) = map.get(root) {
            return Ok(c.clone());
        }
        let client = Arc::new(LspClient::spawn(&self.server, &path_to_file_uri(root)).await?);
        map.insert(root.to_path_buf(), client.clone());
        Ok(client)
    }

    /// 把文档当前内容同步给服务器: 首次 `didOpen`, 之后 `didChange` (全量)。
    pub async fn sync_doc(
        &self,
        client: &LspClient,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<(), LspError> {
        let mut versions = self.versions.lock().await;
        match versions.get_mut(uri) {
            None => {
                client.did_open(uri, language_id, text).await?;
                versions.insert(uri.to_string(), 1);
            }
            Some(ver) => {
                *ver += 1;
                client.did_change_full(uri, *ver, text).await?;
            }
        }
        Ok(())
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 处理一条服务器来的消息 (reader 任务内)。
async fn handle_incoming(
    msg: Value,
    pending: &Pending,
    diagnostics: &Diagnostics,
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    ready_tx: &watch::Sender<bool>,
) {
    let has_method = msg.get("method").is_some();
    let id = msg.get("id").and_then(Value::as_i64);

    if has_method {
        if let Some(sid) = id {
            // server→client 请求 (workspace/configuration 等) → 最小回复以免死锁。
            let reply = json!({ "jsonrpc": "2.0", "id": sid, "result": null });
            let mut s = stdin.lock().await;
            let _ = write_message(&mut *s, &reply).await;
        } else {
            // notification。
            match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                "textDocument/publishDiagnostics" => {
                    if let Some(p) = msg.get("params") {
                        if let Some(uri) = p.get("uri").and_then(Value::as_str) {
                            let diags = p
                                .get("diagnostics")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            diagnostics.lock().unwrap().insert(uri.to_string(), diags);
                        }
                    }
                }
                "rust-analyzer/serverStatus" => {
                    let quiescent = msg
                        .get("params")
                        .and_then(|p| p.get("quiescent"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if quiescent {
                        let _ = ready_tx.send(true);
                    }
                }
                _ => {}
            }
        }
        return;
    }

    // response。
    if let Some(id) = id {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn symbol_names(result: &Value) -> Vec<String> {
        result
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 持久 client 集成测试: spawn → didOpen → documentSymbol, 并**并发**三发证明 id 解复用不串线。
    /// 慢 + 需本机装 rust-analyzer, 故默认 `#[ignore]`。
    /// 跑: `cargo test -p syncode-lsp -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "spawns rust-analyzer; slow and requires ra installed"]
    async fn persistent_client_documentsymbol_and_concurrency() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let ws_root = Path::new(manifest).parent().unwrap().parent().unwrap();
        let file = Path::new(manifest).join("src").join("lib.rs");
        let text = std::fs::read_to_string(&file).expect("read own lib.rs");
        let root_uri = path_to_file_uri(ws_root);
        let file_uri = path_to_file_uri(&file);

        let client = LspClient::spawn("rust-analyzer", &root_uri)
            .await
            .expect("spawn rust-analyzer (on PATH?)");
        client.did_open(&file_uri, "rust", &text).await.unwrap();

        // documentSymbol 是语法级查询; didOpen 后通常立刻可用, 偶发空则短暂重试。
        let mut names = Vec::new();
        for _ in 0..10 {
            let r = client.document_symbol(&file_uri).await.unwrap();
            names = symbol_names(&r);
            if !names.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        println!("documentSymbol → {} symbols: {names:?}", names.len());
        assert!(
            names.iter().any(|n| n == "write_message" || n == "read_message"),
            "expected our fn names: {names:?}"
        );

        // 并发三发: 证明后台 reader 的 id 解复用让多个 in-flight 请求各回各家。
        let (a, b, c) = tokio::join!(
            client.document_symbol(&file_uri),
            client.document_symbol(&file_uri),
            client.document_symbol(&file_uri),
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok(), "{a:?} {b:?} {c:?}");
        assert_eq!(symbol_names(&a.unwrap()).len(), names.len());

        client.shutdown().await;
    }
}
