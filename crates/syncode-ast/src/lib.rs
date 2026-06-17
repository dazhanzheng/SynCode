//! SynCode AST 引擎 (架构 §4): tree-sitter + ast-grep 驱动。
//!
//! 暴露三件能力, 供 `syncode-tools` 工具层与将来的语义/LSP 层复用:
//!   1. **结构化搜索** —— 按语法 pattern (ast-grep 语法, 如 `println!($$$)`) 搜, 拿 typed match
//!      + 行号。文本 grep 搜不到的"所有返回 `Result<_, _>` 的函数"这类它能搜 (§4 搜索 HIGH)。
//!   2. **结构化改写** —— pattern + rewrite 结构替换; 改完 re-parse, **引入新语法错就拒绝**
//!      —— 这就是"AST 级保证合法" (§4 编辑 MED-HIGH)。
//!   3. **语法校验** —— DFS 找 error/missing 节点, 给行编辑 `Edit` 当改后护栏。
//!
//! 语言由文件扩展名 (`.rs`→Rust) 或别名 (`"rust"`/`"py"`) 识别; ast-grep 内建 28 种语言。

use std::path::Path;

use ast_grep_core::source::Edit;
use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt};

pub use ast_grep_language::SupportLang;

#[derive(Debug, thiserror::Error)]
pub enum AstError {
    #[error("could not detect a supported language from the file extension")]
    UnknownLanguage,
    #[error("invalid AST pattern: {0}")]
    BadPattern(String),
    #[error("rewrite would introduce a {lang} syntax error ({detail}); change rejected")]
    InvalidRewrite { lang: String, detail: String },
}

/// 一处结构化匹配。行号 1-based (便于直接喂模型/人读), 列号 1-based 字符列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchHit {
    pub start_line: usize,
    pub end_line: usize,
    pub start_col: usize,
    pub text: String,
}

/// 绑定某语言的 AST 引擎。`SupportLang` 是 `Copy`, `Engine` 随之轻量可复制。
#[derive(Debug, Clone, Copy)]
pub struct Engine {
    lang: SupportLang,
}

impl Engine {
    /// 由文件路径扩展名识别语言 (.rs→Rust, .py→Python …)。识别不出 → `UnknownLanguage`。
    pub fn for_path(path: impl AsRef<Path>) -> Result<Self, AstError> {
        SupportLang::from_path(path.as_ref())
            .map(|lang| Self { lang })
            .ok_or(AstError::UnknownLanguage)
    }

    /// 由别名得到 (`"rust"`/`"rs"`/`"python"`/`"ts"` …, 大小写不敏感)。
    pub fn for_name(name: &str) -> Result<Self, AstError> {
        name.parse::<SupportLang>()
            .map(|lang| Self { lang })
            .map_err(|_| AstError::UnknownLanguage)
    }

    pub fn for_lang(lang: SupportLang) -> Self {
        Self { lang }
    }

    pub fn lang(&self) -> SupportLang {
        self.lang
    }

    /// 结构化搜索: 找出所有匹配 `pattern` 的语法节点。`pattern` 用 ast-grep 语法,
    /// 元变量须大写或 `$_` (如 `$NAME`/`$$$ARGS`)。坏 pattern → `BadPattern` (不 panic)。
    pub fn search(&self, source: &str, pattern: &str) -> Result<Vec<MatchHit>, AstError> {
        let pat = Pattern::try_new(pattern, self.lang)
            .map_err(|e| AstError::BadPattern(e.to_string()))?;
        let root = self.lang.ast_grep(source);
        let hits = root
            .root()
            .find_all(&pat)
            .map(|m| {
                let sp = m.start_pos();
                let ep = m.end_pos();
                MatchHit {
                    start_line: sp.line() + 1,
                    end_line: ep.line() + 1,
                    start_col: sp.column(&*m) + 1,
                    text: m.text().to_string(),
                }
            })
            .collect();
        Ok(hits)
    }

    /// 结构化改写: 把每处 `pattern` 替换成 `rewrite` (可含 `$VAR` 元变量回填)。
    /// 改完 re-parse: 若**引入了新的**语法错 → 拒绝 (`InvalidRewrite`), 兑现"AST 改写语法合法"。
    /// 返回 `(新源码, 改动处数)`; 0 处匹配则原样返回 + 0。
    pub fn rewrite(
        &self,
        source: &str,
        pattern: &str,
        rewrite: &str,
    ) -> Result<(String, usize), AstError> {
        let pat = Pattern::try_new(pattern, self.lang)
            .map_err(|e| AstError::BadPattern(e.to_string()))?;
        let root = self.lang.ast_grep(source);
        let edits = root.root().replace_all(&pat, rewrite);
        let n = edits.len();
        if n == 0 {
            return Ok((source.to_string(), 0));
        }
        let new_source = apply_edits(source, &edits);

        let (before, _) = self.scan_errors(source);
        let (after, first) = self.scan_errors(&new_source);
        if after > before {
            return Err(AstError::InvalidRewrite {
                lang: self.lang.to_string(),
                detail: first.unwrap_or_else(|| "new syntax error".to_string()),
            });
        }
        Ok((new_source, n))
    }

    /// 给行编辑 (`EditTool`) 当改后护栏: 若 `new_source` 比 `old_source` **多出**语法错
    /// (= 这次改动弄坏了语法), 返回 `Some(描述)`; 否则 `None`。旧有的语法错不罚 (只罚改坏的)。
    pub fn introduced_syntax_error(&self, old_source: &str, new_source: &str) -> Option<String> {
        let (before, _) = self.scan_errors(old_source);
        let (after, first) = self.scan_errors(new_source);
        if after > before {
            first.or_else(|| Some("introduced a syntax error".to_string()))
        } else {
            None
        }
    }

    /// DFS 全树数 error/missing 节点, 顺带记第一处的人读描述。
    fn scan_errors(&self, source: &str) -> (usize, Option<String>) {
        let root = self.lang.ast_grep(source);
        let mut count = 0usize;
        let mut first = None;
        for node in root.root().dfs() {
            if node.is_error() || node.is_missing() {
                count += 1;
                if first.is_none() {
                    let kind = if node.is_missing() {
                        "missing token"
                    } else {
                        "syntax error"
                    };
                    first = Some(format!("{kind} near line {}", node.start_pos().line() + 1));
                }
            }
        }
        (count, first)
    }
}

/// 把 ast-grep 的 `Vec<Edit>` (按位置升序、互不重叠) 应用到源码字节上。
/// **逆序**应用, 以免前面的改动移动后面 edit 的偏移。
fn apply_edits(source: &str, edits: &[Edit<String>]) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for e in edits.iter().rev() {
        let start = e.position;
        let end = e.position + e.deleted_length;
        bytes.splice(start..end, e.inserted_text.iter().copied());
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";

    #[test]
    fn detect_language_by_extension() {
        assert_eq!(Engine::for_path("foo.rs").unwrap().lang(), SupportLang::Rust);
        assert_eq!(Engine::for_path("a/b/c.py").unwrap().lang(), SupportLang::Python);
        assert!(Engine::for_path("foo.unknownext").is_err());
        assert!(Engine::for_path("noext").is_err());
    }

    #[test]
    fn detect_language_by_name() {
        assert_eq!(Engine::for_name("rust").unwrap().lang(), SupportLang::Rust);
        assert_eq!(Engine::for_name("RS").unwrap().lang(), SupportLang::Rust);
        assert_eq!(Engine::for_name("ts").unwrap().lang(), SupportLang::TypeScript);
        assert!(Engine::for_name("klingon").is_err());
    }

    #[test]
    fn structural_search_finds_all_with_line_numbers() {
        let eng = Engine::for_name("rust").unwrap();
        let hits = eng.search(RUST_SRC, "let $N = $V;").unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].start_line, 2);
        assert_eq!(hits[0].text, "let x = 1;");
        assert_eq!(hits[1].start_line, 3);
        assert_eq!(hits[1].text, "let y = 2;");
    }

    #[test]
    fn structural_search_bad_pattern_errors_not_panics() {
        let eng = Engine::for_name("rust").unwrap();
        // 空 pattern 无法构造成合法 matcher。
        assert!(matches!(eng.search(RUST_SRC, ""), Err(AstError::BadPattern(_))));
    }

    #[test]
    fn structural_rewrite_replaces_every_match() {
        let eng = Engine::for_name("rust").unwrap();
        let (out, n) = eng
            .rewrite(RUST_SRC, "let $N = $V;", "const $N: i32 = $V;")
            .unwrap();
        assert_eq!(n, 2);
        assert!(out.contains("const x: i32 = 1;"), "{out}");
        assert!(out.contains("const y: i32 = 2;"), "{out}");
    }

    #[test]
    fn structural_rewrite_zero_matches_is_noop() {
        let eng = Engine::for_name("rust").unwrap();
        let (out, n) = eng.rewrite(RUST_SRC, "while $C {}", "loop {}").unwrap();
        assert_eq!(n, 0);
        assert_eq!(out, RUST_SRC);
    }

    #[test]
    fn structural_rewrite_rejects_syntax_breaking_change() {
        let eng = Engine::for_name("rust").unwrap();
        // 把 `let x = 1;` 改成 `let x = ;` —— 引入语法错, 必须被拒。
        let err = eng
            .rewrite(RUST_SRC, "let $N = $V;", "let $N = ;")
            .unwrap_err();
        assert!(matches!(err, AstError::InvalidRewrite { .. }), "{err:?}");
    }

    #[test]
    fn introduced_syntax_error_only_punishes_new_breakage() {
        let eng = Engine::for_name("rust").unwrap();
        let old = "fn a() {}\n";
        let good = "fn b() {}\n";
        let bad = "fn b( {}\n";
        assert!(eng.introduced_syntax_error(old, good).is_none());
        assert!(eng.introduced_syntax_error(old, bad).is_some());
        // 旧的就已经坏的, 改完仍坏但没"更坏" → 不罚。
        assert!(eng.introduced_syntax_error(bad, bad).is_none());
    }
}
