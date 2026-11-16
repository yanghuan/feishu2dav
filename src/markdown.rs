use std::collections::{HashMap, HashSet};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_DESCENDANT_BATCH_BLOCKS: usize = 900;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocxBlock {
    pub block_id: String,
    pub block_type: i64,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub children: Vec<String>,
    #[serde(flatten)]
    pub payload: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    pub markdown: String,
    pub root_children_count: usize,
    pub unconvertible_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescendantBatch {
    pub children_id: Vec<String>,
    pub descendants: Vec<Value>,
}

pub fn format_unconvertible_forbidden_message(doc_name: &str, reasons: &[String]) -> String {
    let joined = if reasons.is_empty() {
        "富媒体或特殊组件".to_string()
    } else {
        reasons.join("、")
    };
    format!(
        "禁止覆盖写回「{doc_name}」：原飞书云文档包含无法无损转为 Markdown 的内容（{joined}），为防止误删原有素材已拦截保存；请直接在飞书中编辑该文档。"
    )
}

/// Converts a Feishu DocX block list into readable Markdown and detects non-Markdown rich blocks.
pub fn docx_blocks_to_markdown(document_id: &str, blocks: &[DocxBlock]) -> RenderedMarkdown {
    let by_id: HashMap<&str, &DocxBlock> = blocks
        .iter()
        .map(|b| (b.block_id.as_str(), b))
        .collect();

    let root = by_id
        .get(document_id)
        .copied()
        .or_else(|| blocks.iter().find(|b| b.block_type == 1));

    let Some(root) = root else {
        return RenderedMarkdown {
            markdown: String::new(),
            root_children_count: 0,
            unconvertible_reasons: Vec::new(),
        };
    };

    let mut ctx = RenderCtx {
        by_id: &by_id,
        visited: HashSet::new(),
        reasons: Vec::new(),
        seen_reasons: HashSet::new(),
    };
    ctx.visited.insert(root.block_id.as_str());

    let segments = ctx.render_top_level(&root.children);
    let mut markdown = segments.join("\n\n");
    if !markdown.is_empty() && !markdown.ends_with('\n') {
        markdown.push('\n');
    }

    RenderedMarkdown {
        markdown,
        root_children_count: root.children.len(),
        unconvertible_reasons: ctx.reasons,
    }
}

struct RenderCtx<'a> {
    by_id: &'a HashMap<&'a str, &'a DocxBlock>,
    visited: HashSet<&'a str>,
    reasons: Vec<String>,
    seen_reasons: HashSet<String>,
}

impl<'a> RenderCtx<'a> {
    fn mark_unconvertible(&mut self, reason: &str) {
        if self.seen_reasons.insert(reason.to_string()) {
            self.reasons.push(reason.to_string());
        }
    }

    fn render_top_level(&mut self, child_ids: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut idx = 0usize;

        while idx < child_ids.len() {
            let Some(&block) = self.by_id.get(child_ids[idx].as_str()) else {
                idx += 1;
                continue;
            };

            // Group consecutive list blocks of the same list stream tightly without blank lines
            if is_list_block(block.block_type) {
                let mut list_lines = Vec::new();
                let mut ordered_num = 1usize;
                while idx < child_ids.len() {
                    let Some(&item) = self.by_id.get(child_ids[idx].as_str()) else {
                        idx += 1;
                        continue;
                    };
                    if !is_list_block(item.block_type) {
                        break;
                    }
                    if item.block_type == 13 {
                        self.render_list_block(item, 0, Some(ordered_num), &mut list_lines);
                        ordered_num += 1;
                    } else {
                        ordered_num = 1;
                        self.render_list_block(item, 0, None, &mut list_lines);
                    }
                    idx += 1;
                }
                if !list_lines.is_empty() {
                    out.push(list_lines.join("\n"));
                }
                continue;
            }

            if let Some(rendered) = self.render_block_standalone(block) {
                out.push(rendered);
            }
            idx += 1;
        }

        out
    }

    fn render_block_standalone(&mut self, block: &'a DocxBlock) -> Option<String> {
        if !self.visited.insert(block.block_id.as_str()) {
            return None;
        }

        let res = match block.block_type {
            2 => {
                let text = self.render_text_block(block, "text");
                let nested = self.render_top_level(&block.children);
                if text.is_empty() && nested.is_empty() {
                    None
                } else if nested.is_empty() {
                    Some(text)
                } else if text.is_empty() {
                    Some(nested.join("\n\n"))
                } else {
                    Some(format!("{text}\n\n{}", nested.join("\n\n")))
                }
            }
            3..=11 => {
                let level = ((block.block_type - 2) as usize).min(6);
                let key = format!("heading{}", block.block_type - 2);
                let text = self.render_text_block(block, &key);
                let hashes = "#".repeat(level);
                let heading = format!("{hashes} {text}");
                let nested = self.render_top_level(&block.children);
                if nested.is_empty() {
                    Some(heading)
                } else {
                    Some(format!("{heading}\n\n{}", nested.join("\n\n")))
                }
            }
            12 | 13 | 17 => {
                let mut lines = Vec::new();
                let ord = (block.block_type == 13).then_some(1);
                self.render_list_block_inner(block, 0, ord, &mut lines);
                (!lines.is_empty()).then(|| lines.join("\n"))
            }
            14 => Some(self.render_code_block(block)),
            15 => {
                let text = self.render_text_block(block, "quote");
                let mut lines: Vec<String> = text
                    .lines()
                    .map(|l| {
                        if l.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {l}")
                        }
                    })
                    .collect();
                if lines.is_empty() {
                    lines.push(">".to_string());
                }
                for child_md in self.render_top_level(&block.children) {
                    for line in child_md.lines() {
                        if line.is_empty() {
                            lines.push(">".to_string());
                        } else {
                            lines.push(format!("> {line}"));
                        }
                    }
                }
                Some(lines.join("\n"))
            }
            34 => {
                let inner = self.render_top_level(&block.children).join("\n\n");
                let mut lines = Vec::new();
                for line in inner.lines() {
                    if line.is_empty() {
                        lines.push(">".to_string());
                    } else {
                        lines.push(format!("> {line}"));
                    }
                }
                if lines.is_empty() {
                    lines.push(">".to_string());
                }
                Some(lines.join("\n"))
            }
            22 => Some("---".to_string()),
            31 => Some(self.render_table_block(block)),
            19 => {
                self.mark_unconvertible("高亮块 (callout)");
                let inner = self.render_top_level(&block.children).join("\n\n");
                let mut lines = vec!["> [!NOTE]".to_string()];
                for line in inner.lines() {
                    if line.is_empty() {
                        lines.push(">".to_string());
                    } else {
                        lines.push(format!("> {line}"));
                    }
                }
                Some(lines.join("\n"))
            }
            24 => {
                self.mark_unconvertible("分栏 (grid)");
                let inner = self.render_top_level(&block.children);
                (!inner.is_empty()).then(|| inner.join("\n\n"))
            }
            25 => {
                self.mark_unconvertible("分栏列 (grid_column)");
                let inner = self.render_top_level(&block.children);
                (!inner.is_empty()).then(|| inner.join("\n\n"))
            }
            27 => {
                self.mark_unconvertible("图片 (image)");
                let tok = block
                    .payload
                    .get("image")
                    .and_then(|v| v.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if tok.is_empty() {
                    Some("![image](feishu-image://)".to_string())
                } else {
                    Some(format!("![image](feishu-image://{tok})"))
                }
            }
            23 => {
                self.mark_unconvertible("附件 (file)");
                let name = block
                    .payload
                    .get("file")
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("attachment");
                let tok = block
                    .payload
                    .get("file")
                    .and_then(|v| v.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Some(format!("[附件: {name}](feishu-file://{tok})"))
            }
            33 => {
                // View wrapper accompanying a file block; surface inner children if any without noisy tag
                let inner = self.render_top_level(&block.children);
                (!inner.is_empty()).then(|| inner.join("\n\n"))
            }
            other => {
                let label = rich_block_label(other);
                self.mark_unconvertible(label);
                let token = extract_rich_token(block);
                let marker = if token.is_empty() {
                    format!("<!-- feishu:{label} -->")
                } else {
                    format!("<!-- feishu:{label} token={token} -->")
                };
                let inner = self.render_top_level(&block.children);
                if inner.is_empty() {
                    Some(marker)
                } else {
                    Some(format!("{marker}\n{}", inner.join("\n\n")))
                }
            }
        };

        self.visited.remove(block.block_id.as_str());
        res
    }

    fn render_list_block(
        &mut self,
        block: &'a DocxBlock,
        indent: usize,
        ordered_num: Option<usize>,
        lines: &mut Vec<String>,
    ) {
        if !self.visited.insert(block.block_id.as_str()) {
            return;
        }
        self.render_list_block_inner(block, indent, ordered_num, lines);
        self.visited.remove(block.block_id.as_str());
    }

    fn render_list_block_inner(
        &mut self,
        block: &'a DocxBlock,
        indent: usize,
        ordered_num: Option<usize>,
        lines: &mut Vec<String>,
    ) {
        let pad = "  ".repeat(indent);
        let (prefix, text) = match block.block_type {
            12 => ("- ".to_string(), self.render_text_block(block, "bullet")),
            13 => (
                format!("{}. ", ordered_num.unwrap_or(1)),
                self.render_text_block(block, "ordered"),
            ),
            17 => {
                let done = block
                    .payload
                    .get("todo")
                    .and_then(|v| v.get("style"))
                    .and_then(|s| s.get("done"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let marker = if done { "- [x] " } else { "- [ ] " };
                (marker.to_string(), self.render_text_block(block, "todo"))
            }
            _ => ("- ".to_string(), String::new()),
        };

        lines.push(format!("{pad}{prefix}{text}"));

        let child_pad = "  ".repeat(indent + 1);
        let mut child_ordered = 1usize;
        for cid in &block.children {
            let Some(&child) = self.by_id.get(cid.as_str()) else {
                continue;
            };
            if is_list_block(child.block_type) {
                if child.block_type == 13 {
                    self.render_list_block(child, indent + 1, Some(child_ordered), lines);
                    child_ordered += 1;
                } else {
                    child_ordered = 1;
                    self.render_list_block(child, indent + 1, None, lines);
                }
            } else {
                child_ordered = 1;
                if let Some(child_md) = self.render_block_standalone(child) {
                    for line in child_md.lines() {
                        if line.is_empty() {
                            lines.push(String::new());
                        } else {
                            lines.push(format!("{child_pad}{line}"));
                        }
                    }
                }
            }
        }
    }

    fn render_code_block(&mut self, block: &DocxBlock) -> String {
        let code_obj = block.payload.get("code");
        let lang_id = code_obj
            .and_then(|c| c.get("style"))
            .and_then(|s| s.get("language"))
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let lang = feishu_lang_id_to_tag(lang_id);

        let mut raw = String::new();
        if let Some(elements) = code_obj.and_then(|c| c.get("elements")).and_then(Value::as_array) {
            for elem in elements {
                if let Some(s) = elem
                    .get("text_run")
                    .and_then(|t| t.get("content"))
                    .and_then(Value::as_str)
                {
                    raw.push_str(s);
                } else if let Some(s) = elem
                    .get("equation")
                    .and_then(|e| e.get("content"))
                    .and_then(Value::as_str)
                {
                    raw.push_str(s);
                }
            }
        }

        let mut backtick_run = 3usize;
        let mut cur = 0usize;
        for ch in raw.chars() {
            if ch == '`' {
                cur += 1;
                backtick_run = backtick_run.max(cur + 1);
            } else {
                cur = 0;
            }
        }
        let fence = "`".repeat(backtick_run);
        if raw.ends_with('\n') {
            format!("{fence}{lang}\n{raw}{fence}")
        } else {
            format!("{fence}{lang}\n{raw}\n{fence}")
        }
    }

    fn render_table_block(&mut self, block: &'a DocxBlock) -> String {
        let table = block.payload.get("table");
        let prop = table.and_then(|t| t.get("property"));
        let row_size = prop
            .and_then(|p| p.get("row_size"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let col_size = prop
            .and_then(|p| p.get("column_size"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        if let Some(merges) = prop.and_then(|p| p.get("merge_info")).and_then(Value::as_array)
            && merges.iter().any(|m| {
                m.get("row_span").and_then(Value::as_u64).unwrap_or(1) > 1
                    || m.get("col_span").and_then(Value::as_u64).unwrap_or(1) > 1
            })
        {
            self.mark_unconvertible("合并单元格表格");
        }

        let cell_ids: Vec<String> = table
            .and_then(|t| t.get("cells"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| block.children.clone());

        if row_size == 0 || col_size == 0 || cell_ids.is_empty() {
            return String::new();
        }

        let mut rows_rendered = Vec::with_capacity(row_size);
        for r in 0..row_size {
            let mut row_cells = Vec::with_capacity(col_size);
            for c in 0..col_size {
                let idx = r * col_size + c;
                let cell_text = if let Some(cid) = cell_ids.get(idx)
                    && let Some(&cell_block) = self.by_id.get(cid.as_str())
                    && self.visited.insert(cell_block.block_id.as_str())
                {
                    let inner = self.render_top_level(&cell_block.children);
                    self.visited.remove(cell_block.block_id.as_str());
                    inner
                        .join("<br>")
                        .replace("\r\n", "\n")
                        .replace('\n', "<br>")
                        .replace('|', "\\|")
                } else {
                    String::new()
                };
                row_cells.push(cell_text);
            }
            rows_rendered.push(row_cells);
        }

        let mut lines = Vec::with_capacity(row_size + 1);
        lines.push(format!("| {} |", rows_rendered[0].join(" | ")));
        let sep = vec!["---"; col_size].join(" | ");
        lines.push(format!("| {sep} |"));
        for row in rows_rendered.into_iter().skip(1) {
            lines.push(format!("| {} |", row.join(" | ")));
        }
        lines.join("\n")
    }

    fn render_text_block(&mut self, block: &DocxBlock, key: &str) -> String {
        let Some(elements) = block
            .payload
            .get(key)
            .and_then(|o| o.get("elements"))
            .and_then(Value::as_array)
        else {
            return String::new();
        };
        self.render_elements(elements)
    }

    fn render_elements(&mut self, elements: &[Value]) -> String {
        let mut out = String::new();
        let mut pending_runs: Vec<StyledRun> = Vec::new();

        for elem in elements {
            if let Some(tr) = elem.get("text_run") {
                let content = tr.get("content").and_then(Value::as_str).unwrap_or("");
                if content.is_empty() {
                    continue;
                }
                let style = tr.get("text_element_style");
                let inline_code = style
                    .and_then(|s| s.get("inline_code"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let bold = style
                    .and_then(|s| s.get("bold"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let italic = style
                    .and_then(|s| s.get("italic"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let strike = style
                    .and_then(|s| s.get("strikethrough"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let link_url = style
                    .and_then(|s| s.get("link"))
                    .and_then(|l| l.get("url"))
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                    .map(percent_decode_str);

                if inline_code || link_url.is_some() {
                    flush_styled_runs(&mut pending_runs, &mut out);
                    let formatted = if inline_code {
                        let code_span = wrap_inline_code(content);
                        if let Some(ref url) = link_url {
                            format!("[{code_span}]({})", format_md_link_dest(url))
                        } else {
                            code_span
                        }
                    } else {
                        let inner = wrap_emphasis(content, bold, italic, strike);
                        format!(
                            "[{inner}]({})",
                            format_md_link_dest(link_url.as_deref().unwrap_or(""))
                        )
                    };
                    out.push_str(&formatted);
                } else if let Some(last) = pending_runs.last_mut()
                    && last.bold == bold
                    && last.italic == italic
                    && last.strike == strike
                {
                    last.text.push_str(content);
                } else {
                    pending_runs.push(StyledRun {
                        text: content.to_string(),
                        bold,
                        italic,
                        strike,
                    });
                }
                continue;
            }

            flush_styled_runs(&mut pending_runs, &mut out);

            if let Some(eq) = elem.get("equation") {
                let raw = eq
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim_end_matches('\n');
                if !raw.is_empty() {
                    if raw.contains('\n') {
                        out.push_str(&format!("$${raw}$$"));
                    } else {
                        out.push_str(&format!("${raw}$"));
                    }
                }
            } else if let Some(mdoc) = elem.get("mention_doc") {
                let raw_url = mdoc.get("url").and_then(Value::as_str).unwrap_or("");
                let title = mdoc
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .unwrap_or("飞书文档");
                if raw_url.is_empty() {
                    out.push_str(title);
                } else {
                    let decoded = percent_decode_str(raw_url);
                    out.push_str(&format!("[{title}]({})", format_md_link_dest(&decoded)));
                }
            } else if let Some(muser) = elem.get("mention_user") {
                self.mark_unconvertible("@用户 (mention_user)");
                let uid = muser.get("user_id").and_then(Value::as_str).unwrap_or("user");
                out.push_str(&format!("@{uid}"));
            } else if let Some(file) = elem.get("file") {
                self.mark_unconvertible("行内附件 (inline_file)");
                let name = file
                    .get("file_name")
                    .and_then(Value::as_str)
                    .unwrap_or("attachment");
                let tok = file.get("file_token").and_then(Value::as_str).unwrap_or("");
                out.push_str(&format!("[{name}](feishu-file://{tok})"));
            } else if elem.get("reminder").is_some() {
                self.mark_unconvertible("日期提醒 (reminder)");
                out.push_str("[reminder]");
            } else {
                let kind = unknown_element_key(elem);
                self.mark_unconvertible(&format!("未知行内元素 ({kind})"));
                out.push_str(&format!("[feishu:{kind}]"));
            }
        }
        flush_styled_runs(&mut pending_runs, &mut out);
        out
    }
}

fn unknown_element_key(elem: &Value) -> &str {
    elem.as_object()
        .and_then(|obj| obj.keys().next())
        .map(String::as_str)
        .unwrap_or("unknown")
}

struct StyledRun {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Strike,
    Bold,
    Italic,
}

impl Mark {
    fn tag(self) -> &'static str {
        match self {
            Mark::Strike => "~~",
            Mark::Bold => "**",
            Mark::Italic => "*",
        }
    }

    fn active_in(self, run: &StyledRun) -> bool {
        match self {
            Mark::Strike => run.strike,
            Mark::Bold => run.bold,
            Mark::Italic => run.italic,
        }
    }
}

fn flush_styled_runs(runs: &mut Vec<StyledRun>, out: &mut String) {
    if runs.is_empty() {
        return;
    }
    let drained = std::mem::take(runs);
    let mut open: Vec<Mark> = Vec::new();

    for (idx, run) in drained.iter().enumerate() {
        let is_ws_only = run.text.trim().is_empty();
        if is_ws_only && open.is_empty() {
            out.push_str(&run.text);
            continue;
        }

        // Close active marks down to the earliest mark no longer active on this run
        if let Some(close_pos) = open.iter().position(|m| !m.active_in(run)) {
            while open.len() > close_pos {
                if let Some(m) = open.pop() {
                    out.push_str(m.tag());
                }
            }
        }

        let lead_len = if open.is_empty() && !is_ws_only {
            run.text.len() - run.text.trim_start().len()
        } else {
            0
        };
        if lead_len > 0 {
            out.push_str(&run.text[..lead_len]);
        }

        for m in [Mark::Strike, Mark::Bold, Mark::Italic] {
            if m.active_in(run) && !open.contains(&m) {
                open.push(m);
                out.push_str(m.tag());
            }
        }

        let next = drained.get(idx + 1);
        let will_close_any = match next {
            None => !open.is_empty(),
            Some(nr) => open.iter().any(|m| !m.active_in(nr)),
        };

        let trail_start = if will_close_any && !is_ws_only {
            run.text.trim_end().len().max(lead_len)
        } else {
            run.text.len()
        };

        out.push_str(&run.text[lead_len..trail_start]);

        if trail_start < run.text.len() {
            if let Some(nr) = next {
                if let Some(close_pos) = open.iter().position(|m| !m.active_in(nr)) {
                    while open.len() > close_pos {
                        if let Some(m) = open.pop() {
                            out.push_str(m.tag());
                        }
                    }
                }
            } else {
                while let Some(m) = open.pop() {
                    out.push_str(m.tag());
                }
            }
            out.push_str(&run.text[trail_start..]);
        }
    }

    while let Some(m) = open.pop() {
        out.push_str(m.tag());
    }
}

fn is_list_block(block_type: i64) -> bool {
    matches!(block_type, 12 | 13 | 17)
}

fn rich_block_label(block_type: i64) -> &'static str {
    match block_type {
        18 => "多维表格 (bitable)",
        21 => "流程图/图表 (diagram)",
        26 => "内嵌网页 (iframe)",
        29 => "思维笔记 (mindnote)",
        30 => "电子表格 (sheet)",
        35 => "任务 (task)",
        36..=39 => "OKR 组件 (okr)",
        40 => "文档小组件 (add_ons)",
        41 => "Jira 卡片 (jira_issue)",
        42 => "知识库目录 (wiki_catalog)",
        43 => "画板 (board)",
        44 => "会议议程 (agenda)",
        _ => "未知富媒体块",
    }
}

fn extract_rich_token(block: &DocxBlock) -> String {
    for val in block.payload.values() {
        if let Some(tok) = val.get("token").and_then(Value::as_str)
            && !tok.is_empty()
        {
            return tok.to_string();
        }
    }
    String::new()
}

fn wrap_inline_code(content: &str) -> String {
    let mut run = 1usize;
    let mut cur = 0usize;
    for ch in content.chars() {
        if ch == '`' {
            cur += 1;
            run = run.max(cur + 1);
        } else {
            cur = 0;
        }
    }
    let ticks = "`".repeat(run);
    if content.starts_with('`') || content.ends_with('`') {
        format!("{ticks} {content} {ticks}")
    } else {
        format!("{ticks}{content}{ticks}")
    }
}

fn wrap_emphasis(content: &str, bold: bool, italic: bool, strike: bool) -> String {
    if !bold && !italic && !strike {
        return content.to_string();
    }
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return content.to_string();
    }
    let lead_len = content.len() - content.trim_start().len();
    let trail_start = content.trim_end().len();
    let lead = &content[..lead_len];
    let trail = &content[trail_start..];
    let core = &content[lead_len..trail_start];

    let mut wrapped = core.to_string();
    if italic {
        wrapped = format!("*{wrapped}*");
    }
    if bold {
        wrapped = format!("**{wrapped}**");
    }
    if strike {
        wrapped = format!("~~{wrapped}~~");
    }
    format!("{lead}{wrapped}{trail}")
}

fn format_md_link_dest(url: &str) -> String {
    if url.contains([' ', '(', ')']) {
        format!("<{url}>")
    } else {
        url.to_string()
    }
}

/// Converts CommonMark/GFM Markdown into batched Feishu DocX `descendant` payloads
/// (each batch holds <= `MAX_DESCENDANT_BATCH_BLOCKS` total blocks).
pub fn markdown_to_docx_descendant_batches(markdown: &str) -> Result<Vec<DescendantBatch>, String> {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let parser = Parser::new_ext(markdown, opts);

    let mut builder = DocxBuilder::default();
    builder.parse(parser);
    builder.into_batches()
}

#[derive(Default, Clone)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    link: Option<String>,
}

impl InlineStyle {
    fn to_json(&self) -> Option<Value> {
        let mut obj = serde_json::Map::new();
        if self.bold {
            obj.insert("bold".to_string(), json!(true));
        }
        if self.italic {
            obj.insert("italic".to_string(), json!(true));
        }
        if self.strikethrough {
            obj.insert("strikethrough".to_string(), json!(true));
        }
        if let Some(ref raw_url) = self.link
            && let Some(encoded) = sanitize_and_encode_feishu_link(raw_url)
        {
            obj.insert("link".to_string(), json!({ "url": encoded }));
        }
        if obj.is_empty() {
            None
        } else {
            Some(Value::Object(obj))
        }
    }
}

struct NodeDraft {
    block_id: String,
    block_type: i64,
    key: String,
    data: Value,
    children: Vec<String>,
}

enum ContainerFrame {
    QuoteContainer {
        block_id: String,
        children: Vec<String>,
    },
    List {
        ordered: bool,
    },
    ListItem {
        block_id: String,
        ordered: bool,
        todo_done: Option<bool>,
        elements: Vec<Value>,
        primary_sealed: bool,
        children: Vec<String>,
    },
    Paragraph {
        elements: Vec<Value>,
        is_primary_list_para: bool,
    },
    Heading {
        level: u8,
        elements: Vec<Value>,
    },
    CodeBlock {
        language: i64,
        content: String,
    },
    Table {
        rows: Vec<Vec<Vec<Value>>>,
        current_row: Vec<Vec<Value>>,
        current_cell: Vec<Value>,
        in_cell: bool,
    },
}

#[derive(Default)]
struct DocxBuilder {
    next_id: usize,
    nodes: Vec<NodeDraft>,
    top_level_ids: Vec<String>,
    frames: Vec<ContainerFrame>,
    style: InlineStyle,
    link_stack: Vec<Option<String>>,
}

impl DocxBuilder {
    fn alloc_id(&mut self) -> String {
        self.next_id += 1;
        format!("md_{}", self.next_id)
    }

    fn parse<'a>(&mut self, parser: Parser<'a>) {
        for ev in parser {
            match ev {
                Event::Start(tag) => self.on_start(tag),
                Event::End(tag) => self.on_end(tag),
                Event::Text(text) => self.on_text(&text),
                Event::Code(code) => self.on_inline_code(&code),
                Event::InlineMath(math) => self.on_inline_math(&math),
                Event::DisplayMath(math) => self.on_display_math(&math),
                Event::SoftBreak => self.on_text(" "),
                Event::HardBreak => self.on_text("\n"),
                Event::Rule => self.on_rule(),
                Event::TaskListMarker(checked) => self.on_task_marker(checked),
                Event::InlineHtml(html) => {
                    let trimmed = html.trim().to_ascii_lowercase();
                    if matches!(trimmed.as_str(), "<br>" | "<br/>" | "<br />") {
                        self.on_text("\n");
                    }
                }
                Event::Html(_) | Event::FootnoteReference(_) => {}
            }
        }
    }

    fn current_ordered_parent(&self) -> bool {
        self.frames
            .iter()
            .rev()
            .find_map(|f| match f {
                ContainerFrame::List { ordered } => Some(*ordered),
                _ => None,
            })
            .unwrap_or(false)
    }

    fn attach_child_block(&mut self, child_id: String) {
        for frame in self.frames.iter_mut().rev() {
            match frame {
                ContainerFrame::ListItem { children, .. }
                | ContainerFrame::QuoteContainer { children, .. } => {
                    children.push(child_id);
                    return;
                }
                _ => {}
            }
        }
        self.top_level_ids.push(child_id);
    }

    fn push_inline_element(&mut self, elem: Value) {
        for frame in self.frames.iter_mut().rev() {
            match frame {
                ContainerFrame::Table {
                    current_cell,
                    in_cell: true,
                    ..
                } => {
                    current_cell.push(elem);
                    return;
                }
                ContainerFrame::Paragraph { elements, .. }
                | ContainerFrame::Heading { elements, .. } => {
                    elements.push(elem);
                    return;
                }
                ContainerFrame::ListItem {
                    elements,
                    primary_sealed: false,
                    ..
                } => {
                    elements.push(elem);
                    return;
                }
                _ => {}
            }
        }
    }

    fn on_start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                let attach_to_list_item = matches!(
                    self.frames.last(),
                    Some(ContainerFrame::ListItem {
                        primary_sealed: false,
                        elements,
                        children,
                        ..
                    }) if elements.is_empty() && children.is_empty()
                );
                self.frames.push(ContainerFrame::Paragraph {
                    elements: Vec::new(),
                    is_primary_list_para: attach_to_list_item,
                });
            }
            Tag::Heading { level, .. } => {
                let lvl = heading_level_to_u8(level);
                self.frames.push(ContainerFrame::Heading {
                    level: lvl,
                    elements: Vec::new(),
                });
            }
            Tag::BlockQuote(_) => {
                let id = self.alloc_id();
                self.frames.push(ContainerFrame::QuoteContainer {
                    block_id: id,
                    children: Vec::new(),
                });
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let tag = info.split_whitespace().next().unwrap_or("");
                        tag_to_feishu_lang_id(tag)
                    }
                    CodeBlockKind::Indented => 1,
                };
                self.frames.push(ContainerFrame::CodeBlock {
                    language: lang,
                    content: String::new(),
                });
            }
            Tag::List(start) => {
                // Any following content inside parent ListItem belongs to children blocks
                if let Some(ContainerFrame::ListItem { primary_sealed, .. }) = self.frames.last_mut() {
                    *primary_sealed = true;
                }
                self.frames.push(ContainerFrame::List {
                    ordered: start.is_some(),
                });
            }
            Tag::Item => {
                let ordered = self.current_ordered_parent();
                let id = self.alloc_id();
                self.frames.push(ContainerFrame::ListItem {
                    block_id: id,
                    ordered,
                    todo_done: None,
                    elements: Vec::new(),
                    primary_sealed: false,
                    children: Vec::new(),
                });
            }
            Tag::Table(_) => {
                self.frames.push(ContainerFrame::Table {
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: Vec::new(),
                    in_cell: false,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(ContainerFrame::Table { current_row, .. }) = self.frames.last_mut() {
                    current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(ContainerFrame::Table {
                    current_cell,
                    in_cell,
                    ..
                }) = self.frames.last_mut()
                {
                    current_cell.clear();
                    *in_cell = true;
                }
            }
            Tag::Strong => self.style.bold = true,
            Tag::Emphasis => self.style.italic = true,
            Tag::Strikethrough => self.style.strikethrough = true,
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(self.style.link.take());
                self.style.link = Some(dest_url.into_string());
            }
            _ => {}
        }
    }

    fn on_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some(ContainerFrame::Paragraph {
                    elements,
                    is_primary_list_para,
                }) = self.frames.pop()
                {
                    if is_primary_list_para {
                        if let Some(ContainerFrame::ListItem {
                            elements: item_elems,
                            primary_sealed,
                            ..
                        }) = self.frames.last_mut()
                        {
                            *item_elems = elements;
                            *primary_sealed = true;
                        }
                    } else {
                        let id = self.alloc_id();
                        self.nodes.push(NodeDraft {
                            block_id: id.clone(),
                            block_type: 2,
                            key: "text".to_string(),
                            data: json!({ "elements": elements }),
                            children: Vec::new(),
                        });
                        self.attach_child_block(id);
                    }
                }
            }
            TagEnd::Heading(_) => {
                if let Some(ContainerFrame::Heading { level, elements }) = self.frames.pop() {
                    let id = self.alloc_id();
                    let block_type = 2 + i64::from(level);
                    let key = format!("heading{level}");
                    self.nodes.push(NodeDraft {
                        block_id: id.clone(),
                        block_type,
                        key,
                        data: json!({ "elements": elements }),
                        children: Vec::new(),
                    });
                    self.attach_child_block(id);
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(ContainerFrame::QuoteContainer {
                    block_id,
                    mut children,
                }) = self.frames.pop()
                {
                    if children.is_empty() {
                        let empty_id = self.alloc_id();
                        self.nodes.push(NodeDraft {
                            block_id: empty_id.clone(),
                            block_type: 2,
                            key: "text".to_string(),
                            data: json!({ "elements": [] }),
                            children: Vec::new(),
                        });
                        children.push(empty_id);
                    }
                    self.nodes.push(NodeDraft {
                        block_id: block_id.clone(),
                        block_type: 34,
                        key: "quote_container".to_string(),
                        data: json!({}),
                        children,
                    });
                    self.attach_child_block(block_id);
                }
            }
            TagEnd::CodeBlock => {
                if let Some(ContainerFrame::CodeBlock {
                    language,
                    mut content,
                }) = self.frames.pop()
                {
                    if content.ends_with('\n') {
                        content.pop();
                    }
                    let id = self.alloc_id();
                    let elements = if content.is_empty() {
                        vec![]
                    } else {
                        vec![json!({ "text_run": { "content": content } })]
                    };
                    self.nodes.push(NodeDraft {
                        block_id: id.clone(),
                        block_type: 14,
                        key: "code".to_string(),
                        data: json!({
                            "style": { "language": language, "wrap": true },
                            "elements": elements,
                        }),
                        children: Vec::new(),
                    });
                    self.attach_child_block(id);
                }
            }
            TagEnd::List(_) => {
                if matches!(self.frames.last(), Some(ContainerFrame::List { .. })) {
                    self.frames.pop();
                }
            }
            TagEnd::Item => {
                if let Some(ContainerFrame::ListItem {
                    block_id,
                    ordered,
                    todo_done,
                    elements,
                    children,
                    ..
                }) = self.frames.pop()
                {
                    let (block_type, key, data) = if let Some(done) = todo_done {
                        (
                            17,
                            "todo".to_string(),
                            json!({
                                "style": { "done": done },
                                "elements": elements,
                            }),
                        )
                    } else if ordered {
                        (13, "ordered".to_string(), json!({ "elements": elements }))
                    } else {
                        (12, "bullet".to_string(), json!({ "elements": elements }))
                    };
                    self.nodes.push(NodeDraft {
                        block_id: block_id.clone(),
                        block_type,
                        key,
                        data,
                        children,
                    });
                    self.attach_child_block(block_id);
                }
            }
            TagEnd::TableCell => {
                if let Some(ContainerFrame::Table {
                    current_row,
                    current_cell,
                    in_cell,
                    ..
                }) = self.frames.last_mut()
                {
                    current_row.push(std::mem::take(current_cell));
                    *in_cell = false;
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(ContainerFrame::Table {
                    rows, current_row, ..
                }) = self.frames.last_mut()
                    && !current_row.is_empty()
                {
                    rows.push(std::mem::take(current_row));
                }
            }
            TagEnd::Table => {
                if let Some(ContainerFrame::Table { mut rows, .. }) = self.frames.pop() {
                    let row_size = rows.len();
                    let col_size = rows.iter().map(Vec::len).max().unwrap_or(0);
                    if row_size > 0 && col_size > 0 {
                        let table_id = self.alloc_id();
                        let mut cell_ids = Vec::with_capacity(row_size * col_size);
                        for row in &mut rows {
                            row.resize_with(col_size, Vec::new);
                            for cell_elems in row.drain(..) {
                                let cell_id = self.alloc_id();
                                let text_id = self.alloc_id();
                                self.nodes.push(NodeDraft {
                                    block_id: text_id.clone(),
                                    block_type: 2,
                                    key: "text".to_string(),
                                    data: json!({ "elements": cell_elems }),
                                    children: Vec::new(),
                                });
                                self.nodes.push(NodeDraft {
                                    block_id: cell_id.clone(),
                                    block_type: 32,
                                    key: "table_cell".to_string(),
                                    data: json!({}),
                                    children: vec![text_id],
                                });
                                cell_ids.push(cell_id);
                            }
                        }
                        self.nodes.push(NodeDraft {
                            block_id: table_id.clone(),
                            block_type: 31,
                            key: "table".to_string(),
                            data: json!({
                                "property": {
                                    "row_size": row_size,
                                    "column_size": col_size,
                                    "header_row": true,
                                }
                            }),
                            children: cell_ids,
                        });
                        self.attach_child_block(table_id);
                    }
                }
            }
            TagEnd::Strong => self.style.bold = false,
            TagEnd::Emphasis => self.style.italic = false,
            TagEnd::Strikethrough => self.style.strikethrough = false,
            TagEnd::Link => {
                self.style.link = self.link_stack.pop().unwrap_or(None);
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ContainerFrame::CodeBlock { content, .. }) = self.frames.last_mut() {
            content.push_str(text);
            return;
        }
        let mut tr = serde_json::Map::new();
        tr.insert("content".to_string(), json!(text));
        if let Some(style_json) = self.style.to_json() {
            tr.insert("text_element_style".to_string(), style_json);
        }
        self.push_inline_element(json!({ "text_run": Value::Object(tr) }));
    }

    fn on_inline_code(&mut self, code: &str) {
        if code.is_empty() {
            return;
        }
        let mut style_map = serde_json::Map::new();
        style_map.insert("inline_code".to_string(), json!(true));
        if let Some(ref raw_url) = self.style.link
            && let Some(encoded) = sanitize_and_encode_feishu_link(raw_url)
        {
            style_map.insert("link".to_string(), json!({ "url": encoded }));
        }
        self.push_inline_element(json!({
            "text_run": {
                "content": code,
                "text_element_style": Value::Object(style_map),
            }
        }));
    }

    fn on_inline_math(&mut self, math: &str) {
        if math.is_empty() {
            return;
        }
        self.push_inline_element(json!({ "equation": { "content": math } }));
    }

    fn on_display_math(&mut self, math: &str) {
        let id = self.alloc_id();
        self.nodes.push(NodeDraft {
            block_id: id.clone(),
            block_type: 2,
            key: "text".to_string(),
            data: json!({
                "elements": [{ "equation": { "content": math } }]
            }),
            children: Vec::new(),
        });
        self.attach_child_block(id);
    }

    fn on_rule(&mut self) {
        let id = self.alloc_id();
        self.nodes.push(NodeDraft {
            block_id: id.clone(),
            block_type: 22,
            key: "divider".to_string(),
            data: json!({}),
            children: Vec::new(),
        });
        self.attach_child_block(id);
    }

    fn on_task_marker(&mut self, checked: bool) {
        for frame in self.frames.iter_mut().rev() {
            if let ContainerFrame::ListItem { todo_done, .. } = frame {
                *todo_done = Some(checked);
                return;
            }
        }
    }

    fn into_batches(self) -> Result<Vec<DescendantBatch>, String> {
        if self.top_level_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut by_id: HashMap<String, NodeDraft> = self
            .nodes
            .into_iter()
            .map(|n| (n.block_id.clone(), n))
            .collect();

        let mut batches = Vec::new();
        let mut current_roots = Vec::new();
        let mut current_descendants = Vec::new();

        for top_id in self.top_level_ids {
            let mut subtree = Vec::new();
            collect_subtree(&top_id, &mut by_id, &mut subtree);
            if subtree.len() > MAX_DESCENDANT_BATCH_BLOCKS {
                return Err(format!(
                    "单个顶层块（含子块共 {} 个）超过飞书单次 descendant 1000 块限制，请拆分该结构后再保存",
                    subtree.len()
                ));
            }
            if !current_descendants.is_empty()
                && current_descendants.len() + subtree.len() > MAX_DESCENDANT_BATCH_BLOCKS
            {
                batches.push(DescendantBatch {
                    children_id: std::mem::take(&mut current_roots),
                    descendants: std::mem::take(&mut current_descendants),
                });
            }
            current_roots.push(top_id);
            current_descendants.extend(subtree);
        }

        if !current_descendants.is_empty() {
            batches.push(DescendantBatch {
                children_id: current_roots,
                descendants: current_descendants,
            });
        }

        Ok(batches)
    }
}

fn collect_subtree(
    id: &str,
    by_id: &mut HashMap<String, NodeDraft>,
    out: &mut Vec<Value>,
) {
    let Some(node) = by_id.remove(id) else {
        return;
    };
    let child_ids = node.children.clone();
    let mut map = serde_json::Map::new();
    map.insert("block_id".to_string(), json!(node.block_id));
    map.insert("block_type".to_string(), json!(node.block_type));
    map.insert("children".to_string(), json!(node.children));
    map.insert(node.key, node.data);
    out.push(Value::Object(map));

    for cid in child_ids {
        collect_subtree(&cid, by_id, out);
    }
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn sanitize_and_encode_feishu_link(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return None;
    }
    Some(percent_encode_str(parsed.as_str()))
}

fn percent_encode_str(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn percent_decode_str(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn feishu_lang_id_to_tag(id: i64) -> &'static str {
    match id {
        7 => "bash",
        8 => "csharp",
        9 => "cpp",
        10 => "c",
        12 => "css",
        15 => "dart",
        18 => "dockerfile",
        22 => "go",
        23 => "groovy",
        24 => "html",
        26 => "http",
        27 => "haskell",
        28 => "json",
        29 => "java",
        30 => "javascript",
        32 => "kotlin",
        33 => "latex",
        36 => "lua",
        38 => "makefile",
        39 => "markdown",
        40 => "nginx",
        41 => "objc",
        43 => "php",
        44 => "perl",
        46 => "powershell",
        48 => "protobuf",
        49 => "python",
        50 => "r",
        52 => "ruby",
        53 => "rust",
        55 => "scss",
        56 => "sql",
        57 => "scala",
        60 => "shell",
        61 => "swift",
        63 => "typescript",
        66 => "xml",
        67 => "yaml",
        68 => "cmake",
        75 => "toml",
        _ => "",
    }
}

fn tag_to_feishu_lang_id(tag: &str) -> i64 {
    match tag.trim().to_ascii_lowercase().as_str() {
        "bash" => 7,
        "c#" | "csharp" | "cs" => 8,
        "c++" | "cpp" | "cxx" => 9,
        "c" => 10,
        "css" => 12,
        "dart" => 15,
        "dockerfile" | "docker" => 18,
        "go" | "golang" => 22,
        "groovy" => 23,
        "html" => 24,
        "http" => 26,
        "haskell" | "hs" => 27,
        "json" | "jsonc" => 28,
        "java" => 29,
        "javascript" | "js" | "jsx" => 30,
        "kotlin" | "kt" => 32,
        "latex" | "tex" => 33,
        "lua" => 36,
        "makefile" | "make" => 38,
        "markdown" | "md" => 39,
        "nginx" => 40,
        "objc" | "objective-c" => 41,
        "php" => 43,
        "perl" | "pl" => 44,
        "powershell" | "ps1" | "pwsh" => 46,
        "protobuf" | "proto" => 48,
        "python" | "py" => 49,
        "r" => 50,
        "ruby" | "rb" => 52,
        "rust" | "rs" => 53,
        "scss" => 55,
        "sql" => 56,
        "scala" => 57,
        "shell" | "sh" | "zsh" => 60,
        "swift" => 61,
        "typescript" | "ts" | "tsx" => 63,
        "xml" => 66,
        "yaml" | "yml" => 67,
        "cmake" => 68,
        "toml" => 75,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_common_markdown_features() {
        let md = "\
# Main Title

Hello **bold** and *italic* and ~~gone~~ and `code` and [Feishu](https://open.feishu.cn/docs?a=1&b=%E4%B8%AD%E6%96%87).

- Bullet A
  - Nested Bullet B
- [x] Done task
- [ ] Open task

1. First
2. Second

> Quoted line

```rust
fn main() {
    println!(\"hi\");
}
```

| Col1 | Col2 |
| --- | --- |
| v1 | v2 |

---
";

        let batches = markdown_to_docx_descendant_batches(md).unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];

        // Build DocxBlock tree as Feishu would return it
        let mut blocks = vec![DocxBlock {
            block_id: "doc_1".to_string(),
            block_type: 1,
            parent_id: String::new(),
            children: batch.children_id.clone(),
            payload: HashMap::new(),
        }];
        for val in &batch.descendants {
            blocks.push(serde_json::from_value(val.clone()).unwrap());
        }

        let rendered = docx_blocks_to_markdown("doc_1", &blocks);
        assert!(rendered.unconvertible_reasons.is_empty());
        assert_eq!(rendered.root_children_count, batch.children_id.len());
        assert!(rendered.markdown.contains("# Main Title"));
        assert!(rendered.markdown.contains("**bold**"));
        assert!(rendered.markdown.contains("*italic*"));
        assert!(rendered.markdown.contains("~~gone~~"));
        assert!(rendered.markdown.contains("`code`"));
        assert!(rendered.markdown.contains("https://open.feishu.cn/docs?a=1&b=%E4%B8%AD%E6%96%87"));
        assert!(rendered.markdown.contains("- Bullet A\n  - Nested Bullet B"));
        assert!(rendered.markdown.contains("- [x] Done task\n- [ ] Open task"));
        assert!(rendered.markdown.contains("1. First\n2. Second"));
        assert!(rendered.markdown.contains("> Quoted line"));
        assert!(rendered.markdown.contains("```rust\nfn main()"));
        assert!(rendered.markdown.contains("| Col1 | Col2 |"));
        assert!(rendered.markdown.contains("---"));
    }

    #[test]
    fn percent_encoding_roundtrips_links() {
        let raw = "https://open.feishu.cn/document?k=hello world&c=中文";
        let enc = sanitize_and_encode_feishu_link(raw).unwrap();
        assert!(!enc.contains(':'));
        assert!(!enc.contains('/'));
        assert_eq!(
            percent_decode_str(&enc),
            "https://open.feishu.cn/document?k=hello%20world&c=%E4%B8%AD%E6%96%87"
        );
    }

    #[test]
    fn coalesces_adjacent_nested_italic_and_bold_runs() {
        let blocks = vec![
            DocxBlock {
                block_id: "doc_e".to_string(),
                block_type: 1,
                parent_id: String::new(),
                children: vec!["b_p".to_string()],
                payload: HashMap::new(),
            },
            DocxBlock {
                block_id: "b_p".to_string(),
                block_type: 2,
                parent_id: "doc_e".to_string(),
                children: Vec::new(),
                payload: HashMap::from([(
                    "text".to_string(),
                    json!({
                        "elements": [
                            { "text_run": { "content": "规范", "text_element_style": { "italic": true } } },
                            { "text_run": { "content": "「都不」", "text_element_style": { "italic": true, "bold": true } } },
                            { "text_run": { "content": "存在。", "text_element_style": { "italic": true } } }
                        ]
                    }),
                )]),
            },
        ];
        let rendered = docx_blocks_to_markdown("doc_e", &blocks);
        assert_eq!(rendered.markdown, "*规范**「都不」**存在。*\n");
    }

    #[test]
    fn unconvertible_blocks_are_flagged_and_readable() {
        let blocks = vec![
            DocxBlock {
                block_id: "doc_rich".to_string(),
                block_type: 1,
                parent_id: String::new(),
                children: vec!["b_p".to_string(), "b_img".to_string(), "b_board".to_string()],
                payload: HashMap::new(),
            },
            DocxBlock {
                block_id: "b_p".to_string(),
                block_type: 2,
                parent_id: "doc_rich".to_string(),
                children: Vec::new(),
                payload: HashMap::from([(
                    "text".to_string(),
                    json!({ "elements": [{ "text_run": { "content": "Intro text" } }] }),
                )]),
            },
            DocxBlock {
                block_id: "b_img".to_string(),
                block_type: 27,
                parent_id: "doc_rich".to_string(),
                children: Vec::new(),
                payload: HashMap::from([("image".to_string(), json!({ "token": "img_tok_1" }))]),
            },
            DocxBlock {
                block_id: "b_board".to_string(),
                block_type: 43,
                parent_id: "doc_rich".to_string(),
                children: Vec::new(),
                payload: HashMap::from([("board".to_string(), json!({ "token": "brd_tok_1" }))]),
            },
        ];

        let rendered = docx_blocks_to_markdown("doc_rich", &blocks);
        assert_eq!(
            rendered.unconvertible_reasons,
            vec!["图片 (image)", "画板 (board)"]
        );
        assert!(rendered.markdown.contains("Intro text"));
        assert!(rendered.markdown.contains("![image](feishu-image://img_tok_1)"));
        assert!(rendered.markdown.contains("<!-- feishu:画板 (board) token=brd_tok_1 -->"));
    }

    #[test]
    fn chunks_large_documents_under_1000_blocks() {
        let mut md = String::new();
        for i in 0..950 {
            md.push_str(&format!("Paragraph {i}\n\n"));
        }
        let batches = markdown_to_docx_descendant_batches(&md).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches[0].descendants.len() <= MAX_DESCENDANT_BATCH_BLOCKS);
        assert!(batches[1].descendants.len() <= MAX_DESCENDANT_BATCH_BLOCKS);
    }

    #[test]
    fn heading_children_and_unknown_inline_elements_are_preserved_and_flagged() {
        let blocks = vec![
            DocxBlock {
                block_id: "doc_nest".to_string(),
                block_type: 1,
                parent_id: String::new(),
                children: vec!["h1".to_string()],
                payload: HashMap::new(),
            },
            DocxBlock {
                block_id: "h1".to_string(),
                block_type: 4,
                parent_id: "doc_nest".to_string(),
                children: vec!["h_child".to_string(), "h_img".to_string()],
                payload: HashMap::from([(
                    "heading2".to_string(),
                    json!({
                        "elements": [
                            { "text_run": { "content": "Section " } },
                            { "inline_block": { "block_id": "inl_1" } }
                        ]
                    }),
                )]),
            },
            DocxBlock {
                block_id: "h_child".to_string(),
                block_type: 2,
                parent_id: "h1".to_string(),
                children: Vec::new(),
                payload: HashMap::from([(
                    "text".to_string(),
                    json!({ "elements": [{ "text_run": { "content": "Child under heading" } }] }),
                )]),
            },
            DocxBlock {
                block_id: "h_img".to_string(),
                block_type: 27,
                parent_id: "h1".to_string(),
                children: Vec::new(),
                payload: HashMap::from([("image".to_string(), json!({ "token": "img_sub" }))]),
            },
        ];

        let rendered = docx_blocks_to_markdown("doc_nest", &blocks);
        assert!(rendered.markdown.contains("## Section [feishu:inline_block]"));
        assert!(rendered.markdown.contains("Child under heading"));
        assert!(rendered.markdown.contains("![image](feishu-image://img_sub)"));
        assert_eq!(
            rendered.unconvertible_reasons,
            vec!["未知行内元素 (inline_block)", "图片 (image)"]
        );
    }
}
