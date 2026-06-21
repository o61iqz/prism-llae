//! Beautified `.xlsx` export for sweep results (`--report`)

use std::io::Result as IoResult;

pub struct Row {
    pub rate: u32,
    pub backend: String,
    pub mode: String,
    pub req_format: String,
    pub act_format: String,
    pub req_block: u32,
    pub act_block: u32,
    pub min_f: u32,
    pub median_f: u32,
    pub mean_f: f64,
    pub max_f: u32,
    pub min_ms: f64,
    pub median_ms: f64,
    pub mean_ms: f64,
    pub max_ms: f64,
    pub jitter: u32,
}

const STYLES_XML: &str = include_str!("report/styles.xml");
const THEME_XML: &str = include_str!("report/theme1.xml");

const HEADERS: [&str; 17] = [
    "rate", "backend", "mode", "req_format", "act_format", "req_block", "act_block",
    "min_frames", "median_frames", "mean_frames", "max_frames",
    "min_ms", "median_ms", "mean_ms", "max_ms", "jitter_frames", "Notes",
];

#[derive(Clone, Copy, PartialEq)]
enum Edge {
    None,
    Dotted,
    Thin,
    Thick,
}

fn col_letter(c: usize) -> char {
    (b'A' + c as u8) as char
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn write(path: &str, rows: &[Row], notes: &[String]) -> IoResult<()> {
    let sheet = build_sheet(rows, notes);
    let parts: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", CONTENT_TYPES.into()),
        ("_rels/.rels", RELS.into()),
        ("xl/workbook.xml", WORKBOOK.into()),
        ("xl/_rels/workbook.xml.rels", WB_RELS.into()),
        ("xl/styles.xml", STYLES_XML.as_bytes().to_vec()),
        ("xl/theme/theme1.xml", THEME_XML.as_bytes().to_vec()),
        ("xl/worksheets/sheet1.xml", sheet.into_bytes()),
    ];
    std::fs::write(path, zip_store(&parts))
}

fn build_sheet(rows: &[Row], notes: &[String]) -> String {
    let n = rows.len();
    let last = n + 1; // header = row 1

    let edge_at = |i: usize| -> (bool, bool, bool) {
        let r = &rows[i];
        let is_last = i + 1 == n;
        let rate_end = is_last || rows[i + 1].rate != r.rate;
        let backend_end = rate_end || rows[i + 1].backend != r.backend;
        let fmt_end =
            backend_end || rows[i + 1].mode != r.mode || rows[i + 1].req_format != r.req_format;
        (rate_end, backend_end, fmt_end)
    };
    let mode_end_at = |i: usize| -> bool {
        let (_, backend_end, _) = edge_at(i);
        backend_end || (i + 1 < n && rows[i + 1].mode != rows[i].mode)
    };

    let group_kind = |i: usize| -> Edge {
        let (rate_end, backend_end, _) = edge_at(i);
        if rate_end {
            Edge::Thick
        } else if backend_end {
            Edge::Thin
        } else {
            Edge::Dotted
        }
    };
    let row_kind = |i: usize| -> Edge {
        let (rate_end, backend_end, fmt_end) = edge_at(i);
        if rate_end {
            Edge::Thick
        } else if backend_end {
            Edge::Thin
        } else if fmt_end || rows[i].req_block == 0 {
            Edge::Dotted
        } else {
            Edge::None
        }
    };

    // style
    let s_general = |e: Edge| match e {
        Edge::None => 4,
        Edge::Dotted => 5,
        Edge::Thin => 14,
        Edge::Thick => 17,
    };
    let s_ms = |e: Edge| match e {
        Edge::None => 10,
        Edge::Dotted => 6,
        Edge::Thin => 15,
        Edge::Thick => 18,
    };
    let s_jitter = |e: Edge| match e {
        Edge::None => 9,
        Edge::Dotted => 7,
        Edge::Thin => 11,
        Edge::Thick => 19,
    };
    let s_covered = |e: Edge| match e {
        Edge::Dotted => 12,
        Edge::Thin => 13,
        _ => 16,
    };

    // merge run keys for columns A(rate)-E(act_format)
    let merged_key = |i: usize, c: usize| -> String {
        let r = &rows[i];
        match c {
            0 => format!("{}", r.rate),
            1 => format!("{}|{}", r.rate, r.backend),
            2 => format!("{}|{}|{}", r.rate, r.backend, r.mode),
            3 => format!("{}|{}|{}|{}", r.rate, r.backend, r.mode, r.req_format),
            _ => format!("{}|{}|{}|{}|{}", r.rate, r.backend, r.mode, r.req_format, r.act_format),
        }
    };
    let is_run_start = |i: usize, c: usize| i == 0 || merged_key(i, c) != merged_key(i - 1, c);
    let group_ends = |i: usize, c: usize| -> bool {
        let (rate_end, backend_end, fmt_end) = edge_at(i);
        match c {
            0 => rate_end,
            1 => backend_end,
            2 => mode_end_at(i),
            _ => fmt_end,
        }
    };

    let mut s = String::with_capacity(64 * 1024);
    s.push_str(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#);
    s.push_str(r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#);
    s.push_str(r#"<sheetPr><outlinePr summaryBelow="0" summaryRight="0"/></sheetPr>"#);
    s.push_str(r#"<sheetViews><sheetView workbookViewId="0"><pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/><selection pane="bottomLeft" activeCell="A2" sqref="A2"/></sheetView></sheetViews>"#);
    s.push_str(r#"<sheetFormatPr customHeight="1" defaultColWidth="12.63" defaultRowHeight="15.75"/>"#);
    s.push_str(r#"<cols><col customWidth="1" min="17" max="17" width="54.63"/></cols>"#);
    s.push_str("<sheetData>");

    // header row
    s.push_str(r#"<row r="1">"#);
    for (c, h) in HEADERS.iter().enumerate() {
        let style = if c <= 10 {
            1
        } else if c <= 14 {
            2
        } else {
            3
        };
        s.push_str(&format!(
            r#"<c r="{}1" s="{}" t="inlineStr"><is><t>{}</t></is></c>"#,
            col_letter(c),
            style,
            esc(h)
        ));
    }
    s.push_str("</row>");

    // data rows
    for i in 0..n {
        let r = &rows[i];
        let rr = i + 2;
        let rk = row_kind(i);
        s.push_str(&format!(r#"<row r="{rr}">"#));

        // merged config columns A-E
        for c in 0..5 {
            let letter = col_letter(c);
            let start = is_run_start(i, c);
            let ends = group_ends(i, c);
            if start {
                let style = if ends { s_general(group_kind(i)) } else { 4 };
                let val = match c {
                    0 => num_cell(&letter, rr, style, &r.rate.to_string()),
                    1 => str_cell(&letter, rr, style, &r.backend),
                    2 => str_cell(&letter, rr, style, &r.mode),
                    3 => str_cell(&letter, rr, style, &r.req_format),
                    _ => str_cell(&letter, rr, style, &r.act_format),
                };
                s.push_str(&val);
            } else if ends {
                s.push_str(&format!(
                    r#"<c r="{letter}{rr}" s="{}"/>"#,
                    s_covered(group_kind(i))
                ));
            }
        }

        // F req_block
        if r.req_block == 0 {
            s.push_str(&str_cell(&'F', rr, s_general(rk), "auto"));
        } else {
            s.push_str(&num_cell(&'F', rr, s_general(rk), &r.req_block.to_string()));
        }
        // G act_block, H-K frames
        s.push_str(&num_cell(&'G', rr, s_general(rk), &r.act_block.to_string()));
        s.push_str(&num_cell(&'H', rr, s_general(rk), &r.min_f.to_string()));
        s.push_str(&num_cell(&'I', rr, s_general(rk), &r.median_f.to_string()));
        s.push_str(&num_cell(&'J', rr, s_general(rk), &fmt_num(r.mean_f)));
        s.push_str(&num_cell(&'K', rr, s_general(rk), &r.max_f.to_string()));
        // L-O ms
        s.push_str(&num_cell(&'L', rr, s_ms(rk), &format!("{:.3}", r.min_ms)));
        s.push_str(&num_cell(&'M', rr, s_ms(rk), &format!("{:.3}", r.median_ms)));
        s.push_str(&num_cell(&'N', rr, s_ms(rk), &format!("{:.3}", r.mean_ms)));
        s.push_str(&num_cell(&'O', rr, s_ms(rk), &format!("{:.3}", r.max_ms)));
        // P jitter
        s.push_str(&num_cell(&'P', rr, s_jitter(rk), &r.jitter.to_string()));

        // Q Notes column
        let note = notes.get(i);
        let q_style = if i + 1 == n {
            19
        } else if i == 0 {
            8
        } else if i == 5 {
            11
        } else {
            9
        };
        match note {
            Some(text) if !text.is_empty() => s.push_str(&str_cell(&'Q', rr, q_style, text)),
            _ => s.push_str(&format!(r#"<c r="Q{rr}" s="{q_style}"/>"#)),
        }

        s.push_str("</row>");
    }
    s.push_str("</sheetData>");

    // merges
    let mut merges: Vec<String> = Vec::new();
    for c in 0..5 {
        let letter = col_letter(c);
        let mut start = 0usize;
        for i in 0..n {
            let end_of_run = i + 1 == n || merged_key(i, c) != merged_key(i + 1, c);
            if end_of_run {
                if i > start {
                    merges.push(format!(
                        r#"<mergeCell ref="{letter}{}:{letter}{}"/>"#,
                        start + 2,
                        i + 2
                    ));
                }
                start = i + 1;
            }
        }
    }
    s.push_str(&format!(r#"<mergeCells count="{}">"#, merges.len()));
    for m in &merges {
        s.push_str(m);
    }
    s.push_str("</mergeCells>");

    // conditional formatting
    let mut prio = 1;
    let mut cf = |sqref: String, rule: String, p: &mut i32| {
        s.push_str(&format!(
            r#"<conditionalFormatting sqref="{sqref}"><cfRule {rule}</conditionalFormatting>"#
        ));
        *p += 1;
    };
    let eq = |dxf: i32, p: i32, val: &str| {
        format!(
            r#"type="cellIs" dxfId="{dxf}" priority="{p}" operator="equal"><formula>"{val}"</formula></cfRule>"#
        )
    };
    let scale = |p: i32| {
        format!(
            r#"type="colorScale" priority="{p}"><colorScale><cfvo type="min"/><cfvo type="percentile" val="50"/><cfvo type="max"/><color rgb="FFD9EAD3"/><color rgb="FFFFF2CC"/><color rgb="FFF4CCCC"/></colorScale></cfRule>"#
        )
    };
    let b = format!("B1:B{last}");
    let c = format!("C1:C{last}");
    let de = format!("D1:E{last}");
    cf(b.clone(), eq(0, prio, "WASAPI"), &mut prio);
    cf(b.clone(), eq(1, prio, "WDM/KS"), &mut prio);
    cf(b, eq(2, prio, "ASIO"), &mut prio);
    cf(c.clone(), eq(3, prio, "-"), &mut prio);
    cf(c.clone(), eq(4, prio, "shared"), &mut prio);
    cf(c, eq(2, prio, "exclusive"), &mut prio);
    cf(format!("A2:A{last}"), scale(prio), &mut prio);
    cf(de.clone(), eq(5, prio, "auto"), &mut prio);
    cf(de.clone(), eq(0, prio, "i16"), &mut prio);
    cf(de.clone(), eq(1, prio, "i24"), &mut prio);
    cf(de.clone(), eq(2, prio, "i32"), &mut prio);
    cf(format!("F1:F{last}"), eq(5, prio, "auto"), &mut prio);
    cf(format!("F1:G{last}"), scale(prio), &mut prio);
    cf(de.clone(), eq(6, prio, "i32/24"), &mut prio);
    cf(de, eq(5, prio, "f32"), &mut prio);
    cf(format!("H2:K{last}"), scale(prio), &mut prio);
    cf(format!("L2:O{last}"), scale(prio), &mut prio);
    cf(
        format!("P2:P{last}"),
        format!(r#"type="cellIs" dxfId="2" priority="{prio}" operator="equal"><formula>0</formula></cfRule>"#),
        &mut prio,
    );
    cf(
        format!("P2:P{last}"),
        format!(r#"type="cellIs" dxfId="0" priority="{prio}" operator="greaterThan"><formula>0</formula></cfRule>"#),
        &mut prio,
    );

    s.push_str("</worksheet>");
    s
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn num_cell(col: &char, row: usize, style: i32, v: &str) -> String {
    format!(r#"<c r="{col}{row}" s="{style}"><v>{v}</v></c>"#)
}

fn str_cell(col: &char, row: usize, style: i32, v: &str) -> String {
    format!(
        r#"<c r="{col}{row}" s="{style}" t="inlineStr"><is><t>{}</t></is></c>"#,
        esc(v)
    )
}

// == static parts ===========================================

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default ContentType="application/xml" Extension="xml"/><Default ContentType="application/vnd.openxmlformats-package.relationships+xml" Extension="rels"/><Override ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml" PartName="/xl/worksheets/sheet1.xml"/><Override ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml" PartName="/xl/styles.xml"/><Override ContentType="application/vnd.openxmlformats-officedocument.theme+xml" PartName="/xl/theme/theme1.xml"/><Override ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" PartName="/xl/workbook.xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet state="visible" name="Latency Sweep" sheetId="1" r:id="rId3"/></sheets></workbook>"#;

const WB_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;

// == minimal zip (store, no compression) ====================

fn zip_store(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offsets = Vec::new();

    for (name, data) in entries {
        offsets.push(out.len() as u32);
        let crc = crc32(data);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header sig
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // store, no compression
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }

    for (i, (name, data)) in entries.iter().enumerate() {
        let crc = crc32(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header sig
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offsets[i].to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }

    let cd_off = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD sig
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
