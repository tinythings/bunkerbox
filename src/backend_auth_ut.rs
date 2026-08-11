use super::*;

#[test]
fn resolve_template_substitutes_all_placeholders() {
    let result = resolve_template(
        "{host}/path?callback={callback_url}&custom={custom_token}&refresh={refresh_token}",
        "https://example.com",
        "http://127.0.0.1:1234/callback",
        "CT123",
        "RT456",
    );
    assert_eq!(result, "https://example.com/path?callback=http://127.0.0.1:1234/callback&custom=CT123&refresh=RT456");
}

#[test]
fn resolve_template_ignores_unknown() {
    let result = resolve_template("{host}/foo?{unknown}", "h", "", "", "");
    assert_eq!(result, "h/foo?{unknown}");
}

#[test]
fn parse_query_param_present() {
    assert_eq!(parse_query_param("a=1&b=2&c=3", "b"), Some("2".into()));
}

#[test]
fn parse_query_param_first() {
    assert_eq!(parse_query_param("key=value&other=x", "key"), Some("value".into()));
}

#[test]
fn parse_query_param_last() {
    assert_eq!(parse_query_param("a=b&c=d", "c"), Some("d".into()));
}

#[test]
fn parse_query_param_missing() {
    assert_eq!(parse_query_param("a=1&b=2", "c"), None);
}

#[test]
fn parse_query_param_empty_query() {
    assert_eq!(parse_query_param("", "x"), None);
}

#[test]
fn url_decode_plain_text() {
    assert_eq!(url_decode("hello"), "hello");
}

#[test]
fn url_decode_spaces() {
    assert_eq!(url_decode("hello+world"), "hello world");
}

#[test]
fn url_decode_percent_encoded() {
    assert_eq!(url_decode("hello%20world"), "hello world");
    assert_eq!(url_decode("%3D%3D"), "==");
    assert_eq!(url_decode("a%2Fb%2Fc"), "a/b/c");
}

#[test]
fn url_decode_mixed() {
    assert_eq!(url_decode("name+with%20spaces"), "name with spaces");
}

#[test]
fn extract_string_value() {
    let v = serde_json::json!({"key": "val"});
    assert_eq!(extract_string(&v, "key").unwrap(), "val");
}

#[test]
fn extract_string_number() {
    let v = serde_json::json!({"key": 42});
    assert_eq!(extract_string(&v, "key").unwrap(), "42");
}

#[test]
fn extract_string_missing() {
    let v = serde_json::json!({});
    assert!(extract_string(&v, "key").is_err());
}

#[test]
fn extract_string_empty() {
    let v = serde_json::json!({"key": ""});
    assert!(extract_string(&v, "key").is_err());
}

#[test]
fn extract_u64_value() {
    let v = serde_json::json!({"key": 3600});
    assert_eq!(extract_u64(&v, "key").unwrap(), 3600);
}

#[test]
fn extract_u64_string() {
    let v = serde_json::json!({"key": "7200"});
    assert_eq!(extract_u64(&v, "key").unwrap(), 7200);
}

#[test]
fn extract_u64_missing() {
    let v = serde_json::json!({});
    assert!(extract_u64(&v, "key").is_err());
}

#[test]
fn dot_get_simple() {
    let v = serde_json::json!({"a": {"b": "c"}});
    assert_eq!(dot_get(&v, "a"), &serde_json::json!({"b": "c"}));
    assert_eq!(dot_get(&v, "a.b"), &serde_json::json!("c"));
}

#[test]
fn dot_get_missing() {
    let v = serde_json::json!({"a": 1});
    assert_eq!(dot_get(&v, "x"), &serde_json::Value::Null);
    assert_eq!(dot_get(&v, "a.b.c"), &serde_json::Value::Null);
}

#[test]
fn random_string_length() {
    let s = random_string(32);
    assert_eq!(s.len(), 32);
    assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn sha256_known_vector() {
    let hash = sha256(b"hello");
    assert_eq!(hash.len(), 32);
    let hex = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
}

#[test]
fn base64_url_no_pad_known() {
    let encoded = base64_url_no_pad(b"hello");
    assert_eq!(encoded, "aGVsbG8");
    assert!(!encoded.contains('='));
}
