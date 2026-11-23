//! Rich-message table conversion for external feed HTML.

use std::sync::LazyLock;

use regex::Regex;

use crate::htmltext::{
    balance_html_tags, decode_entities, escape_html, find_balanced_close, visible_len,
};

const MAX_COLUMNS: usize = 20;

static RE_TABLE_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<table\b([^>]*)>").unwrap());
static RE_TABLE_CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)</table\s*>").unwrap());
static RE_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr\s*>").unwrap());
static RE_CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<t([hd])\b([^>]*)>(.*?)</t[dh]\s*>").unwrap());
static RE_CAPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<caption\b[^>]*>(.*?)</caption\s*>").unwrap());
static RE_LAYOUT_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?is)(?:^|\s)(?:role\s*=\s*["']?presentation\b"#,
        r#"|class\s*=\s*(?:"[^"]*\blayout\b[^"]*"|'[^']*\blayout\b[^']*'|[^\s"'>]*\blayout\b))"#,
    ))
    .unwrap()
});
static RE_COLSPAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:^|\s)colspan\s*=\s*["']?\s*([0-9]+)"#).unwrap());
static RE_ROWSPAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:^|\s)rowspan\s*=\s*["']?\s*([0-9]+)"#).unwrap());
static RE_ALIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)align\s*=\s*["']?\s*(left|center|right)\b"#).unwrap()
});
static RE_VALIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)valign\s*=\s*["']?\s*(top|middle|bottom)\b"#).unwrap()
});
static RE_STYLE_TEXT_ALIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\btext-align\s*:\s*(left|center|right)\b").unwrap());
static RE_STYLE_VALIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bvertical-align\s*:\s*(top|middle|bottom)\b").unwrap());
static RE_BLOCK_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<(?:img|video|audio|iframe|pre|blockquote|figure|h[1-6]|ul|ol|dl|form|svg|canvas|details|hr|style|script)\b",
    )
    .unwrap()
});

struct Cell {
    header: bool,
    content: String,
    colspan: usize,
    rowspan: Option<usize>,
    align: Option<String>,
    valign: Option<String>,
}

impl Cell {
    fn empty() -> Self {
        Self {
            header: false,
            content: String::new(),
            colspan: 1,
            rowspan: None,
            align: None,
            valign: None,
        }
    }
}

pub(crate) fn protect_rich_tables(html: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(html.len());
    let mut tables = Vec::new();
    let mut last = 0usize;
    let mut cursor = 0usize;
    while let Some(open) = RE_TABLE_OPEN.find(&html[cursor..]) {
        let start = cursor + open.start();
        let open_end = cursor + open.end();
        let Some((_, close_end)) =
            find_balanced_close(html, open_end, &RE_TABLE_OPEN, &RE_TABLE_CLOSE)
        else {
            break;
        };
        if let Some(table) = render_rich_table(&html[start..close_end]) {
            out.push_str(&html[last..start]);
            out.push_str("\u{FFFE}TBL");
            out.push_str(&tables.len().to_string());
            out.push('\u{FFFE}');
            tables.push(table);
            last = close_end;
        }
        cursor = close_end;
    }
    out.push_str(&html[last..]);
    (out, tables)
}

fn render_rich_table(table_html: &str) -> Option<String> {
    let open = RE_TABLE_OPEN.captures(table_html)?;
    let attrs = open.get(1).map(|m| m.as_str()).unwrap_or("");
    if RE_LAYOUT_ATTR.is_match(attrs) {
        return None;
    }
    if RE_TABLE_OPEN.find_iter(table_html).count() > 1 {
        return None;
    }
    if RE_BLOCK_MARKER.is_match(table_html)
        || table_html.contains("\u{FFFE}BQO\u{FFFE}")
        || table_html.contains("\u{FFFE}BQC\u{FFFE}")
    {
        return None;
    }

    let caption = RE_CAPTION
        .captures(table_html)
        .map(|c| sanitize_inline(&c[1]))
        .filter(|c| visible_len(c) > 0);

    let mut rows = Vec::new();
    for row in RE_ROW.captures_iter(table_html) {
        let mut cells = Vec::new();
        for cell in RE_CELL.captures_iter(&row[1]) {
            let content = sanitize_inline(&cell[3]);
            let content = if visible_len(&content) == 0 {
                String::new()
            } else {
                content
            };
            let attrs = &cell[2];
            let colspan = RE_COLSPAN
                .captures(attrs)
                .and_then(|c| c[1].parse::<usize>().ok())
                .unwrap_or(1);
            if colspan > MAX_COLUMNS {
                return None;
            }
            let rowspan = RE_ROWSPAN
                .captures(attrs)
                .and_then(|c| c[1].parse::<usize>().ok())
                .filter(|n| *n >= 2);
            cells.push(Cell {
                header: &cell[1] == "h",
                content,
                colspan: colspan.max(1),
                rowspan,
                align: cell_align(attrs),
                valign: cell_valign(attrs),
            });
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    if rows.len() < 2 {
        return None;
    }

    let mut max_columns = 0usize;
    for row in &rows {
        let used: usize = row.iter().map(|c| c.colspan).sum();
        if used > MAX_COLUMNS {
            return None;
        }
        max_columns = max_columns.max(used);
    }
    if max_columns < 2
        || !rows
            .iter()
            .any(|row| row.iter().any(|c| !c.content.is_empty()))
    {
        return None;
    }

    for row in &mut rows {
        let used: usize = row.iter().map(|c| c.colspan).sum();
        for _ in used..max_columns {
            row.push(Cell::empty());
        }
    }

    let mut out = String::from("<table bordered>");
    if let Some(caption) = caption {
        out.push_str("<caption>");
        out.push_str(&caption);
        out.push_str("</caption>");
    }
    for row in rows {
        out.push_str("<tr>");
        for cell in row {
            let tag = if cell.header { "th" } else { "td" };
            out.push('<');
            out.push_str(tag);
            if cell.colspan > 1 {
                out.push_str(&format!(" colspan=\"{}\"", cell.colspan));
            }
            if let Some(rowspan) = cell.rowspan {
                out.push_str(&format!(" rowspan=\"{rowspan}\""));
            }
            if let Some(align) = &cell.align {
                out.push_str(&format!(" align=\"{align}\""));
            }
            if let Some(valign) = &cell.valign {
                out.push_str(&format!(" valign=\"{valign}\""));
            }
            out.push('>');
            out.push_str(&cell.content);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
    Some(out)
}

fn cell_align(attrs: &str) -> Option<String> {
    RE_ALIGN
        .captures(attrs)
        .map(|c| c[1].to_ascii_lowercase())
        .or_else(|| {
            RE_STYLE_TEXT_ALIGN
                .captures(attrs)
                .map(|c| c[1].to_ascii_lowercase())
        })
}

fn cell_valign(attrs: &str) -> Option<String> {
    RE_VALIGN
        .captures(attrs)
        .map(|c| c[1].to_ascii_lowercase())
        .or_else(|| {
            RE_STYLE_VALIGN
                .captures(attrs)
                .map(|c| c[1].to_ascii_lowercase())
        })
}

fn sanitize_inline(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            if let Some(end) = rest.find("-->") {
                i += end + 3;
                continue;
            }
        }
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                let raw = &rest[..gt + 1];
                if let Some((closing, self_closing, name)) = tag_info(raw) {
                    match name.as_str() {
                        "br" => push_br(&mut out),
                        "p" | "div" | "section" | "article" | "header" | "footer" | "li" => {
                            push_br(&mut out)
                        }
                        "b" | "strong" | "i" | "em" | "u" | "ins" | "s" | "strike" | "del"
                        | "mark" | "sub" | "sup" | "code" => {
                            let tag = match name.as_str() {
                                "strong" => "b",
                                "em" => "i",
                                "ins" => "u",
                                "strike" | "del" => "s",
                                other => other,
                            };
                            if closing {
                                out.push_str("</");
                                out.push_str(tag);
                                out.push('>');
                            } else if !self_closing {
                                out.push('<');
                                out.push_str(tag);
                                out.push('>');
                            }
                        }
                        _ => {}
                    }
                }
                i += gt + 1;
                continue;
            }
            out.push_str(&normalize_text(rest));
            break;
        }
        let next = rest.find('<').map(|p| i + p).unwrap_or(html.len());
        out.push_str(&normalize_text(&html[i..next]));
        i = next;
    }
    trim_breaks(balance_html_tags(&out))
}

fn tag_info(tag: &str) -> Option<(bool, bool, String)> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let closing = inner.starts_with('/');
    let inner = inner.trim_start_matches('/');
    let self_closing = inner.trim_end().ends_with('/');
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if name.is_empty() {
        return None;
    }
    Some((closing, self_closing, name.to_ascii_lowercase()))
}

fn push_br(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with("<br/>") {
        out.push_str("<br/>");
    }
}

fn normalize_text(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut out = String::with_capacity(decoded.len());
    let mut whitespace = false;
    for c in decoded.chars() {
        if c.is_whitespace() || c == '\u{00A0}' {
            whitespace = true;
        } else {
            if whitespace {
                out.push(' ');
            }
            whitespace = false;
            out.push(c);
        }
    }
    if whitespace {
        out.push(' ');
    }
    escape_html(&out)
}

fn trim_breaks(mut html: String) -> String {
    html = html.replace(" <br/>", "<br/>").replace("<br/> ", "<br/>");
    while html.contains("<br/><br/>") {
        html = html.replace("<br/><br/>", "<br/>");
    }
    while let Some(rest) = html.strip_prefix("<br/>") {
        html = rest.trim_start().to_string();
    }
    while let Some(rest) = html.strip_suffix("<br/>") {
        html = rest.trim_end().to_string();
    }
    html.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_table_becomes_rich_table() {
        let html = r#"<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>"#;
        assert_eq!(
            render_rich_table(html).unwrap(),
            "<table bordered><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>"
        );
    }

    #[test]
    fn caption_cells_and_attributes_are_preserved() {
        let html = r#"<table><caption>表 1</caption><tr><th>项目</th><th>值</th></tr><tr><td colspan="2" align="center" valign="middle">A&amp;B<br>C</td></tr><tr><td>1</td><td>2</td></tr></table>"#;
        assert_eq!(
            render_rich_table(html).unwrap(),
            "<table bordered><caption>表 1</caption><tr><th>项目</th><th>值</th></tr><tr><td colspan=\"2\" align=\"center\" valign=\"middle\">A&amp;B<br/>C</td></tr><tr><td>1</td><td>2</td></tr></table>"
        );
    }

    #[test]
    fn tbody_and_inline_styles_are_ignored() {
        let html = concat!(
            r#"<table class="article-table" style="text-align:center;width:100%"><tbody>"#,
            r#"<tr style="background-color:#fff;color:#000;"><td nowrap="" style="line-height:18px;padding:8px 0;">_</td>"#,
            r#"<td nowrap="" style="font-size:14px;">1-10月累计</td></tr>"#,
            r#"<tr><td style="border-top:1px solid#999;">单位：亿元</td><td>2022年</td></tr>"#,
            r#"</tbody></table>"#
        );
        assert_eq!(
            render_rich_table(html).unwrap(),
            "<table bordered><tr><td>_</td><td>1-10月累计</td></tr><tr><td>单位：亿元</td><td>2022年</td></tr></table>"
        );
    }

    #[test]
    fn inline_aliases_are_canonicalized() {
        let html = concat!(
            "<table><tr><th>A</th><th>B</th></tr><tr><td>",
            "<strong>x</strong><em>y</em><ins>z</ins><del>w</del>",
            "</td><td>3</td></tr></table>"
        );
        let out = render_rich_table(html).unwrap();
        assert!(
            out.contains("<b>x</b><i>y</i><u>z</u><s>w</s>"),
            "out: {out}"
        );
    }

    #[test]
    fn paragraph_cells_become_line_breaks() {
        let html = r#"<table><tr><th>A</th><th>B</th></tr><tr><td><p>one</p><p>two</p></td><td>3</td></tr></table>"#;
        assert!(render_rich_table(html)
            .unwrap()
            .contains("<td>one<br/>two</td>"));
    }

    #[test]
    fn too_many_columns_fall_back() {
        let cells = "<td>x</td>".repeat(MAX_COLUMNS + 1);
        let html = format!("<table><tr>{cells}</tr><tr>{cells}</tr></table>");
        assert!(render_rich_table(&html).is_none());
    }

    #[test]
    fn layout_and_nested_tables_fall_back() {
        let layout = r#"<table><tr><td><ul><li>x</li></ul></td><td>y</td></tr><tr><td>1</td><td>2</td></tr></table>"#;
        assert!(render_rich_table(layout).is_none());
        let nested = r#"<table><tr><td><table><tr><td>x</td><td>y</td></tr></table></td><td>z</td></tr><tr><td>1</td><td>2</td></tr></table>"#;
        assert!(render_rich_table(nested).is_none());
        let presentation = r#"<table role="presentation"><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>"#;
        assert!(render_rich_table(presentation).is_none());
    }

    #[test]
    fn empty_or_single_axis_tables_fall_back() {
        let empty = r#"<table><tr><td> </td><td></td></tr><tr><td></td><td> </td></tr></table>"#;
        assert!(render_rich_table(empty).is_none());
        let one_row = r#"<table><tr><td>a</td><td>b</td></tr></table>"#;
        assert!(render_rich_table(one_row).is_none());
        let one_column = r#"<table><tr><td>a</td></tr><tr><td>b</td></tr></table>"#;
        assert!(render_rich_table(one_column).is_none());
    }

    #[test]
    fn protect_replaces_only_convertible_tables() {
        let html = concat!(
            "<p>before</p>",
            "<table><tr><td>a</td><td>b</td></tr><tr><td>1</td><td>2</td></tr></table>",
            "<table><tr><td><pre>x</pre></td><td>y</td></tr><tr><td>1</td><td>2</td></tr></table>",
            "<p>after</p>"
        );
        let (out, tables) = protect_rich_tables(html);
        assert_eq!(tables.len(), 1);
        assert!(out.contains("\u{FFFE}TBL0\u{FFFE}"));
        assert!(out.contains("<pre>x</pre>"));
        assert!(out.contains("after"));
    }
}
