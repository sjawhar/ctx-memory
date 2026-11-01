//! Validation of typed record frontmatter against tree-owned JSON Schemas.

use std::ffi::OsStr;
use std::fs::{read_dir, read_to_string};
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use serde::Serialize;

use crate::tree::{yaml_options, Tree};

/// A JSON Schema validation error for one record frontmatter field.
#[derive(Debug, Serialize)]
pub struct SchemaViolation {
    pub path: String,
    pub pointer: String,
    pub message: String,
}

/// One schema the tree owns: the record `type` it governs, its path in the tree, and its text.
#[derive(Debug, Serialize)]
pub struct SchemaFile {
    pub kind: String,
    pub path: String,
    pub text: String,
}

/// Every schema the tree owns, in file-name order.
///
/// `ctx rules` prints these after the rules page: a writer that cannot see them has to guess
/// which fields its record type requires, and the server refuses the guess.
///
/// # Errors
///
/// Returns an error when the schema directory or one of its files cannot be read.
pub fn files(tree: &Tree) -> Result<Vec<SchemaFile>> {
    let mut schemas = Vec::new();
    for (kind, path, raw) in read_schemas(tree)? {
        schemas.push(SchemaFile {
            kind,
            path: path
                .strip_prefix(&tree.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
            text: raw,
        });
    }
    Ok(schemas)
}

/// Each schema file of the tree, in file-name order: the record `type` its stem names, its
/// path, and its text. A tree with no `schemas/` directory has none.
fn read_schemas(tree: &Tree) -> Result<Vec<(String, PathBuf, String)>> {
    let schemas_directory = tree.root.join("schemas");
    let entries = match read_dir(&schemas_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading schemas in {}", schemas_directory.display()));
        }
    };
    let mut entries = entries
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("reading schemas in {}", schemas_directory.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut schemas = Vec::new();
    for entry in entries {
        let schema_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading schema metadata for {}", schema_path.display()))?;
        if !file_type.is_file()
            || schema_path
                .extension()
                .is_none_or(|extension| extension != "yaml")
        {
            continue;
        }
        let Some(kind) = schema_path.file_stem().and_then(OsStr::to_str) else {
            bail!(
                "schema filename is not valid UTF-8: {}",
                schema_path.display()
            );
        };
        let kind = kind.to_owned();
        let raw = read_to_string(&schema_path)
            .with_context(|| format!("reading record schema {}", schema_path.display()))?;
        schemas.push((kind, schema_path, raw));
    }
    Ok(schemas)
}

/// Validates each record against a YAML-authored JSON Schema for its record type.
///
/// Schema files are direct children of `schemas/` at the tree root. Their file stem selects
/// the matching frontmatter `type`.
///
/// # Errors
///
/// Returns an error when a schema directory cannot be read, a schema cannot be parsed or
/// compiled.
pub fn validate(tree: &Tree) -> Result<Vec<SchemaViolation>> {
    let mut violations = Vec::new();
    for (kind, schema_path, raw) in read_schemas(tree)? {
        let schema: serde_json::Value =
            serde_saphyr::from_str_with_options(&raw, yaml_options())
                .with_context(|| format!("parsing record schema {}", schema_path.display()))?;
        let validator = jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&schema)
            .with_context(|| format!("compiling record schema {}", schema_path.display()))?;
        for doc in tree
            .docs
            .iter()
            .filter(|doc| doc.front.kind.as_deref() == Some(kind.as_str()))
        {
            let instance = doc.frontmatter_json()?;
            for error in validator.iter_errors(&instance) {
                violations.push(SchemaViolation {
                    path: doc.rel.to_string_lossy().into_owned(),
                    pointer: error.instance_path().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(violations)
}
