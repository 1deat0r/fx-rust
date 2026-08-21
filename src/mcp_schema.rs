//! JSON-Schema validation for MCP tool arguments, mirroring upstream fx's
//! `json_schema_resolver.zig` behavior at the level that matters on the wire:
//! `$ref` resolution (local pointers + named fragments), type checks,
//! required/properties/additionalProperties, array items, numeric ranges,
//! string lengths/patterns, enums, consts, and anyOf/oneOf/allOf/not.
//!
//! Errors carry JSON-pointer-ish paths (`/properties/foo`), so a failed
//! validation becomes a precise tool error returned to the model rather than
//! an opaque server-side rejection.

use serde_json::Value;

use crate::mcp_transport::MAX_TOOL_SCHEMA_DEPTH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// JSON-pointer-ish path into the instance, e.g. `/properties/foo`.
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.path, self.message)
    }
}

/// Validate `instance` against `schema`. Returns `Ok(())` or a list of
/// validation errors (all failures found, not just the first).
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut depth = 0usize;
    check(schema, instance, "$", &mut errors, &mut depth, None);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn push(errors: &mut Vec<ValidationError>, path: &str, message: impl Into<String>) {
    errors.push(ValidationError {
        path: path.to_string(),
        message: message.into(),
    });
}

fn check(
    schema: &Value,
    instance: &Value,
    path: &str,
    errors: &mut Vec<ValidationError>,
    depth: &mut usize,
    root: Option<&Value>,
) {
    *depth += 1;
    if *depth > MAX_TOOL_SCHEMA_DEPTH {
        push(errors, path, "schema nesting exceeds depth limit");
        *depth -= 1;
        return;
    }

    // The document root is the first schema node we entered from (allows local
    // `#/...` refs to resolve against the whole tool schema).
    let doc_root = root.unwrap_or(schema);

    // $ref resolution (local only; remote refs are reported, not followed).
    if let Some(reference) = schema.get("$ref").and_then(|r| r.as_str()) {
        match resolve_ref(doc_root, reference) {
            Some(target) => {
                check(target, instance, path, errors, depth, Some(doc_root));
                *depth -= 1;
                return;
            }
            None => {
                push(errors, path, format!("cannot resolve $ref `{reference}`"));
                *depth -= 1;
                return;
            }
        }
    }

    if let Some(schema_value) = schema.get("const") {
        if instance != schema_value {
            push(errors, path, format!("must equal const {}", schema_value));
        }
    }
    if let Some(enum_vals) = schema.get("enum").and_then(|e| e.as_array()) {
        if !enum_vals.iter().any(|v| v == instance) {
            push(errors, path, "value is not one of the allowed enum values");
        }
    }

    if let Some(expected) = schema.get("type") {
        if let Some(t) = expected.as_str() {
            if !matches_type(t, instance) {
                push(errors, path, format!("must be of type `{t}`"));
            }
        } else if let Some(types) = expected.as_array() {
            let types: Vec<&str> = types.iter().filter_map(|t| t.as_str()).collect();
            if !types.iter().any(|t| matches_type(t, instance)) {
                push(
                    errors,
                    path,
                    format!("must be one of types [{}]", types.join(", ")),
                );
            }
        }
    }

    match instance {
        Value::Object(map) => {
            // Required properties.
            if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
                for name in required {
                    if let Some(name) = name.as_str() {
                        if !map.contains_key(name) {
                            push(errors, path, format!("missing required property `{name}`"));
                        }
                    }
                }
            }
            if let Some(min) = schema.get("minProperties").and_then(|v| v.as_u64()) {
                if (map.len() as u64) < min {
                    push(errors, path, format!("must have at least {min} properties"));
                }
            }
            if let Some(max) = schema.get("maxProperties").and_then(|v| v.as_u64()) {
                if (map.len() as u64) > max {
                    push(errors, path, format!("must have at most {max} properties"));
                }
            }
            let props_schema = schema.get("properties").and_then(|p| p.as_object());
            let pattern_props = schema.get("patternProperties").and_then(|p| p.as_object());
            let additional = schema.get("additionalProperties");
            for (key, value) in map {
                let mut matched = false;
                if let Some(props) = props_schema {
                    if let Some(prop_schema) = props.get(key) {
                        matched = true;
                        let child_path = format!("{path}/{key}");
                        check(
                            prop_schema,
                            value,
                            &child_path,
                            errors,
                            depth,
                            Some(doc_root),
                        );
                    }
                }
                if let Some(pp) = pattern_props {
                    for (pkey, pschema) in pp {
                        let Ok(re) = regex::Regex::new(pkey) else {
                            continue;
                        };
                        if re.is_match(key) {
                            matched = true;
                            let child_path = format!("{path}/{key}");
                            check(pschema, value, &child_path, errors, depth, Some(doc_root));
                        }
                    }
                }
                if !matched {
                    match additional {
                        Some(Value::Bool(false)) => push(
                            errors,
                            path,
                            format!("additional property `{key}` is not allowed"),
                        ),
                        Some(Value::Object(additional_schema)) => {
                            let child_path = format!("{path}/{key}");
                            check(
                                &Value::Object(additional_schema.clone()),
                                value,
                                &child_path,
                                errors,
                                depth,
                                Some(doc_root),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        Value::Array(items) => {
            let mut item_schemas: Option<Vec<Value>> = None;
            if let Some(schema) = schema.get("items") {
                if schema.is_array() {
                    item_schemas = Some(schema.as_array().cloned().unwrap_or_default());
                } else if schema.is_object() {
                    item_schemas = Some(vec![schema.clone()]);
                }
            }
            let prefix = schema.get("prefixItems").and_then(|p| p.as_array());
            if let Some(min) = schema.get("minItems").and_then(|v| v.as_u64()) {
                if (items.len() as u64) < min {
                    push(errors, path, format!("must have at least {min} items"));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(|v| v.as_u64()) {
                if (items.len() as u64) > max {
                    push(errors, path, format!("must have at most {max} items"));
                }
            }
            if let Some(true) = schema.get("uniqueItems").and_then(|v| v.as_bool()) {
                for i in 0..items.len() {
                    for j in (i + 1)..items.len() {
                        if items[i] == items[j] {
                            push(errors, path, "array items must be unique");
                            break;
                        }
                    }
                }
            }
            for (idx, item) in items.iter().enumerate() {
                let child_path = format!("{path}/{idx}");
                if let Some(schemas) = &item_schemas {
                    if schemas.len() == 1 {
                        // Tuple form: prefixItems defines the leading item schemas.
                        let child_schema = if let Some(prefix) = prefix {
                            if idx < prefix.len() {
                                prefix[idx].clone()
                            } else {
                                schemas[0].clone()
                            }
                        } else {
                            schemas[0].clone()
                        };
                        check(
                            &child_schema,
                            item,
                            &child_path,
                            errors,
                            depth,
                            Some(doc_root),
                        );
                    } else {
                        // Array form: each item against its own schema (2020-12 `items`).
                        if idx < schemas.len() {
                            check(
                                &schemas[idx],
                                item,
                                &child_path,
                                errors,
                                depth,
                                Some(doc_root),
                            );
                        }
                    }
                } else if let Some(prefix) = prefix {
                    if idx < prefix.len() {
                        let child_schema = prefix[idx].clone();
                        check(
                            &child_schema,
                            item,
                            &child_path,
                            errors,
                            depth,
                            Some(doc_root),
                        );
                    } else if let Some(additional_items) = schema.get("items") {
                        check(
                            additional_items,
                            item,
                            &child_path,
                            errors,
                            depth,
                            Some(doc_root),
                        );
                    }
                }
            }
        }
        _ => {
            // Scalars: numeric and string constraints.
            if let Some(num) = instance.as_f64() {
                if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
                    if num < min {
                        push(errors, path, format!("must be >= {min}"));
                    }
                }
                if let Some(min) = schema.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
                    if num <= min {
                        push(errors, path, format!("must be > {min}"));
                    }
                }
                if let Some(max) = schema.get("maximum").and_then(|v| v.as_f64()) {
                    if num > max {
                        push(errors, path, format!("must be <= {max}"));
                    }
                }
                if let Some(max) = schema.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
                    if num >= max {
                        push(errors, path, format!("must be < {max}"));
                    }
                }
                if let Some(multiple) = schema.get("multipleOf").and_then(|v| v.as_f64()) {
                    if multiple > 0.0 {
                        let ratio = num / multiple;
                        if (ratio - ratio.round()).abs() > 1e-9 {
                            push(errors, path, format!("must be a multiple of {multiple}"));
                        }
                    }
                }
            }
            if let Some(s) = instance.as_str() {
                let len = s.chars().count() as u64;
                if let Some(min) = schema.get("minLength").and_then(|v| v.as_u64()) {
                    if len < min {
                        push(errors, path, format!("must be at least {min} characters"));
                    }
                }
                if let Some(max) = schema.get("maxLength").and_then(|v| v.as_u64()) {
                    if len > max {
                        push(errors, path, format!("must be at most {max} characters"));
                    }
                }
                if let Some(pattern) = schema.get("pattern").and_then(|v| v.as_str()) {
                    match regex::Regex::new(pattern) {
                        Ok(re) => {
                            if !re.is_match(s) {
                                push(errors, path, format!("must match pattern `{pattern}`"));
                            }
                        }
                        Err(e) => push(errors, path, format!("schema pattern invalid: {e}")),
                    }
                }
            }
            if instance.is_null() {
                // `null` type is covered by the type check; nothing else applies.
            }
        }
    }

    // Composite keywords.
    if let Some(all) = schema.get("allOf").and_then(|v| v.as_array()) {
        let mut sub_errors: Vec<ValidationError> = Vec::new();
        let mut sub_depth = *depth;
        for subschema in all {
            check(
                subschema,
                instance,
                path,
                &mut sub_errors,
                &mut sub_depth,
                Some(doc_root),
            );
        }
        errors.extend(sub_errors);
        *depth = sub_depth;
    }
    if let Some(any) = schema.get("anyOf").and_then(|v| v.as_array()) {
        let mut passed = false;
        let mut all_errors: Vec<ValidationError> = Vec::new();
        for subschema in any {
            let mut sub_errors: Vec<ValidationError> = Vec::new();
            let mut sub_depth = *depth;
            check(
                subschema,
                instance,
                path,
                &mut sub_errors,
                &mut sub_depth,
                Some(doc_root),
            );
            if sub_errors.is_empty() {
                passed = true;
                break;
            }
            all_errors.extend(sub_errors);
        }
        if !passed {
            push(errors, path, "must match at least one schema in anyOf");
            for e in all_errors.into_iter().take(3) {
                push(errors, &format!("{path}/anyOf"), format!("({e})"));
            }
        }
    }
    if let Some(one) = schema.get("oneOf").and_then(|v| v.as_array()) {
        let mut passes = 0usize;
        for subschema in one {
            let mut sub_errors: Vec<ValidationError> = Vec::new();
            let mut sub_depth = *depth;
            check(
                subschema,
                instance,
                path,
                &mut sub_errors,
                &mut sub_depth,
                Some(doc_root),
            );
            if sub_errors.is_empty() {
                passes += 1;
            }
        }
        if passes != 1 {
            push(
                errors,
                path,
                format!("must match exactly one schema in oneOf (matched {passes})"),
            );
        }
    }
    if let Some(not) = schema.get("not") {
        let mut sub_errors: Vec<ValidationError> = Vec::new();
        let mut sub_depth = *depth;
        check(
            not,
            instance,
            path,
            &mut sub_errors,
            &mut sub_depth,
            Some(doc_root),
        );
        if sub_errors.is_empty() {
            push(errors, path, "must NOT match the `not` schema");
        }
    }

    *depth -= 1;
}

fn matches_type(t: &str, instance: &Value) -> bool {
    match t {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        // `integer` matches integral numbers (JSON Schema semantics).
        "integer" => instance
            .as_f64()
            .map(|n| n.fract() == 0.0 && n.is_finite())
            .unwrap_or(false),
        _ => true,
    }
}

/// Resolve a `$ref` (URI fragment) against the schema document containing it.
/// Supports `#`, `#/pointer`, and `#/$defs/name` / `#/definitions/name`.
/// External (non-fragment) refs are not fetched; we return None.
fn resolve_ref<'a>(schema: &'a Value, reference: &str) -> Option<&'a Value> {
    if reference.is_empty() {
        return Some(schema);
    }
    let fragment = reference.split('#').next_back().unwrap_or(reference);
    if fragment.starts_with("http://") || fragment.starts_with("https://") {
        // External document: not supported without a fetcher.
        return None;
    }
    if reference.contains('#') && !reference.starts_with('#') {
        // `path/to/file#/frag` external document reference.
        return None;
    }
    if fragment.is_empty() {
        return Some(schema);
    }
    let mut current = schema;
    for token in fragment.trim_start_matches('/').split('/') {
        let token = unescape_pointer(token);
        {
            let next = current.get(token)?;
            current = next
        }
    }
    Some(current)
}

fn unescape_pointer(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_basic_types() {
        let schema = json!({
            "type": "object",
            "required": ["name", "count"],
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer", "minimum": 0 },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        });
        assert!(validate(&schema, &json!({"name": "x", "count": 2, "tags": ["a"]})).is_ok());
        let err = validate(&schema, &json!({"name": "x", "count": -1, "tags": [1]}));
        assert!(err.is_err());
        let errors = err.unwrap_err();
        let joined = errors.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert!(joined
            .iter()
            .any(|e| e.contains("/count") && e.contains(">= 0")));
        assert!(joined
            .iter()
            .any(|e| e.contains("/tags/0") && e.contains("string")));
    }

    #[test]
    fn required_and_additional_properties() {
        let schema = json!({
            "type": "object",
            "required": ["a"],
            "properties": { "a": { "type": "string" } },
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"a": "x"})).is_ok());
        let errors = validate(&schema, &json!({"b": 1})).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.message.contains("missing required property `a`")));
        let errors2 = validate(&schema, &json!({"a": "x", "b": 1})).unwrap_err();
        assert!(errors2.iter().any(|e| e.message.contains("not allowed")));
    }

    #[test]
    fn resolves_local_refs() {
        let schema = json!({
            "$defs": {
                "positive": { "type": "integer", "minimum": 1 }
            },
            "type": "object",
            "properties": {
                "count": { "$ref": "#/$defs/positive" }
            }
        });
        assert!(validate(&schema, &json!({"count": 2})).is_ok());
        assert!(validate(&schema, &json!({"count": 0})).is_err());
        assert!(validate(&schema, &json!({"count": "x"})).is_err());
    }

    #[test]
    fn enum_and_const() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["fast", "safe"] },
                "level": { "const": 3 }
            }
        });
        assert!(validate(&schema, &json!({"mode": "fast", "level": 3})).is_ok());
        assert!(validate(&schema, &json!({"mode": "turbo"})).is_err());
        assert!(validate(&schema, &json!({"level": 4})).is_err());
    }

    #[test]
    fn string_constraints() {
        let schema = json!({
            "type": "string",
            "minLength": 2,
            "maxLength": 5,
            "pattern": "^[a-z]+$"
        });
        assert!(validate(&schema, &json!("abc")).is_ok());
        assert!(validate(&schema, &json!("a")).is_err());
        assert!(validate(&schema, &json!("abcdefgh")).is_err());
        assert!(validate(&schema, &json!("ABC")).is_err());
    }

    #[test]
    fn any_of_requires_one_branch() {
        let schema = json!({
            "anyOf": [
                { "type": "string", "pattern": "^\\d+$" },
                { "type": "integer" }
            ]
        });
        assert!(validate(&schema, &json!("123")).is_ok());
        assert!(validate(&schema, &json!(42)).is_ok());
        assert!(validate(&schema, &json!("abc")).is_err());
    }
}
