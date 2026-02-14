use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use parsing::decl::DataType;

use super::error::CompileError;
use super::CompileResult;

/// Convert a snake_case string to CamelCase for use as a Datalog variable name.
/// E.g., "target_ip" → "TargetIp", "dc" → "Dc".
pub fn to_datalog_var(s: &str) -> String {
    s.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut result = first.to_uppercase().to_string();
                    result.extend(chars);
                    result
                }
            }
        })
        .collect()
}

/// Generate Datalog text representation of the compiled program.
pub fn emit_datalog(result: &CompileResult) -> String {
    let mut out = String::new();

    // String table (for reference).
    if !result.string_table.id_to_str.is_empty() {
        writeln!(out, "// String table:").unwrap();
        for (id, s) in result.string_table.id_to_str.iter().enumerate() {
            writeln!(out, "// {} = \"{}\"", id, s).unwrap();
        }
        writeln!(out).unwrap();
    }

    // EDB declarations.
    if !result.program.edbs().is_empty() {
        writeln!(out, ".in").unwrap();
        for decl in result.program.edbs() {
            writeln!(out, ".decl {}", decl).unwrap();
        }
    }

    // EDB facts (as comments showing the logical content).
    // Use declared attribute types to decide how to display values.
    for (rel_name, facts) in &result.edb_facts {
        // Find the EDB declaration to get attribute types.
        let decl = result.program.edbs().iter().find(|d| d.name() == rel_name);
        writeln!(out).unwrap();
        for tuple in facts {
            let vals: Vec<String> = tuple
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let attr_type = decl
                        .and_then(|d| d.attributes().get(i))
                        .map(|a| *a.data_type());
                    match attr_type {
                        Some(DataType::String) => {
                            if let Some(s) = result.string_table.decode(*v) {
                                format!("\"{}\"", s)
                            } else {
                                v.to_string()
                            }
                        }
                        Some(DataType::Float) => {
                            let f = f64::from_bits(*v as u64);
                            format!("{}", f)
                        }
                        _ => v.to_string(),
                    }
                })
                .collect();
            writeln!(out, "// {}({}).", rel_name, vals.join(", ")).unwrap();
        }
    }

    // IDB declarations.
    if !result.program.idbs().is_empty() {
        writeln!(out).unwrap();
        writeln!(out, ".printsize").unwrap();
        for decl in result.program.idbs() {
            writeln!(out, ".decl {}", decl).unwrap();
        }
    }

    // Rules.
    if !result.program.rules().is_empty() {
        writeln!(out).unwrap();
        for rule in result.program.rules() {
            writeln!(out, "{}", rule).unwrap();
        }
    }

    out
}

/// Write EDB facts to `.facts` files in the given directory.
/// Each file is named `{relation_name}.facts` with tab-separated i64 values.
pub fn write_facts(
    edb_facts: &HashMap<String, Vec<Vec<i64>>>,
    output_dir: &Path,
) -> Result<(), CompileError> {
    fs::create_dir_all(output_dir)?;

    for (rel_name, facts) in edb_facts {
        let path = output_dir.join(format!("{}.facts", rel_name));
        let mut content = String::new();
        for tuple in facts {
            let line: Vec<String> = tuple.iter().map(|v| v.to_string()).collect();
            writeln!(content, "{}", line.join("\t")).unwrap();
        }
        fs::write(&path, content)?;
    }

    Ok(())
}
