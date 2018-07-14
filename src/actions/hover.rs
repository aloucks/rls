use span::{Span, ZeroIndexed, Row};
use analysis::{Def, DefKind};
use actions::InitActionContext;
use vfs::{self, Vfs};
use lsp_data::*;
use racer;
use actions::requests::racer_coord;
use server::ResponseError;
use rustfmt::{self, Input as FmtInput};
use config::FmtConfig;

use std::path::Path;
use std::sync::Arc;

/// Cleanup documentation code blocks. The `docs` are expected to have the preceeding `///` or `//!`
/// prefixes already trimmed.
pub fn process_docs(docs: &str) -> String {
    trace!("process_docs");
    let mut in_codeblock = false;
    let mut in_rust_codeblock = false;
    let mut processed_docs = Vec::new();
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

        let is_attribute = trimmed.starts_with("#[") && in_rust_codeblock;
        let is_hidden = trimmed.starts_with("#") && in_rust_codeblock && !is_attribute;

        let ignore_whitespace = last_line_ignored && trimmed.is_empty();
        let ignore_line = ignore_slashes || ignore_whitespace || is_hidden;

        if !ignore_line {
            processed_docs.push(line);
            last_line_ignored = false;
        } else {
            last_line_ignored = true;
        }
    }

    processed_docs.join("\n")
}

/// Extracts documentation from the `file` at the specified `row_start`. If the row is
/// equal to `0`, the scan will include the current row and move _downward_. Otherwise,
/// the scan will ignore the specified row and move _upward_.
/// 
/// The documentation is run through a post-process to cleanup code blocks.
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
                    let pos = if line.chars().skip(3).next().map(|c| c.is_whitespace()).unwrap_or(false) {
                        4
                    } else {
                        3
                    };
                    let doc_line = line[pos..].into();
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

fn tooltip_local_variable_usage(vfs: &Vfs, def: &Def) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    let mut context = String::new();
    match vfs.load_line(&def.span.file, def.span.range.row_start) {
        Ok(line) => {
            context.push_str(line.trim());
        },
        Err(e) => {
            error!("local_variable_usage: error = {:?}", e);
        }
    }
    if context.ends_with("{") {
        context.push_str(" ... }");
    }

    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), the_type.into()));
    if !context.is_empty() {
        tooltip.push(MarkedString::from_language_code("rust".into(), context));
    }

    tooltip
}

fn tooltip_field_or_variant(vfs: &Vfs, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    let docs = extract_docs(&vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());

    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), the_type.into()));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }

    tooltip
}

/// Extracts a function, method, struct, enum, or trait decleration from source.
fn extract_decleration(vfs: &Vfs, file: &Path, mut row: Row<ZeroIndexed>) -> Result<Vec<String>, vfs::Error> {
    debug!("extract_decleration: row_start: {:?}, file: {:?}", row, file);
    let mut lines = Vec::new();
    loop {
        match vfs.load_line(file, row) {
            Ok(line) => {
                row = Row::new_zero_indexed(row.0.saturating_add(1));
                let mut line = line.trim();
                if let Some(pos) = line.rfind("{") {
                    line = &line[0..pos].trim_right();
                    lines.push(line.into());
                    break;
                } else if let Some(pos) = line.rfind(";") {
                    line = &line[0..pos].trim_right();
                    lines.push(line.into());
                    break;
                } else {
                    lines.push(line.into());
                }
            },
            Err(e) => {
                trace!("extract_decleration error: {:?}", e);
                return Err(e);
            }
        }
    }
    Ok(lines)
}

fn tooltip_struct_enum_union_trait(vfs: &Vfs, fmt_config: &FmtConfig, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    // fallback in case source extration fails
    let the_type = || match def.kind {
        DefKind::Struct => format!("struct {}", def.name),
        DefKind::Enum => format!("enum {}", def.name),
        DefKind::Union => format!("union {}", def.name),
        DefKind::Trait => format!("trait {}", def.value),
        _ => def.value.trim().into()
    };

    let the_type = {
        let decl = extract_decleration(vfs, &def.span.file, def.span.range.row_start)
            .map(|lines| lines.join("\n"))
            .unwrap_or(the_type());
        format_object(fmt_config, decl.to_string())
    };
    
    let docs = extract_docs(vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), the_type));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

fn tooltip_mod(vfs: &Vfs, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = def.value.trim();
    
    let docs = extract_docs(vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), the_type.into()));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

fn tooltip_function_method(vfs: &Vfs, fmt_config: &FmtConfig, def: &Def, doc_url: Option<String>) -> Vec<MarkedString> {
    let the_type = || def.value.trim()
        .replacen("fn ", &format!("fn {}", def.name), 1)
        .replace("> (", ">(").replace("->(", "-> (");

    let decl = extract_decleration(vfs, &def.span.file, def.span.range.row_start)
        .map(|lines| lines.join("\n"));
    
    let the_type = format_method(fmt_config, decl.unwrap_or(the_type()));
    
    let docs = extract_docs(&vfs, def.span.file.as_ref(), def.span.range.row_start)
        .unwrap_or_else(|| def.docs.trim().into());
    
    let mut tooltip = vec![];
    tooltip.push(MarkedString::from_language_code("rust".into(), the_type));
    if let Some(doc_url) = doc_url {
        tooltip.push(MarkedString::from_markdown(doc_url));
    }
    if !docs.is_empty() {
        tooltip.push(MarkedString::from_markdown(docs));
    }
    
    tooltip
}

/// Extracts documentation and the context string information using racer.
fn racer(vfs: Arc<Vfs>, fmt_config: FmtConfig, span: &Span<ZeroIndexed>, def: Option<&Def>) -> Option<(String, Option<String>)> {
    let file_path = &span.file;

    if !file_path.as_path().exists() {
        trace!("racer: skipping non-existant file: {:?}", file_path);
        return None;
    }

    let name = vfs.load_line(file_path.as_path(), span.range.row_start).ok().and_then(|line| {
        let col_start = span.range.col_start.0 as usize;
        let col_end = span.range.col_end.0 as usize;
        line.get(col_start..col_end).map(|line| line.to_string())
    });

    debug!("racer: name: {:?}", name);
    
    let results = ::std::panic::catch_unwind(move || {
        let cache = racer::FileCache::new(vfs);
        let session = racer::Session::new(&cache);
        let row = span.range.row_end.one_indexed();
        let coord = racer_coord(row, span.range.col_end);
        let location = racer::Location::Coords(coord);
        trace!("racer: file_path: {:?}, location: {:?}", file_path, location);
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
                trace!("racer: contextstr: {:?}", m.contextstr);
                let contextstr = m.contextstr.trim_right_matches("{").trim();
                match m.mtype {
                    racer::MatchType::Module => {
                        // Ignore
                    },
                    racer::MatchType::Function => {
                        let the_type = format_method(&fmt_config, contextstr.into());
                        if !the_type.is_empty() {
                            ty = Some(the_type.into())
                        }
                    },
                    racer::MatchType::Trait | racer::MatchType::Enum | racer::MatchType::Struct => {
                        let the_type = format_object(&fmt_config, contextstr.into());
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
                trace!("racer: racer_ty: {:?}", ty);
                (m.docs, ty)
            })
            .next()
    });

    let results = results.map_err(|_| {
        error!("racer: racer panicked");
    });

    results.unwrap_or(None)
}

/// Formats a struct, enum, union, or trait. The original type is returned
/// in the event of an error.
fn format_object(fmt_config: &FmtConfig, the_type: String) -> String {
    debug!("format_object: {}", the_type);
    let config = fmt_config.get_rustfmt_config();
    let trimmed = the_type.trim();
    
    // Normalize the ending for rustfmt
    let object = if trimmed.ends_with(")") {
        format!("{};", trimmed)
    } else if trimmed.ends_with("}") {
        trimmed.to_string()
    } else if trimmed.ends_with(";") {
        trimmed.to_string()
    } else if trimmed.ends_with("{") {
        let trimmed = trimmed.trim_right_matches("{").to_string();
        format!("{}{{}}", trimmed)
    } else {
        format!("{}{{}}", trimmed)
    };

    let mut out = Vec::<u8>::with_capacity(the_type.len());
    let formatted = match rustfmt::format_input(FmtInput::Text(object), &config, Some(&mut out)) {
        Ok(_) => {
            let utf8 = String::from_utf8(out);
            match utf8.map(|lines| (lines.rfind("{"), lines)) {
                Ok((Some(pos), lines)) => {
                    lines[0..pos].into()
                },
                Ok((None, lines)) => {
                    lines.into()
                },
                _ => trimmed.into(),
            }
        },
        Err(e) => {
            error!("format_object: error: {:?}", e);
            trimmed.to_string()
        }
    };

    // If it's a tuple, remove the trailing ';' and hide non-pub components for pub types
    let result = if formatted.trim().ends_with(";") {
        let mut decl = formatted.trim().trim_right_matches(";");
        if let (Some(pos), true) = (decl.rfind("("), decl.ends_with(")")) {
            let tuple_parts = decl[pos+1..decl.len()-1].split(",").map(|part| {
                let part = part.trim();
                if decl.starts_with("pub") && !part.starts_with("pub") {
                    "_".to_string()
                } else {
                    part.to_string()
                }
            }).collect::<Vec<String>>().join(", ");
            decl = &decl[0..pos];
            format!("{}({})", decl, tuple_parts)
        } else {
            decl.into()
        }
    } else {
        formatted
    };

    result.trim().into()
}

/// Formats a method or function. The original type is returned
/// in the event of an error.
fn format_method(fmt_config: &FmtConfig, the_type: String) -> String {
    trace!("format_method: {}", the_type);
    let the_type = the_type.trim().trim_right_matches(";").to_string();
    let config = fmt_config.get_rustfmt_config();
    let method = format!("impl Dummy {{ {} {{ unimplmented!() }} }}", the_type);
    let mut out = Vec::<u8>::with_capacity(the_type.len());
    let result = match rustfmt::format_input(FmtInput::Text(method), config, Some(&mut out)) {
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
            error!("format_method: error: {:?}", e);
            the_type
        }
    };

    result.trim().into()
}

/// Builds a hover tooltip composed of the function signature or type decleration, doc URL
/// (if available in the save-analysis), source extracted documentation, and code context 
/// for local variables.
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

    trace!("tooltip: span: {:?}", hover_span);
    trace!("tooltip: span_def: {:?}", hover_span_def);
    trace!("tooltip: span_ty: {:?}", hover_span_ty);

    let doc_url = analysis.doc_url(&hover_span).ok();
    
    let mut contents = vec![];

    let vfs = ctx.vfs.clone();
    let fmt_config = ctx.fmt_config();

    let racer_fallback = |contents: &mut Vec<MarkedString>| {
        if let Some((racer_docs, racer_ty)) = racer(vfs.clone(), ctx.fmt_config(), &hover_span, None) {
            let docs = process_docs(&racer_docs);
            let docs = if docs.trim().is_empty() { hover_span_docs } else { docs };
            let ty = racer_ty.unwrap_or(hover_span_ty);
            let ty = ty.trim();
            if !ty.is_empty() {
                contents.push(MarkedString::from_language_code("rust".into(), ty.into()));
            }
            if !docs.is_empty() {
                contents.push(MarkedString::from_markdown(docs.into()));
            }
        }
    };
    
    if let Ok(def) = hover_span_def {
        if def.kind == DefKind::Local && def.span == hover_span && def.qualname.contains("$") {
            debug!("tooltip: local variable declaration: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), def.value.trim().into()));
        } else if def.kind == DefKind::Local && def.span != hover_span && !def.qualname.contains("$") {
            debug!("tooltip: function argument usage: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), def.value.trim().into()));
        } else if def.kind == DefKind::Local && def.span != hover_span && def.qualname.contains("$") {
            debug!("tooltip: local variable usage: {}", def.name);
            contents.extend(tooltip_local_variable_usage(&vfs, &def));
        } else if def.kind == DefKind::Local && def.span == hover_span {
            debug!("tooltip: function signature argument: {}", def.name);
            contents.push(MarkedString::from_language_code("rust".into(), def.value.trim().into()));
        } else { match def.kind {
            DefKind::TupleVariant | DefKind::StructVariant | DefKind::Field => {
                debug!("tooltip: field or variant: {}", def.name);
                contents.extend(tooltip_field_or_variant(&vfs, &def, doc_url));
            },
            DefKind::Enum | DefKind::Union | DefKind::Struct | DefKind::Trait => {
                debug!("tooltip: struct, enum, union, or trait: {}", def.name);
                contents.extend(tooltip_struct_enum_union_trait(&vfs, &fmt_config, &def, doc_url));
            },
            DefKind::Function | DefKind::Method => {
                debug!("tooltip: function or method: {}", def.name);
                contents.extend(tooltip_function_method(&vfs, &fmt_config, &def, doc_url));
            },
            DefKind::Mod => {
                debug!("tooltip: mod usage: {}", def.name);
                contents.extend(tooltip_mod(&vfs, &def, doc_url));
            },
            DefKind::Static | DefKind::Const => {
                debug!("tooltip: static or const (using racer): {}", def.name);
                racer_fallback(&mut contents);
            },
            _ => {
                debug!("tooltip: ignoring def = {:?}", def);
            }
        }}
    } else {
        debug!("tooltip: def is empty; falling back to racer");
        racer_fallback(&mut contents);
    }

    Ok(contents)
}

#[test]
fn test_process_docs_rust_blocks() {
    let docs = "
Brief one liner.

Lorem ipsum dolor sit amet, consectetur adipiscing elit. Phasellus vitae ex
vel mi egestas semper in non dolor. Proin ut arcu at odio hendrerit consequat.

# Examples

Donec ullamcorper risus quis massa sollicitudin, id faucibus nibh bibendum.

## Hidden code lines and proceeding whitespace is removed and meta attributes are preserved

```
# extern crate foo;

use foo::bar;

#[derive(Debug)]
struct Baz(u32);

let baz = Baz(1);
```

## Rust code block attributes are converted to 'rust'

```compile_fail,E0123
let foo = ;
```

## Inner comments and indentation is preserved

```
/// inner doc comment
fn foobar() {
    // inner comment
    let indent = 1;
}
```
    ".trim();

    let expected = "
Brief one liner.

Lorem ipsum dolor sit amet, consectetur adipiscing elit. Phasellus vitae ex
vel mi egestas semper in non dolor. Proin ut arcu at odio hendrerit consequat.

# Examples

Donec ullamcorper risus quis massa sollicitudin, id faucibus nibh bibendum.

## Hidden code lines and proceeding whitespace is removed and meta attributes are preserved

```rust
use foo::bar;

#[derive(Debug)]
struct Baz(u32);

let baz = Baz(1);
```

## Rust code block attributes are converted to 'rust'

```rust
let foo = ;
```

## Inner comments and indentation is preserved

```rust
/// inner doc comment
fn foobar() {
    // inner comment
    let indent = 1;
}
```
    ".trim();

    let actual = process_docs(docs);
    assert_eq!(expected, actual);
}

#[test]
fn test_process_docs_bash_block() {
    let expected = "
Brief one liner.

```bash
# non rust-block comment lines are preserved
ls -la
```
    ".trim();

    let actual = process_docs(expected);
    assert_eq!(expected, actual);
}

#[test]
fn test_process_docs_racer_noise() {
    let docs = "
////////////////////////////////////////////////////////////////////////////////

Spawns a new thread, returning a [`JoinHandle`] for it.

The join handle will implicitly *detach* the child thread upon being
dropped. In this case, the child thread may outlive the parent (unless
   ".trim();

    let expected = "
Spawns a new thread, returning a [`JoinHandle`] for it.

The join handle will implicitly *detach* the child thread upon being
dropped. In this case, the child thread may outlive the parent (unless
    ".trim();

    let actual = process_docs(docs);
    assert_eq!(expected, actual);
}

#[test]
fn test_format_object() {

    let config = &FmtConfig::default();

    let input = "pub struct Box<T: ?Sized>(Unique<T>);";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Box<T: ?Sized>(_)", &result);

    let input = "pub struct Thing(pub u32);";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing(pub u32)", &result, "tuple struct with trailing ';' from racer");

    let input = "pub struct Thing(pub u32)";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing(pub u32)", &result, "pub tuple struct");

    let input = "pub struct Thing(pub u32, i32)";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing(pub u32, _)", &result, "non-pub components of pub tuples should be hidden");

    let input = "struct Thing(u32, i32)";
    let result = format_object(config, input.into());
    assert_eq!("struct Thing(u32, i32)", &result, "private tuple struct may show private components");

    let input = "pub struct Thing<T: Copy>";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing<T: Copy>", &result, "pub struct");

    let input = "pub struct Thing<T: Copy> {";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing<T: Copy>", &result, "pub struct with trailing '{{' from racer");

    let input = "pub struct Thing { x: i32 }";
    let result = format_object(config, input.into());
    assert_eq!("pub struct Thing", &result, "pub struct with body");

    let input = "pub enum Foobar { Foo, Bar }";
    let result = format_object(config, input.into());
    assert_eq!("pub enum Foobar", &result, "pub enum with body");

    let input = "pub trait Thing<T, U> where T: Copy + Sized, U: Clone";
    let expected = "
pub trait Thing<T, U>
where
    T: Copy + Sized,
    U: Clone,
    ".trim();
    let result = format_object(config, input.into());
    assert_eq!(expected, &result, "trait with where clause");
}


#[test]
fn test_format_method() {

    let config = &FmtConfig::default();

    let input = "fn foo() -> ()";
    let result = format_method(config, input.into());
    assert_eq!(input, &result, "function explicit void return");

    let input = "fn foo()";
    let expected = "fn foo()";
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "function");

    let input = "fn foo() -> Thing";
    let expected = "fn foo() -> Thing";
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "function with return");

    let input = "fn foo(&self);";
    let expected = "fn foo(&self)";
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "method");

    let input = "fn foo<T>(t: T) where T: Copy";
    let expected = "
fn foo<T>(t: T)
where
    T: Copy,
    ".trim();
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "function with generic parameters");

    let input = "fn foo<T>(&self, t: T) where T: Copy";
    let expected = "
fn foo<T>(&self, t: T)
where
    T: Copy,
    ".trim();
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "method with type parameters");

    let input = "   fn foo<T>(
         &self, 
 t: T) 
      where 
T: Copy

";
    let expected = "
fn foo<T>(&self, t: T)
where
    T: Copy,
    ".trim();
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "method with type parameters; corrected spacing");

    let input = "fn really_really_really_really_long_name<T>(foo_thing: String, bar_thing: Thing, baz_thing: Vec<T>, foo_other: u32, bar_other: i32) -> Thing";
    let expected = "
fn really_really_really_really_long_name<T>(
    foo_thing: String,
    bar_thing: Thing,
    baz_thing: Vec<T>,
    foo_other: u32,
    bar_other: i32,
) -> Thing
    ".trim();
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "long function signature");

    let input = "fn really_really_really_really_long_name(&self, foo_thing: String, bar_thing: Thing, baz_thing: Vec<T>, foo_other: u32, bar_other: i32) -> Thing";
    let expected = "
fn really_really_really_really_long_name(
    &self,
    foo_thing: String,
    bar_thing: Thing,
    baz_thing: Vec<T>,
    foo_other: u32,
    bar_other: i32,
) -> Thing
    ".trim();
    let result = format_method(config, input.into());
    assert_eq!(expected, &result, "long method signature with unspecified generic");
}

#[test]
fn test_extract_docs_module() {
    let expected = "
Sample module

Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas
tincidunt tristique maximus. Sed venenatis urna vel sagittis tempus.
In hac habitasse platea dictumst.

# Examples

```rust
let foo = sample::foo();
```
    ".trim();

    let vfs = Vfs::new();
    let file = Path::new("test_data/hover/src/sample.rs");
    let row_start = Row::new_zero_indexed(0);
    let actual = extract_docs(&vfs, file, row_start).expect("module docs");
    assert_eq!(expected, actual, "hover/sample.rs module docs");
}

#[test]
fn test_extract_docs() {
    let expected = "
The `Baz` variant

Aliquam erat volutpat.
    ".trim();

    let vfs = Vfs::new();
    let file = Path::new("test_data/hover/src/sample.rs");
    let row_start = Row::new_zero_indexed(61);
    let actual = extract_docs(&vfs, file, row_start).expect("module docs");
    assert_eq!(expected, actual, "hover/sample.rs module docs");
}

#[test]
fn test_extract_decleration() {
    let vfs = Vfs::new();
    let file = Path::new("test_data/hover/src/sample.rs");

    let expected = "
pub trait Qeh<T, U>
where T: Copy,
U: Clone
    ".trim();
    let row_start = Row::new_zero_indexed(112);
    let actual = extract_decleration(&vfs, file, row_start).expect("trait decleration").join("\n");
    assert_eq!(expected, actual);

    let expected = "
pub fn multiple_lines(
s: String,
i: i32
)
    ".trim();
    let row_start = Row::new_zero_indexed(118);
    let actual = extract_decleration(&vfs, file, row_start).expect("function decleration").join("\n");
    assert_eq!(expected, actual);

    let expected = "fn make_copy(&self) -> Self";
    let row_start = Row::new_zero_indexed(102);
    let actual = extract_decleration(&vfs, file, row_start).expect("method decleration").join("\n");
    assert_eq!(expected, actual);

    let expected = "pub struct NewType(pub u32, f32)";
    let row_start = Row::new_zero_indexed(70);
    let actual = extract_decleration(&vfs, file, row_start).expect("tuple decleration").join("\n");
    assert_eq!(expected, actual);

    let expected = "pub struct Foo<T>";
    let row_start = Row::new_zero_indexed(45);
    let actual = extract_decleration(&vfs, file, row_start).expect("struct decleration").join("\n");
    assert_eq!(expected, actual);
}

#[test]
fn test_tooltip() {
    use config;
    use analysis;
    use lsp_data::{ClientCapabilities, InitializationOptions};
    use ls_types::{TextDocumentPositionParams, TextDocumentIdentifier, Position};
    use server::{Output, RequestId};
    use url::Url;
    use serde_json as json;
    use build::BuildPriority;
    
    use std::env;
    use std::fmt::Debug;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;

    let pid = process::id();
    let cwd = env::current_dir().unwrap();
    let project_dir = cwd.join("test_data").join("hover");
    let client_caps = ClientCapabilities {
        code_completion_has_snippet_support: true,
        related_information_support: true
    };

    let vfs = Arc::new(Vfs::new());
    let mut config = config::Config::default();
    config.infer_defaults(&project_dir).expect("config::infer_defaults failed");
    let config = Arc::new(Mutex::new(config));
    let analysis = Arc::new(analysis::AnalysisHost::new(analysis::Target::Debug));

    let ctx = InitActionContext::new(
        analysis.clone(), 
        vfs.clone(), 
        config.clone(), 
        client_caps, 
        project_dir.clone(), 
        pid, 
        true
    );

    #[derive(Clone, Default)]
    struct Out(Arc<Mutex<u64>>);

    impl Output for Out {
        fn response(&self, output: String) {
            trace!("{}", output);
        }

        fn provide_id(&self) -> RequestId {
            let mut id = self.0.lock().unwrap();
            *id += 1;
            RequestId::Num(*id as u64)
        }
    }
    
    let init_options = InitializationOptions::default();
    let out = Out::default();
    ctx.init(&init_options, &out);
    ctx.build(&project_dir, BuildPriority::Immediate, &out);
    ctx.block_on_build();

    fn str_err<D: Debug>(e: &D) -> String { 
        format!("{:?}", e)
    }

    let target_dir = env::var("CARGO_TARGET_DIR")
        .map(|s| Path::new(&s).to_owned())
        .unwrap_or_else(|_| {
            cwd.join("target")
        })
        .join("debug")
        .join("hover")
        .join("tooltip_test_results");

    fs::create_dir_all(&target_dir).expect("failed to create hover_test_data");

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
    struct Test {
        /// relative to test_data/hover/src
        file: String,
        /// one based
        line: u64,
        /// one based
        col: u64,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct TestResult {
        test: Test,
        data: Result<Vec<MarkedString>, String>,
    }

    impl TestResult {
        fn save(&self, target_dir: &Path) {
            let path = self.test.path(target_dir);
            let data = json::to_string_pretty(&self)
                .expect("failed to serialize hover test result");
            fs::write(path, data)
                .expect("failed to save hover test result");
        }

        fn load(&self, test_data_dir: &Path) -> Result<TestResult, String> {
            let path = self.test.path(test_data_dir);
            let file = fs::File::open(path.clone()).map_err(|e| {
                eprintln!("failed to load hover test result: {:?} ({:?})", path, e);
                str_err(&e)
            })?;
            let result: Result<TestResult, String> = json::from_reader(file).map_err(|e| {
                eprintln!("failed to parse hover test result: {:?} ({:?})", path, e);
                str_err(&e)
            });
            result
        }
    }

    impl Test {
        fn new(file: &str, line: u64, col: u64) -> Test {
            Test { file: file.into(), line, col }
        }

        fn path(&self, target_dir: &Path) -> PathBuf {
            target_dir.join(format!("{}.{:04}_{:03}.json", self.file, self.line, self.col))
        }

        fn run(&self, project_dir: &Path, ctx: &InitActionContext) -> TestResult {
            let url = Url::from_file_path(project_dir.join("src").join(&self.file)).expect(&self.file);
            let doc_id = TextDocumentIdentifier::new(url.clone());
            let position = Position::new(self.line - 1u64, self.col - 1u64);
            let params = TextDocumentPositionParams::new(doc_id, position);
            let result = tooltip(&ctx, &params).map_err(|e| format!("{:?}", e));
            
            TestResult {
                test: self.clone(),
                data: result,
            }
        }
    }

    let tests = vec![
        Test::new("main.rs", 12, 12),
        Test::new("main.rs", 14, 9),
        Test::new("main.rs", 14, 15),
        Test::new("main.rs", 15, 15),
        Test::new("main.rs", 16, 15),
        Test::new("main.rs", 17, 15),
        Test::new("main.rs", 30, 12),
        Test::new("main.rs", 30, 22),
        Test::new("main.rs", 30, 27),
        Test::new("main.rs", 31, 7),
        Test::new("main.rs", 31, 12),
        Test::new("main.rs", 33, 10),
        Test::new("main.rs", 33, 16),
        Test::new("main.rs", 33, 22),
        Test::new("main.rs", 34, 12),
        Test::new("main.rs", 38, 14),
        Test::new("main.rs", 38, 24),
        Test::new("main.rs", 38, 31),
        Test::new("main.rs", 38, 35),
        Test::new("main.rs", 38, 42),
        Test::new("main.rs", 38, 48),
        Test::new("main.rs", 39, 12),
        Test::new("main.rs", 43, 11),
        Test::new("main.rs", 43, 18),
        Test::new("main.rs", 43, 25),
        Test::new("main.rs", 44, 12),
        Test::new("main.rs", 44, 21),
        Test::new("main.rs", 44, 28),
        Test::new("main.rs", 45, 22),
        Test::new("main.rs", 46, 21),
        Test::new("main.rs", 46, 28),
        Test::new("main.rs", 47, 13),
        Test::new("main.rs", 47, 22),
        Test::new("main.rs", 47, 28),
        Test::new("main.rs", 47, 40),
        Test::new("main.rs", 47, 50),
        Test::new("main.rs", 48, 19),
        Test::new("main.rs", 51, 13),
        Test::new("main.rs", 51, 20),
        Test::new("main.rs", 56, 12),
        Test::new("main.rs", 56, 26),

        Test::new("sample.rs", 25, 12),
        Test::new("sample.rs", 25, 17),
        Test::new("sample.rs", 46, 14),
        Test::new("sample.rs", 50, 10),
        Test::new("sample.rs", 62, 6),
        Test::new("sample.rs", 81, 14),
        Test::new("sample.rs", 81, 24),
        Test::new("sample.rs", 88, 14),
        Test::new("sample.rs", 88, 70),
        Test::new("sample.rs", 89, 43),
        Test::new("sample.rs", 90, 53),
        Test::new("sample.rs", 99, 12),
        Test::new("sample.rs", 103, 13),
        Test::new("sample.rs", 107, 13),
        Test::new("sample.rs", 119, 14),
        Test::new("sample.rs", 128, 11),
    ];

    let results: Vec<TestResult> = tests.iter().map(|test| {
        let result = test.run(&project_dir, &ctx);
        result.save(&target_dir);
        result
    })
    .collect();

    let tooltip_test_results_dir = project_dir.join("tooltip_test_results");
    let failed_results: Vec<(&TestResult, Result<TestResult, String>)> = results.iter().map(|result| {
        match result.load(&tooltip_test_results_dir) {
            Ok(saved_result) => {
                if result.data == saved_result.data {
                    None
                } else {
                    Some((result, Ok(saved_result)))
                }
            }
            Err(e) => {
                Some((result, Err(str_err(&e))))
            }
        }
    })
    .filter(|failed_result| failed_result.is_some())
    .map(|failed_result| failed_result.unwrap())
    .collect();

    for failed_result in failed_results.iter() {
        match failed_result {
            (expect_result, Ok(failed_result)) => {
                let project_file = expect_result.test.path(&tooltip_test_results_dir);
                let target_file = expect_result.test.path(&target_dir);
                eprintln!("Expect hover tooltip result ({:?}):", project_file);
                eprintln!("{}\n", json::to_string(&expect_result.data).unwrap());
                eprintln!("Failed hover tooltip result ({:?}):", target_file);
                eprintln!("{}\n", json::to_string(&failed_result.data).unwrap());

                // use difference::Changeset;
                // let expect = json::to_string(&expect_result.data).unwrap();
                // let actual = json::to_string(&failed_result.data).unwrap();
                // let changeset = Changeset::new(&expect, &actual, " ");
                // println!("{}", changeset);

            }
            (expect_result, Err(e)) => {
                let project_file = expect_result.test.path(&tooltip_test_results_dir);
                eprintln!("Failed to load saved tooltip result ({:?}): {:?}", project_file, e);
            }
        }
        eprintln!("\n");
    }

    if !failed_results.is_empty() {
        panic!("{} / {} hover tooltip tests failed", failed_results.len(), tests.len());
    }
}