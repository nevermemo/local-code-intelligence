//! A checker for the subset of JSON Schema 2020-12 that the RunRecord
//! schema uses. No JSON Schema validator is in the dependency tree, so the
//! spec's fallback applies (run-record.md, section 5): a hand-written
//! checker. It refuses, when the schema is loaded, any keyword, `$ref` or
//! `pattern` it does not implement, so the schema cannot grow a rule this
//! checker silently ignores.

use serde_json::Value;

/// Keywords the checker implements or deliberately ignores (annotations).
const KEYWORDS: [&str; 17] = [
    "$schema",
    "$id",
    "description",
    "$defs",
    "oneOf",
    "$ref",
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "pattern",
    "minimum",
    "maxLength",
    "format",
];

/// The patterns the schema uses, each with a hand-written matcher.
fn pattern_matcher(pattern: &str) -> Option<fn(&str) -> bool> {
    Some(match pattern {
        "^(file|chunk|sym|commit|test|cfg|doc|str):" => |value| {
            [
                "file", "chunk", "sym", "commit", "test", "cfg", "doc", "str",
            ]
            .iter()
            .any(|kind| {
                value
                    .strip_prefix(kind)
                    .is_some_and(|rest| rest.starts_with(':'))
            })
        },
        "^[a-z][a-z0-9_]*(\\.[a-z0-9_]+)*$" => |value| {
            let word = |segment: &str| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            };
            value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && value.split('.').all(word)
        },
        "^[0-9a-f]{16,64}$" => |value| {
            (16..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        },
        "^[0-9A-HJKMNP-TV-Z]{26}$" => {
            |value| value.len() == 26 && value.bytes().all(|b| super::CROCKFORD.contains(&b))
        }
        _ => return None,
    })
}

/// RFC 3339 `date-time`: `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`.
fn is_date_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range.clone())
            .is_some_and(|part| part.len() == range.len() && part.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, wanted: &[u8]| bytes.get(index).is_some_and(|b| wanted.contains(b));
    if !(digits(0..4)
        && at(4, b"-")
        && digits(5..7)
        && at(7, b"-")
        && digits(8..10)
        && at(10, b"Tt")
        && digits(11..13)
        && at(13, b":")
        && digits(14..16)
        && at(16, b":")
        && digits(17..19))
    {
        return false;
    }
    let number = |range: std::ops::Range<usize>| value[range].parse::<u32>().unwrap_or(99);
    if !(1..=12).contains(&number(5..7))
        || !(1..=31).contains(&number(8..10))
        || number(11..13) > 23
        || number(14..16) > 59
        || number(17..19) > 60
    {
        return false;
    }
    let mut rest = &value[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return false;
        }
        rest = &fraction[len..];
    }
    match rest.as_bytes() {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

/// A loaded schema, checked for keywords, references and patterns.
#[derive(Debug)]
pub struct Schema {
    root: Value,
}

impl Schema {
    pub fn parse(text: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let schema = Self { root };
        schema.audit(&schema.root, "#")?;
        Ok(schema)
    }

    /// The `enum` of definition `name`, for tests that tie a Rust
    /// vocabulary to the schema.
    pub fn enum_of(&self, name: &str) -> Option<Vec<&str>> {
        self.root["$defs"][name]["enum"]
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect()
    }

    fn resolve(&self, reference: &str) -> Option<&Value> {
        let name = reference.strip_prefix("#/$defs/")?;
        self.root.get("$defs")?.get(name)
    }

    fn audit(&self, schema: &Value, at: &str) -> Result<(), String> {
        let object = schema
            .as_object()
            .ok_or_else(|| format!("{at}: a schema must be an object"))?;
        for (keyword, value) in object {
            let here = format!("{at}/{keyword}");
            if !KEYWORDS.contains(&keyword.as_str()) {
                return Err(format!("{here}: keyword not implemented by the checker"));
            }
            match keyword.as_str() {
                "$defs" | "properties" => {
                    for (name, inner) in value
                        .as_object()
                        .ok_or_else(|| format!("{here}: must be an object"))?
                    {
                        self.audit(inner, &format!("{here}/{name}"))?;
                    }
                }
                "oneOf" => {
                    for (index, inner) in value
                        .as_array()
                        .ok_or_else(|| format!("{here}: must be an array"))?
                        .iter()
                        .enumerate()
                    {
                        self.audit(inner, &format!("{here}/{index}"))?;
                    }
                }
                "items" => self.audit(value, &here)?,
                "additionalProperties" if !value.is_boolean() => self.audit(value, &here)?,
                "$ref" => {
                    let reference = value.as_str().unwrap_or_default();
                    if self.resolve(reference).is_none() {
                        return Err(format!("{here}: unresolved reference {value}"));
                    }
                }
                "pattern" => {
                    let pattern = value.as_str().unwrap_or_default();
                    if pattern_matcher(pattern).is_none() {
                        return Err(format!("{here}: pattern {value} not implemented"));
                    }
                }
                "format" if value != "date-time" => {
                    return Err(format!("{here}: format {value} not implemented"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks one instance against the root schema; the error names the
    /// first violation and where it is.
    pub fn validate(&self, instance: &Value) -> Result<(), String> {
        self.check(&self.root, instance, "$")
    }

    fn check(&self, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
        let Some(schema) = schema.as_object() else {
            return Ok(());
        };
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let target = self.resolve(reference).expect("audited");
            self.check(target, value, at)?;
        }
        if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
            let errors: Vec<String> = branches
                .iter()
                .filter_map(|branch| self.check(branch, value, at).err())
                .collect();
            let matched = branches.len() - errors.len();
            if matched != 1 {
                return Err(format!(
                    "{at}: matches {matched} of the oneOf branches, expected 1 ({})",
                    errors.join(" | ")
                ));
            }
        }
        if let Some(types) = schema.get("type") {
            let allowed: Vec<&str> = match types {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => vec![],
            };
            if !allowed.iter().any(|name| has_type(value, name)) {
                return Err(format!("{at}: {value} is not of type {types}"));
            }
        }
        if let Some(options) = schema.get("enum").and_then(Value::as_array)
            && !options.contains(value)
        {
            return Err(format!("{at}: {value} is not one of {options:?}"));
        }
        if let Some(constant) = schema.get("const")
            && constant != value
        {
            return Err(format!("{at}: {value} is not {constant}"));
        }
        match value {
            Value::String(text) => {
                if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                    let matches = pattern_matcher(pattern).expect("audited");
                    if !matches(text) {
                        return Err(format!("{at}: {text:?} does not match {pattern}"));
                    }
                }
                if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                    && text.chars().count() as u64 > max
                {
                    return Err(format!("{at}: longer than {max} characters"));
                }
                if schema.get("format").is_some() && !is_date_time(text) {
                    return Err(format!("{at}: {text:?} is not an RFC 3339 date-time"));
                }
            }
            Value::Number(number) => {
                if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
                    && number.as_f64().is_some_and(|n| n < minimum)
                {
                    return Err(format!("{at}: {number} is below {minimum}"));
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = schema.get("items") {
                    for (index, item) in items.iter().enumerate() {
                        self.check(item_schema, item, &format!("{at}[{index}]"))?;
                    }
                }
            }
            Value::Object(fields) => {
                if let Some(required) = schema.get("required").and_then(Value::as_array) {
                    for name in required.iter().filter_map(Value::as_str) {
                        if !fields.contains_key(name) {
                            return Err(format!("{at}: missing required {name:?}"));
                        }
                    }
                }
                let properties = schema.get("properties").and_then(Value::as_object);
                for (name, field) in fields {
                    let here = format!("{at}.{name}");
                    match (
                        properties.and_then(|p| p.get(name)),
                        schema.get("additionalProperties"),
                    ) {
                        (Some(property), _) => self.check(property, field, &here)?,
                        (None, Some(Value::Bool(false))) => {
                            return Err(format!("{here}: property not allowed"));
                        }
                        (None, Some(additional)) => self.check(additional, field, &here)?,
                        (None, None) => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(text: Value) -> Result<Schema, String> {
        Schema::parse(&text.to_string())
    }

    #[test]
    fn unknown_keywords_patterns_and_references_are_refused_at_load() {
        assert!(schema(json!({ "minItems": 1 })).is_err());
        assert!(schema(json!({ "pattern": "^x+$" })).is_err());
        assert!(schema(json!({ "$ref": "#/$defs/missing" })).is_err());
        assert!(schema(json!({ "format": "uri" })).is_err());
        assert!(schema(json!({ "properties": { "a": { "uniqueItems": true } } })).is_err());
    }

    #[test]
    fn each_keyword_rejects_what_it_should() {
        let s = schema(json!({
            "$defs": { "hex": { "type": "string", "pattern": "^[0-9a-f]{16,64}$" } },
            "type": "object",
            "required": ["id", "n"],
            "properties": {
                "id": { "$ref": "#/$defs/hex" },
                "n": { "type": "integer", "minimum": 1 },
                "t": { "type": "string", "format": "date-time" },
                "k": { "enum": ["a", "b"] },
                "c": { "const": "x" },
                "d": { "type": "string", "maxLength": 3 },
                "xs": { "type": "array", "items": { "type": ["number", "null"] } }
            },
            "additionalProperties": false
        }))
        .unwrap();
        let good = json!({
            "id": "0123456789abcdef", "n": 1, "t": "2026-10-12T13:30:00.5Z",
            "k": "a", "c": "x", "d": "abc", "xs": [1.5, null]
        });
        s.validate(&good).unwrap();
        for (field, bad) in [
            ("id", json!("0123456789ABCDEF")),
            ("id", json!("0123")),
            ("n", json!(0)),
            ("n", json!(1.5)),
            ("t", json!("2026-13-12T13:30:00Z")),
            ("t", json!("2026-10-12 13:30:00Z")),
            ("k", json!("z")),
            ("c", json!("y")),
            ("d", json!("abcd")),
            ("xs", json!(["1"])),
            ("extra", json!(1)),
        ] {
            let mut instance = good.clone();
            instance[field] = bad.clone();
            assert!(s.validate(&instance).is_err(), "{field} = {bad} accepted");
        }
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("n");
        assert!(s.validate(&missing).is_err());
    }

    #[test]
    fn one_of_needs_exactly_one_branch() {
        let s =
            schema(json!({ "oneOf": [ { "type": "number" }, { "type": "integer" } ] })).unwrap();
        s.validate(&json!(1.5)).unwrap();
        assert!(s.validate(&json!(2)).is_err(), "both branches match");
        assert!(s.validate(&json!("2")).is_err(), "no branch matches");
    }

    #[test]
    fn the_hand_written_patterns_follow_their_regular_expressions() {
        let node = pattern_matcher("^(file|chunk|sym|commit|test|cfg|doc|str):").unwrap();
        assert!(node("chunk:src/a.rs@ab#0") && node("str:x"));
        assert!(!node("chunks:x") && !node("Chunk:x") && !node("chunk"));
        let map = pattern_matcher("^[a-z][a-z0-9_]*(\\.[a-z0-9_]+)*$").unwrap();
        assert!(map("lexical") && map("vec.chunk.code") && map("prior.source_role") && map("a.1"));
        assert!(!map("") && !map("1a") && !map("a.") && !map("a..b") && !map("A") && !map("a-b"));
        let run = pattern_matcher("^[0-9A-HJKMNP-TV-Z]{26}$").unwrap();
        assert!(run("01JA0000000000000000000000"));
        assert!(!run("01JA000000000000000000000I") && !run("01JA00000000000000000000"));
    }
}
