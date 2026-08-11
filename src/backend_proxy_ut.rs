use super::*;

#[test]
fn translates_only_agent_enabled_models() {
    let body = br#"{"default":"enabled","models":[{"id":"enabled","name":"Enabled","capabilities":["chat","agent"]},{"id":"disabled","capabilities":["agent"],"isAgentEnabled":false},{"id":"chat-only","capabilities":["chat"]},{"id":42,"capabilities":["agent"]}]}"#;

    let models: serde_json::Value = serde_json::from_slice(&translate_models(body)).unwrap();
    assert_eq!(models["object"], "list");
    assert_eq!(models["default"], "enabled");
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "enabled");
}

#[test]
fn ignores_a_default_model_that_cannot_be_used() {
    let body = br#"{"default":"disabled","models":[{"id":"disabled","capabilities":["agent"],"isAgentEnabled":false}]}"#;

    let models: serde_json::Value = serde_json::from_slice(&translate_models(body)).unwrap();
    assert!(models.get("default").is_none());
    assert!(models["data"].as_array().unwrap().is_empty());
}

#[test]
fn translates_max_tokens_without_changing_model() {
    let body = web::Bytes::from_static(br#"{"model":"model-id","max_tokens":123}"#);

    let request: serde_json::Value = serde_json::from_slice(&normalize_chat_request(body)).unwrap();
    assert_eq!(request["model"], "model-id");
    assert_eq!(request["max_completion_tokens"], 123);
    assert!(request.get("max_tokens").is_none());
}
