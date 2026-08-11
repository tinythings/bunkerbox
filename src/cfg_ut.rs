use super::*;

fn deser_auth_backend(yaml: &str) -> AuthBackendConfig {
    serde_yaml::from_str(yaml).expect("deserialize AuthBackendConfig")
}

fn deser_auth_flow(yaml: &str) -> AuthFlow {
    serde_yaml::from_str(yaml).expect("deserialize AuthFlow")
}

fn deser_auth_ref(yaml: &str) -> AuthRef {
    serde_yaml::from_str(yaml).expect("deserialize AuthRef")
}

#[test]
fn auth_ref_inline_declarative() {
    let yaml = r#"
host: https://api.example.com
auth:
  type: env
  variable: API_KEY
request_headers:
  x-foo: bar
"#;
    let a = deser_auth_backend(yaml);
    match a {
        AuthBackendConfig::Declarative(d) => {
            assert_eq!(d.host, "https://api.example.com");
            assert!(matches!(d.auth, AuthFlow::Env { ref variable } if variable == "API_KEY"));
            assert_eq!(d.request_headers.get("x-foo").unwrap(), "bar");
        }
        _ => panic!("expected declarative"),
    }
}

#[test]
fn auth_backend_plugin() {
    let yaml = r#"
name: example
config:
  host: https://example.com
"#;
    let a = deser_auth_backend(yaml);
    match a {
        AuthBackendConfig::Plugin(p) => {
            assert_eq!(p.name, "example");
        }
        _ => panic!("expected plugin"),
    }
}

#[test]
fn auth_flow_env() {
    let yaml = r#"
type: env
variable: SECRET_KEY
"#;
    let f = deser_auth_flow(yaml);
    assert!(matches!(f, AuthFlow::Env { ref variable } if variable == "SECRET_KEY"));
}

#[test]
fn auth_flow_oauth2() {
    let yaml = r#"
type: oauth2
authorize_url: "{host}/auth"
token_url: "{host}/token"
client_id: abc123
response_field: access_token
"#;
    let f = deser_auth_flow(yaml);
    assert!(matches!(f, AuthFlow::OAuth2 { ref client_id, .. } if client_id == "abc123"));
}

#[test]
fn auth_flow_custom_token_exchange() {
    let yaml = r#"
type: custom-token-exchange
login_url: "{host}/app/user/custom-token"
exchange_url: "{host}/auth/sign-in/custom-token"
exchange_body_template: '{"customToken":"{custom_token}"}'
refresh_token_field: refreshToken
refresh_url: "{host}/auth/token/refresh"
refresh_body_template: '{"refreshToken":"{refresh_token}"}'
id_token_field: idToken
"#;
    let f = deser_auth_flow(yaml);
    assert!(matches!(f, AuthFlow::CustomTokenExchange { ref id_token_field, .. } if id_token_field == "idToken"));
}

#[test]
fn auth_flow_missing_type_field() {
    let yaml = r#"
variable: X
"#;
    let result = serde_yaml::from_str::<AuthFlow>(yaml);
    assert!(result.is_err());
}

#[test]
fn auth_declarative_without_headers() {
    let yaml = r#"
host: https://api.example.com
auth:
  type: env
  variable: K
"#;
    let a = deser_auth_backend(yaml);
    match a {
        AuthBackendConfig::Declarative(d) => {
            assert!(d.request_headers.is_empty());
        }
        _ => panic!("expected declarative"),
    }
}

#[test]
fn auth_declarative_oath2_with_scopes() {
    let yaml = r#"
host: https://example.com
auth:
  type: oauth2
  authorize_url: "{host}/authorize"
  token_url: "{host}/token"
  client_id: cid
  scopes:
    - read
    - write
  response_field: access_token
"#;
    let a = deser_auth_backend(yaml);
    match a {
        AuthBackendConfig::Declarative(d) => match d.auth {
            AuthFlow::OAuth2 { ref scopes, .. } => {
                assert_eq!(scopes, &vec!["read".to_string(), "write".to_string()]);
            }
            _ => panic!("expected oauth2"),
        },
        _ => panic!("expected declarative"),
    }
}

#[test]
fn auth_declarative_serializes_roundtrip() {
    let yaml = r#"
host: https://example.com
auth:
  type: env
  variable: K
request_headers:
  x-k: v
"#;
    let a: AuthBackendConfig = serde_yaml::from_str(yaml).unwrap();
    let json = serde_json::to_string(&a).unwrap();
    let roundtripped: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped["host"], "https://example.com");
    assert_eq!(roundtripped["auth"]["type"], "env");
    assert_eq!(roundtripped["auth"]["variable"], "K");
    assert_eq!(roundtripped["request_headers"]["x-k"], "v");
}
