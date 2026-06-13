//! The advertised `ToolSpec` of every built-in must be a well-formed JSON Schema
//! object the providers can forward 1:1, and the names must satisfy the shared
//! provider name constraint.

use serde_json::Value;
use stepper_tools::{ToolRegistry, ToolSpec};

#[test]
fn every_builtin_input_schema_is_a_typed_object() {
    let reg = ToolRegistry::builtins();
    let specs = reg.specs();
    assert_eq!(specs.len(), 9);

    for spec in &specs {
        let schema = &spec.input_schema;
        assert!(
            schema.is_object(),
            "{} input_schema is not a JSON object",
            spec.name
        );
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{} input_schema.type must be \"object\"",
            spec.name
        );
        assert!(
            schema.get("properties").map(Value::is_object).unwrap_or(false),
            "{} input_schema.properties must be an object",
            spec.name
        );
        assert!(
            !spec.description.is_empty(),
            "{} must carry a description",
            spec.name
        );
    }
}

#[test]
fn every_builtin_name_matches_the_provider_constraint() {
    let reg = ToolRegistry::builtins();
    for spec in reg.specs() {
        assert!(
            ToolSpec::name_is_valid(&spec.name),
            "{} is not a valid provider tool name",
            spec.name
        );
    }
}

#[test]
fn required_fields_reference_declared_properties() {
    let reg = ToolRegistry::builtins();
    for spec in reg.specs() {
        let Some(required) = spec.input_schema.get("required").and_then(Value::as_array) else {
            continue;
        };
        let props = spec
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        for field in required {
            let key = field.as_str().unwrap();
            assert!(
                props.contains_key(key),
                "{} requires `{key}` but does not declare it",
                spec.name
            );
        }
    }
}

#[test]
fn read_tools_are_flagged_read_only_and_mutating_tools_are_not() {
    let reg = ToolRegistry::builtins();
    let read_only: Vec<String> = reg
        .specs()
        .into_iter()
        .filter(|s| s.read_only)
        .map(|s| s.name)
        .collect();
    assert!(read_only.contains(&"read_file".to_string()));
    assert!(read_only.contains(&"grep".to_string()));
    assert!(read_only.contains(&"glob".to_string()));
    assert!(read_only.contains(&"list_dir".to_string()));

    let write = reg.get("write_file").unwrap();
    assert!(!write.read_only());
    let bash = reg.get("bash").unwrap();
    assert!(!bash.read_only());
}
