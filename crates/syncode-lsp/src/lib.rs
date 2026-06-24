//! SynCode 进程内 LSP 客户端 (架构 §4 代码智能 / §6.2 机制③)。
//!
//! 机制③「持久服务协议 RPC」: 启一个**常驻**语言服务器 (rust-analyzer 等), 用 JSON-RPC 反复对话,
//! 复用其**持久、增量维护的语义索引** —— code agent 真正甩开纯文本 agent 的地方 (符号全工程引用 /
//! 改完拉诊断自校验 / 改 X 影响谁), 而非对字节 grep 猜。
//!
//! 选型: **自建** —— `tokio::process` + 手写 Content-Length JSON-RPC 帧, 不走 async-lsp 框架
//! (与「完全掌控 round-trip、不引 SDK」原则一致, 同 DeepSeek 走 raw API)。
//!
//! 设计 (经一轮多视角对抗 review 加固):
//!   - **writer 任务**独占 `ChildStdin`, 所有写经 mpsc 入队串行发 —— 任何写都不阻塞 reader, 杜绝
//!     「持 stdin 锁跨 await 阻塞在满管道」的串行/死锁 (review #3)。
//!   - **reader 任务**解复用 response(id→oneshot)/server-request(最小回复)/notification
//!     (publishDiagnostics→诊断表, rust-analyzer/serverStatus→就绪)。reader 退出(EOF/crash)时
//!     **drain 所有 in-flight 等待者 + 置 dead + 就绪置 Dead** —— 服务器崩了请求立刻报错而非永久挂死
//!     (review #1/#2/#5)。
//!   - `request()` 带**单请求超时**兜底; 调用前查 `dead` 短路 (review #1/#5)。
//!   - 就绪靠 `serverStatus`(已 advertise `experimental.serverStatusNotification`, 否则永不触发,
//!     review #4); 3 态 `Ready{Initializing,Ready,Dead}`, crash 立即返回不白等超时 (review #2)。
//!   - path↔uri 走 `url` crate 标准编码, 两侧一致, 诊断键规范化 (review #7/#8/#37)。
//!   - 每 client 自带文档版本 + 内容哈希: didChange 前**去重**, 内容没变不重发 (review #20)。

pub mod lang;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};

/// Content-Length 上限 (防畸形/恶意头让我们一次性分配巨量内存, review #11)。
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// 单请求兜底超时 (即便 drain/dead 失效也不让一个 turn 永久挂死, review #1/#5)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("lsp transport io error: {0}")]
    Io(#[from] io::Error),
    #[error("lsp protocol error: {0}")]
    Protocol(String),
    #[error("language server returned an error: {0}")]
    Server(String),
    #[error("language server is no longer running")]
    Dead,
    #[error("language server request timed out")]
    Timeout,
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
    if len > MAX_MESSAGE_BYTES {
        return Err(LspError::Protocol(format!(
            "Content-Length {len} exceeds cap {MAX_MESSAGE_BYTES}"
        )));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| LspError::Protocol(e.to_string()))
}

// ---- path <-> file:// URI (标准编码, 两侧一致) ----

/// 本地绝对路径 → 规范 `file://` URI (经 `url` crate, 正确处理盘符 / 空格 / 非 ASCII)。
/// 失败 (如非绝对路径) 时回退到朴素拼接。
pub fn path_to_file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| {
            let s = path.to_string_lossy().replace('\\', "/");
            if s.starts_with('/') {
                format!("file://{s}")
            } else {
                format!("file:///{s}")
            }
        })
}

/// `file://` URI → 显示用本地路径 (保留盘符大小写; 经 url 解析, 失败回退原串)。
pub fn uri_to_path(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.to_string())
}

/// `file://` URI → 规范化的本地路径字符串 (诊断表的键, 让"我们构造的 uri"与"服务器回传的 uri"
/// 收敛到同一形式: 盘符小写 + 正斜杠 + percent-decode, review #7)。
pub fn uri_to_key(uri: &str) -> String {
    let path = url::Url::parse(uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.to_string());
    let mut s = path.replace('\\', "/");
    // 盘符小写, 使 C: 与 c: 等价。
    if s.len() >= 2 && &s[1..2] == ":" {
        let drive = s[..1].to_ascii_lowercase();
        s = format!("{drive}{}", &s[1..]);
    }
    s
}

// ---- 持久客户端 ----

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type Diagnostics = Arc<Mutex<HashMap<String, Vec<Value>>>>;

/// 服务器索引就绪态 (crash 可区分, 避免白等超时, review #2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ready {
    Initializing,
    Ready,
    Dead,
}

/// 一个已打开文档的同步状态: 版本号 + 内容哈希 (用于 didChange 去重, review #20)。
struct DocState {
    version: i64,
    hash: u64,
}

/// 常驻语言服务器的持久客户端。`spawn` 启动并完成 initialize 握手后即可并发查询。
pub struct LspClient {
    writer_tx: mpsc::UnboundedSender<Value>,
    pending: Pending,
    diagnostics: Diagnostics,
    docs: Arc<Mutex<HashMap<String, DocState>>>,
    next_id: AtomicI64,
    ready_rx: watch::Receiver<Ready>,
    dead: Arc<AtomicBool>,
    child: Child,
}

impl LspClient {
    /// 启动 `server` (如 `"rust-analyzer"`), 以 `root_uri` 为工程根做 initialize/initialized 握手。
    pub async fn spawn(server: &str, root_uri: &str) -> Result<Self, LspError> {
        Self::spawn_cmd(server, &[], root_uri).await
    }

    /// 同 [`spawn`], 但可给服务器传命令行参数 (如 `pyright-langserver --stdio`、`typescript-language-server --stdio`)。
    pub async fn spawn_cmd(server: &str, args: &[&str], root_uri: &str) -> Result<Self, LspError> {
        let mut child = Command::new(server)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true) // 别泄漏 ra 进程 (review #27)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let docs = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = watch::channel(Ready::Initializing);
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Value>();

        // writer 任务: 独占 ChildStdin, 串行写。任何写阻塞只发生在这里, 不卡 reader。
        tokio::spawn(async move {
            let mut stdin: ChildStdin = stdin;
            while let Some(msg) = writer_rx.recv().await {
                if write_message(&mut stdin, &msg).await.is_err() {
                    break;
                }
            }
        });

        // reader 任务: 解复用; 退出时 drain in-flight + 置 dead/Dead。
        {
            let pending = pending.clone();
            let diagnostics = diagnostics.clone();
            let dead = dead.clone();
            let writer_tx = writer_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader).await {
                        Ok(msg) => handle_incoming(msg, &pending, &diagnostics, &writer_tx, &ready_tx),
                        Err(_) => break, // 服务器关流 / crash
                    }
                }
                // reader 退出: 让所有 in-flight 请求立刻失败, 别永久挂死 (review #1)。
                dead.store(true, Ordering::SeqCst);
                for (_, tx) in pending.lock().unwrap().drain() {
                    drop(tx); // 关闭 channel → request() 的 rx.await 解析为 Err
                }
                let _ = ready_tx.send(Ready::Dead);
            });
        }

        let client = Self {
            writer_tx,
            pending,
            diagnostics,
            docs,
            next_id: AtomicI64::new(1),
            ready_rx,
            dead,
            child,
        };
        client.initialize(root_uri).await?;
        Ok(client)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn is_open(&self, uri: &str) -> bool {
        self.docs.lock().unwrap().contains_key(uri)
    }

    /// 入队一条消息给 writer 任务 (非阻塞; 不碰管道)。
    fn write(&self, msg: Value) -> Result<(), LspError> {
        self.writer_tx.send(msg).map_err(|_| LspError::Dead)
    }

    /// 发请求, 等对应 id 的响应。带超时 + dead 短路, 服务器崩了不挂死。可并发调用。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        if self.is_dead() {
            return Err(LspError::Dead);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // 先登记再发送 (否则极快的响应可能在登记前到达而丢失)。
        self.pending.lock().unwrap().insert(id, tx);
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(LspError::Dead), // sender 被 drop = reader 退出/crash
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(LspError::Timeout);
            }
        };
        if let Some(err) = resp.get("error") {
            // 优先取人读的 message, 没有再退回整个对象。
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            return Err(LspError::Server(msg));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.write(json!({"jsonrpc":"2.0","method":method,"params":params}))
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
                        "definition": {}, "references": {}, "hover": {},
                        "typeDefinition": {}, "implementation": {}
                    },
                    "window": { "workDoneProgress": true },
                    // 没有这个, rust-analyzer 不发 serverStatus, wait_until_ready 永远超时 (review #4)。
                    "experimental": { "serverStatusNotification": true }
                }
            }),
        )
        .await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// 把文档当前内容同步给服务器: 未打开 → didOpen; 已打开且内容变了 → didChange(全量);
    /// 内容未变 → 跳过 (不白触发服务器重分析, review #20)。
    pub async fn sync(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        enum Action {
            Open,
            Change(i64),
            Skip,
        }
        let h = hash_str(text);
        let action = {
            let mut docs = self.docs.lock().unwrap();
            match docs.get_mut(uri) {
                None => {
                    docs.insert(uri.to_string(), DocState { version: 1, hash: h });
                    Action::Open
                }
                Some(st) if st.hash == h => Action::Skip,
                Some(st) => {
                    st.version += 1;
                    st.hash = h;
                    Action::Change(st.version)
                }
            }
        };
        match action {
            Action::Open => {
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
                )
                .await
            }
            Action::Change(version) => {
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [ { "text": text } ]
                    }),
                )
                .await
            }
            Action::Skip => Ok(()),
        }
    }

    /// 等服务器索引就绪。`Ready` → true; `Dead` → 立刻 false (不白等); 超时 → false。
    pub async fn wait_until_ready(&self, timeout: Duration) -> bool {
        let mut rx = self.ready_rx.clone();
        let res = tokio::time::timeout(timeout, async {
            loop {
                match *rx.borrow() {
                    Ready::Ready => return true,
                    Ready::Dead => return false,
                    Ready::Initializing => {}
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await;
        matches!(res, Ok(true))
    }

    // ---- 语义查询 (位置参数 0-based, 工具层从 1-based 转入) ----

    pub async fn document_symbol(&self, uri: &str) -> Result<Value, LspError> {
        self.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }))
            .await
    }

    pub async fn workspace_symbol(&self, query: &str) -> Result<Value, LspError> {
        self.request("workspace/symbol", json!({ "query": query })).await
    }

    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.position_request("textDocument/definition", uri, line, character).await
    }

    pub async fn type_definition(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.position_request("textDocument/typeDefinition", uri, line, character).await
    }

    pub async fn implementation(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.position_request("textDocument/implementation", uri, line, character).await
    }

    pub async fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Value, LspError> {
        self.position_request("textDocument/hover", uri, line, character).await
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

    async fn position_request(
        &self,
        method: &str,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        self.request(
            method,
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
        .await
    }

    /// 某文件最近一次 `publishDiagnostics` 推送的诊断 (改完即时拉诊断自校验, §4)。
    /// 按规范化键查 (与服务器回传的 uri 形式收敛, review #7)。
    pub fn diagnostics_for(&self, uri: &str) -> Vec<Value> {
        self.diagnostics
            .lock()
            .unwrap()
            .get(&uri_to_key(uri))
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

/// 处理一条服务器来的消息 (reader 任务内, 同步; 写经 writer_tx 入队不阻塞)。
fn handle_incoming(
    msg: Value,
    pending: &Pending,
    diagnostics: &Diagnostics,
    writer_tx: &mpsc::UnboundedSender<Value>,
    ready_tx: &watch::Sender<Ready>,
) {
    let has_method = msg.get("method").is_some();
    let id = msg.get("id").and_then(Value::as_i64);

    if has_method {
        if let Some(sid) = id {
            // server→client 请求 (workspace/configuration 等) → 最小回复防死锁。
            let _ = writer_tx.send(json!({ "jsonrpc": "2.0", "id": sid, "result": null }));
        } else {
            match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                "textDocument/publishDiagnostics" => {
                    if let Some(p) = msg.get("params") {
                        if let Some(uri) = p.get("uri").and_then(Value::as_str) {
                            let diags = p
                                .get("diagnostics")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            diagnostics.lock().unwrap().insert(uri_to_key(uri), diags);
                        }
                    }
                }
                // ra 1.96 实测发的是 `experimental/serverStatus` (旧版是 `rust-analyzer/serverStatus`);
                // 两个都收, 跨版本稳。capability 是 `experimental.serverStatusNotification`。
                "experimental/serverStatus" | "rust-analyzer/serverStatus" => {
                    let params = msg.get("params");
                    let quiescent = params
                        .and_then(|p| p.get("quiescent"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let health = params
                        .and_then(|p| p.get("health"))
                        .and_then(Value::as_str)
                        .unwrap_or("ok");
                    // quiescent = 后台活儿干完; health 非 ok 时也放行 (否则持续报错的工程白等)。
                    if quiescent || health == "error" || health == "warning" {
                        let _ = ready_tx.send(Ready::Ready);
                    }
                }
                _ => {}
            }
        }
        return;
    }

    if let Some(id) = id {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg);
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---- 跨工具复用的管理器 ----

/// 跨工具调用复用的 LSP 客户端管理器: 按 `(工程根, 服务器)` **惰性启动并缓存**常驻 [`LspClient`]。
/// 多语言: 同一仓库里 Rust / Go / TS 各自一个常驻 server, 按 key 区分 (§5.3)。
/// 放进 `ToolCtx`, 与文件缓存一样跨工具调用共享 (持久活状态的前提)。
pub struct LspManager {
    /// key = (工程根, 服务器可执行名)。
    clients: tokio::sync::Mutex<HashMap<(PathBuf, String), Arc<LspClient>>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self { clients: tokio::sync::Mutex::new(HashMap::new()) }
    }

    /// 取 (或惰性启动并缓存) 某 `(工程根, 服务器)` 的客户端。缓存里若是**已死**的 → 踢掉重启 (review #9)。
    pub async fn client_for(
        &self,
        root: &Path,
        server: &str,
        args: &[&str],
    ) -> Result<Arc<LspClient>, LspError> {
        let key = (root.to_path_buf(), server.to_string());
        let mut map = self.clients.lock().await;
        if let Some(c) = map.get(&key) {
            if !c.is_dead() {
                return Ok(c.clone());
            }
            map.remove(&key); // 死了的不复用
        }
        let client = Arc::new(LspClient::spawn_cmd(server, args, &path_to_file_uri(root)).await?);
        map.insert(key, client.clone());
        Ok(client)
    }

    /// 文件落盘改动后调用 (Edit/Write/AstEdit 写完): 对**已打开该文档**的常驻 client 重读磁盘并推
    /// didChange, 让索引与编辑保持同步 (跨文件正确性 + 暖索引)。未打开的文档不强开。无 client 则 no-op。
    /// languageId 按文件扩展名定 (didChange 其实不依赖它, 但 sync 接口需要)。
    pub async fn notify_file_changed(&self, path: &Path) {
        let uri = path_to_file_uri(path);
        let lang_id = lang::language_id_for_path(path);
        let clients = self.clients.lock().await;
        for ((root, _server), client) in clients.iter() {
            if path.starts_with(root) && !client.is_dead() && client.is_open(&uri) {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let _ = client.sync(&uri, lang_id, &text).await;
                }
                return;
            }
        }
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
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

    // Windows: 盘符大小写 + percent-encoded 空格应收敛到同一键。
    // (C:/ 风格路径仅在 Windows 上被 url::from_file_path 视为绝对路径; 在 Unix 上它是相对路径、解析回退,
    //  且盘符语义对 Unix 无意义 —— 故本断言 Windows-only, macOS 见下方 uri_key_normalizes_encoding。)
    #[cfg(windows)]
    #[test]
    fn uri_key_normalizes_drive_case_and_encoding() {
        // 服务器风格 (小写盘符 + percent-encoded 空格) 与工具风格 (大写盘符 + 原始) 应收敛到同一键。
        let server = "file:///c:/Users/x/a%20b.rs";
        let tool = path_to_file_uri(Path::new("C:/Users/x/a b.rs"));
        assert_eq!(uri_to_key(server), uri_to_key(&tool), "tool uri = {tool}");
    }

    // Unix/macOS: 无盘符概念, 但 percent-encoded 空格仍须让「服务器回传」与「工具构造」的 uri 收敛同键
    // (诊断表键一致, 否则同一文件的诊断对不上)。用绝对 /Users 路径: from_file_path 在 Unix 上要求绝对路径。
    #[cfg(unix)]
    #[test]
    fn uri_key_normalizes_encoding() {
        let server = "file:///Users/x/a%20b.rs";
        let tool = path_to_file_uri(Path::new("/Users/x/a b.rs"));
        assert_eq!(uri_to_key(server), uri_to_key(&tool), "tool uri = {tool}");
    }

    #[test]
    fn path_uri_roundtrips_space_and_unicode() {
        let p = Path::new("C:/tmp/hello world/文件.rs");
        let uri = path_to_file_uri(p);
        assert!(uri.starts_with("file:///"), "{uri}");
        // 经 url 解析回路径, 规范化键应与直接对路径取键一致。
        assert_eq!(uri_to_key(&uri), uri_to_key(&path_to_file_uri(p)));
    }

    /// 持久 client 集成测试: spawn → sync → documentSymbol, 并**并发**三发证明 id 解复用不串线。
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
        client.sync(&file_uri, "rust", &text).await.unwrap();

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

        let (a, b, c) = tokio::join!(
            client.document_symbol(&file_uri),
            client.document_symbol(&file_uri),
            client.document_symbol(&file_uri),
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok(), "{a:?} {b:?} {c:?}");

        client.shutdown().await;
    }

    /// 验证就绪信号修复: `serverStatus` capability 没声明时 `wait_until_ready` 会超时返回 false;
    /// 声明后应在索引完成时返回 true。顺带证明索引可用 (按名字找到本 crate 符号)。慢, 默认 `#[ignore]`。
    #[tokio::test]
    #[ignore = "spawns rust-analyzer and indexes the workspace; slow"]
    async fn readiness_signal_resolves_and_workspace_symbol_works() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let ws_root = Path::new(manifest).parent().unwrap().parent().unwrap();
        let file = Path::new(manifest).join("src").join("lib.rs");
        let text = std::fs::read_to_string(&file).unwrap();
        let root_uri = path_to_file_uri(ws_root);
        let file_uri = path_to_file_uri(&file);

        let client = LspClient::spawn("rust-analyzer", &root_uri).await.expect("spawn");
        client.sync(&file_uri, "rust", &text).await.unwrap();

        // 关键断言: 就绪信号必须真的到达 (修复前这里会白等到超时返回 false)。
        let ready = client.wait_until_ready(Duration::from_secs(180)).await;
        assert!(ready, "wait_until_ready timed out — serverStatus not arriving");

        // 索引就绪后, 按名字找符号应命中本 crate 的类型。
        let syms = client.workspace_symbol("LspClient").await.unwrap();
        let names = symbol_names(&syms);
        println!("workspace_symbol(LspClient) → {names:?}");
        assert!(names.iter().any(|n| n == "LspClient"), "{names:?}");

        client.shutdown().await;
    }
}
