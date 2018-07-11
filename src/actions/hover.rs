use span::{Span, ZeroIndexed, Row};
use analysis::{Def, DefKind};
use actions::InitActionContext;
use vfs::Vfs;
use lsp_data::*;
use racer;
use actions::requests::racer_coord;
use server::ResponseError;
use rustfmt::{format_input, Input as FmtInput};

use std::path::Path;

/// Cleanup documentation code blocks
pub fn process_docs(docs: &str) -> String {
    trace!("process_docs");
    let mut in_codeblock = false;
    let mut in_rust_codeblock = false;
    let mut processed_docs = String::with_capacity(docs.len());
    let mut last_line_ignored = false;
    for line in docs.lines() {
        let mut trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_rust_codeblock = trimmed == "```" || 
                trimmed.contains("rust") || 
                trimmed.contains("no_run") || 
                trimmed.contains("ignore") || 
                trimmed.contains("should_panic") ||
                trimmed.contains("compile_fail");
            in_codeblock = !in_codeblock;
            if !in_codeblock {
                in_rust_codeblock = false;
            }
        }
        let mut line = line.to_string();
        if in_rust_codeblock && trimmed.starts_with("```") {
            line = "```rust".into();
        }
        
        // Racer sometimes pulls out comment block headers from the standard library
        let ignore_slashes = line.starts_with("////");

        let is_attribute = trimmed.starts_with("#[");
        let ignore_whitespace = last_line_ignored && trimmed.is_empty();
        let ignore_line = ignore_whitespace || (in_rust_codeblock && trimmed.starts_with("#") && !is_attribute);
        let ignore_line = ignore_line || ignore_slashes;

        if !ignore_line {
            processed_docs.push_str(line.trim_right());
            processed_docs.push_str("\n");
            last_line_ignored = false;
        } else {
            last_line_ignored = true;
        }
    }

    processed_docs
}

/// Extracts documentation from the `file` at the specified `row_start`. If the row is
/// equal to `0`, the scan will include the current row and move _downward_. Otherwise,
/// the scan will ignore the specified row and move _upward_. The documentation is
/// run through a post-process to cleanup code blocks.
pub fn extract_docs(vfs: &Vfs, file: &Path, row_start: Row<ZeroIndexed>) -> Option<String> {
    let preceeding = if row_start.0 == 0 { false } else { true };
    let direction = if preceeding { "up" } else { "down" };
    debug!("extract_docs: row_start = {:?}, direction = {:?}, file = {:?}", row_start, direction, file);

    let mut docs: Vec<String> = Vec::new();
    let mut row = if preceeding { 
        Row::new_zero_indexed(row_start.0.saturating_sub(1)) 
    } else {
        Row::new_zero_indexed(row_start.0)
    };
    loop {
        match vfs.load_line(file, row) {
            Ok(line) => {
                let next_row = if preceeding { 
                    Row::new_zero_indexed(row.0.saturating_sub(1)) 
                } else {
                    Row::new_zero_indexed(row.0.saturating_add(1))
                };
                if row == next_row {
                    warn!("extract_docs: bailing out: prev_row == next_row; next_row = {:?}", next_row);
                    break;
                } else {
                    row = next_row;
                }
                let line = line.trim();
                trace!("extract_docs: row = {:?}, line = {}", row, line);
                if line.starts_with("#") {
                    // Ignore meta attributes
                    continue;
                } else if line.starts_with("///") || line.starts_with("//!") {
                    let doc_line = line[3..].into();
                    if preceeding {
                        docs.insert(0, doc_line);
                    } else {
                        docs.push(doc_line);
                    }
                } else if line.starts_with("//") {
                    // Ignore non-doc comments, but continue scanning. This is required to skip copyright
                    // notices at the start of modules.
                    continue;
                } else if line.is_empty() {
                    // Ignore the gap that's often between the copyright notice and module level docs.
                    continue;
                } else if line.starts_with("////") {
                    // Break if we reach a comment header block (which is frequent in the standard library)
                    break;
                } else {
                    // Otherwise, we've reached the end of the docs 
                    break;
                }
            },
            Err(e) => {
                error!("extract_docs: error = {:?}", e);
            }
        }
    }
    trace!("extract_docs: lines = {:?}", docs);
    let docs = process_docs(&docs.join("\n"));
    if docs.trim().is_empty() {
        None
    } else {
        Some(docs)
    }
}

fn local_variable_usage(ctx: &InitActionContext, def: &Def) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    let mut context = String::new();
    match ctx.vfs.load_line(&def.span.file, def.span.range.row_start) {
        Ok(line) => {
            context.push_str(line.trim());
        },
        Err(e) => {
            error!("hover_local_variable_declaration: error = {:?}", e);
        }
    }
    if context.ends_with("{") {
        context.push_str(" ... }");
    }

    let mut tooltip = vec![];
    if context.is_empty() {
        tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", the_type)));
    } else {
        tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}\n\n", the_type)));
        tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", context)));
    }

    tooltip
}

fn field_or_variant(ctx: &InitActionContext, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    let docs = extract_docs(&ctx.vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", the_type)));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url + "\n\n"));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }

    tooltip
}

fn struct_enum_union_trait(ctx: &InitActionContext, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    // fallback in case source extration fails
    let the_type = || match def.kind {
        DefKind::Struct => format!("struct {}", def.name),
        DefKind::Enum => format!("enum {}", def.name),
        DefKind::Union => format!("union {}", def.name),
        DefKind::Trait => format!("trait {}", def.value),
        _ => def.value.trim().into()
    };

    let the_type = {
        let mut lines = Vec::new();
        let mut row = def.span.range.row_start;
        loop {
            match ctx.vfs.load_line(&def.span.file, row) {
                Ok(line) => {
                    row = Row::new_zero_indexed(row.0.saturating_add(1));
                    let mut line = line.trim();
                    if let Some(pos) = line.rfind("{") {
                        line = &line[0..pos];
                        lines.push(line.into());
                        break;
                    } else if let Some(pos) = line.rfind(";") {
                        line = &line[0..pos];
                        lines.push(line.into());
                        break;
                    } else {
                        lines.push(line.into());
                    }
                },
                Err(e) => {
                    warn!("hover: structure_enum_union_trait: failed to load source: {:?}", e);
                    lines.clear();
                    lines.push(the_type());
                }
            }
        }
        let decl = lines.join(" ");
        let mut decl = decl.trim();
        if let (Some(pos), true) = (decl.rfind("("), decl.ends_with(")")) {
             decl = &decl[0..pos];
        }
        format_object(ctx, decl.to_string())
    };
    
    let docs = extract_docs(&ctx.vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", the_type)));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url + "\n\n"));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

fn mod_use(ctx: &InitActionContext, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    
    let docs = extract_docs(&ctx.vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", the_type)));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url + "\n\n"));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

fn function_method(ctx: &InitActionContext, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = def.value.trim()
        .replacen("fn ", &format!("fn {}", def.name), 1)
        .replace("> (", ">(").replace("->(", "-> (");

    let the_type = format_method(ctx, the_type);
    
    let docs = extract_docs(&ctx.vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), format!("{}", the_type)));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url + "\n\n"));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

/// Extracts hover docs and the context string information using racer.
fn racer_docs(ctx: &InitActionContext, span: &Span<ZeroIndexed>, def: Option<&Def>) -> Option<(String, Option<String>)> {
    trace!("hover_racer: def.name: {:?}", def.map(|def| &def.name));
    let vfs = ctx.vfs.clone();
    let file_path = &span.file;

    if !file_path.as_path().exists() {
        trace!("hover_racer: skipping non-existant file: {:?}", file_path);
        return None;
    }

    let name = vfs.load_line(file_path.as_path(), span.range.row_start).ok().and_then(|line| {
        let col_start = span.range.col_start.0 as usize;
        let col_end = span.range.col_end.0 as usize;
        line.get(col_start..col_end).map(|line| line.to_string())
    });

    trace!("hover_racer: name: {:?}", name);
    
    let results = ::std::panic::catch_unwind(move || {
        let cache = racer::FileCache::new(vfs);
        let session = racer::Session::new(&cache);
        let row = span.range.row_end.one_indexed();
        let coord = racer_coord(row, span.range.col_end);
        let location = racer::Location::Coords(coord);
        trace!("hover_racer: file_path: {:?}, location: {:?}", file_path, location);
        let matches = racer::complete_from_file(file_path, location, &session);
        matches
            // Remove any matches that don't match the def or span name.
            .filter(|m| {
                def.map(|def| def.name == m.matchstr)
                   .unwrap_or(name.as_ref().map(|name| name == &m.matchstr).unwrap_or(false))
            })
            // Avoid duplicate lines when the context string and the name are the same
            .filter(|m| {
                name.as_ref().map(|name| name != &m.contextstr).unwrap_or(true)
            })
            .map(|m| {
                let mut ty = None;
                trace!("hover_racer: contextstr: {:?}", m.contextstr);
                let contextstr = m.contextstr.trim_right_matches("{").trim();
                match m.mtype {
                    racer::MatchType::Module => {
                        // Ignore
                    },
                    racer::MatchType::Function => {
                        let the_type = format_method(ctx, contextstr.into());
                        if !the_type.is_empty() {
                            ty = Some(the_type.into())
                        }
                    },
                    racer::MatchType::Trait | racer::MatchType::Enum | racer::MatchType::Struct => {
                        let the_type = format_object(ctx, contextstr.into());
                        if !the_type.is_empty() {
                            ty = Some(the_type.into())
                        }
                    },
                    _ => {
                        if !contextstr.trim().is_empty() {
                            ty = Some(contextstr.into())
                        }
                    }
                }
                trace!("hover_racer: racer_ty: {:?}", ty);
                (m.docs, ty)
            })
            .next()
    });

    let results = results.map_err(|_| {
        warn!("hover_racer: racer panicked");
    });

    results.unwrap_or(None)
}

/// Formats a struct, enum, union, or trait. The original type is returned
/// in the event of an error.
fn format_object(ctx: &InitActionContext, the_type: String) -> String {
    trace!("hover: format_object: {}", the_type);
    let config = ctx.fmt_config().get_rustfmt_config().clone();
    let object = format!("{}{{}}", the_type);
    let mut out = Vec::<u8>::with_capacity(the_type.len());
    match format_input(FmtInput::Text(object), &config, Some(&mut out)) {
        Ok(_) => {
            let utf8 = String::from_utf8(out);
            if let Ok((Some(pos), lines)) = utf8.map(|lines| (lines.rfind("{"), lines)) {
                lines[0..pos].into()
            } else {
                the_type
            }
        },
        Err(e) => {
            warn!("hover: object format failed: {:?}", e);
            the_type
        }
    }
}

/// Formats a method or function. The original type is returned
/// in the event of an error.
fn format_method(ctx: &InitActionContext, the_type: String) -> String {
    trace!("hover: format_method: {}", the_type);
    let config = ctx.fmt_config().get_rustfmt_config().clone();
    let method = format!("impl Dummy {{ {} {{ unimplmented!() }} }}", the_type);
    let mut out = Vec::<u8>::with_capacity(the_type.len());
    match format_input(FmtInput::Text(method), &config, Some(&mut out)) {
        Ok(_) => {
            if let Ok(mut lines) = String::from_utf8(out) {
                if let Some(front_pos) = lines.find("{") {
                    lines = lines[front_pos..].chars().skip(1).collect();
                }
                if let Some(back_pos) = lines.rfind("{") {
                    lines = lines[0..back_pos].into();
                }
                lines.lines().filter(|line| line.trim() != "").map(|line| {
                    let mut spaces = config.tab_spaces() + 1;
                    let should_trim = |c: char| {
                        spaces = spaces.saturating_sub(1);
                        spaces > 0 && c.is_whitespace()
                    };
                    let line = line.trim_left_matches(should_trim);
                    format!("{}\n", line)
                }).collect()
            } else {
                the_type
            }
        },
        Err(e) => {
            warn!("hover: method format failed: {:?}", e);
            the_type
        }
    }
}

pub fn tooltip(
    ctx: &InitActionContext,
    params: &TextDocumentPositionParams
) -> Result<Vec<MarkedString>, ResponseError> {
    let analysis = &ctx.analysis;

    let hover_file_path = parse_file_path!(&params.text_document.uri, "hover")?;
    let hover_span = ctx.convert_pos_to_span(hover_file_path, params.position);
    let hover_span_ty = analysis.show_type(&hover_span).unwrap_or_else(|_| String::new());
    let hover_span_def = analysis.id(&hover_span).and_then(|id| analysis.get_def(id));
    let hover_span_docs = analysis.docs(&hover_span).unwrap_or_else(|_| String::new());

    trace!("hover: span: {:?}", hover_span);
    trace!("hover: span_def: {:?}", hover_span_def);
    trace!("hover: span_ty: {:?}", hover_span_ty);

    let doc_url = analysis.doc_url(&hover_span).ok();
    
    let mut contents = vec![];

    let racer_fallback = |contents: &mut Vec<MarkedString>| {
        if let Some((racer_docs, racer_ty)) = racer_docs(&ctx, &hover_span, None) {
            let docs = process_docs(&racer_docs);
            let docs = if docs.trim().is_empty() { hover_span_docs } else { docs };
            let ty = racer_ty.unwrap_or(hover_span_ty);
            let ty = ty.trim();
            if !ty.is_empty() {
                contents.push(MarkedString::from_language_code("rust".into(), format!("{}", ty)));
            }
            if !docs.is_empty() {
                contents.push(MarkedString::from_markdown(format!("{}", docs)));
            }
        }
    };
    
    if let Ok(def) = hover_span_def {
        if def.kind == DefKind::Local && def.span == hover_span && def.qualname.contains("$") {
            trace!("hover: local variable declaration: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), format!("{}", def.value.trim())));
        } else if def.kind == DefKind::Local && def.span != hover_span && !def.qualname.contains("$") {
            trace!("hover: function argument usage: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), format!("{}", def.value.trim())));
        } else if def.kind == DefKind::Local && def.span != hover_span && def.qualname.contains("$") {
            trace!("hover: local variable usage: {}", def.name);
            contents.extend(local_variable_usage(&ctx, &def));
        } else if def.kind == DefKind::Local && def.span == hover_span {
            trace!("hover: function signature argument: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), format!("{}", def.value.trim())));
        } else { match def.kind {
            DefKind::TupleVariant | DefKind::StructVariant | DefKind::Field => {
                trace!("hover: field or variant: {}", def.name);
                contents.extend(field_or_variant(&ctx, &def, doc_url));
            },
            DefKind::Enum | DefKind::Union | DefKind::Struct | DefKind::Trait => {
                trace!("hover: struct, enum, union, or trait: {}", def.name);
                contents.extend(struct_enum_union_trait(&ctx, &def, doc_url));
            },
            DefKind::Function | DefKind::Method => {
                trace!("hover: function or method: {}", def.name);
                contents.extend(function_method(&ctx, &def, doc_url));
            },
            DefKind::Mod => {
                trace!("hover: mod usage: {}", def.name);
                contents.extend(mod_use(&ctx, &def, doc_url));
            },
            DefKind::Static | DefKind::Const => {
                trace!("hover: static or const (using racer): {}", def.name);
                racer_fallback(&mut contents);
            },
            _ => {
                trace!("hover: ignoring def = {:?}", def);
            }
        }}
    } else {
        trace!("hover: def is empty; falling back to racer");
        racer_fallback(&mut contents);
    }

    Ok(contents)
}