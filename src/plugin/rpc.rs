//! JSON-RPC 2.0, one message per line, and the error codes of the source
//! plugin protocol.

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::api::{find, HttpError, SignInNeeded};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;
pub const AUTH_REQUIRED: i64 = -32001;
pub const NOT_FOUND: i64 = -32002;
pub const UNAVAILABLE: i64 = -32003;
pub const RATE_LIMITED: i64 = -32004;
pub const NETWORK: i64 = -32005;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

pub type RpcResult = Result<Value, RpcError>;

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, message)
    }

    fn to_json(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            e["data"] = data.clone();
        }
        e
    }
}

/// TIDAL `subStatus` values of a 401 that is about the item, not the
/// token: not ready for playback, not in this country, not with this
/// subscription.
const UNPLAYABLE_SUB_STATUS: [i64; 5] = [4005, 4010, 4032, 4033, 4035];

/// Map API and transport failures onto the protocol codes.
impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        let message = format!("{e:#}");
        if find::<SignInNeeded>(&e).is_some() {
            return Self::new(AUTH_REQUIRED, message);
        }
        if let Some(r) = find::<Unavailable>(&e) {
            return Self::new(UNAVAILABLE, r.0.clone());
        }
        if let Some(h) = find::<HttpError>(&e) {
            let code = match h.status {
                401 if h.sub_status.is_some_and(|s| UNPLAYABLE_SUB_STATUS.contains(&s)) => UNAVAILABLE,
                401 => AUTH_REQUIRED,
                403 => UNAVAILABLE,
                404 => NOT_FOUND,
                429 => RATE_LIMITED,
                500..=599 => NETWORK,
                _ => INTERNAL,
            };
            let mut err = Self::new(code, message);
            if code == RATE_LIMITED {
                err.data = Some(json!({"retry_after": h.retry_after.unwrap_or(30).min(3600)}));
            }
            return err;
        }
        if e.chain().any(|c| c.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_timeout() || r.is_connect() || r.is_request())) {
            return Self::new(NETWORK, message);
        }
        Self::new(INTERNAL, message)
    }
}

/// TIDAL will not stream this track (in a format the output plays): the
/// reason, for the host.
#[derive(Debug, Clone)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

/// One line read from the host.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    /// Answer to a request of ours (the plugin sends none).
    Response { id: Value, result: RpcResult },
    /// Unparseable line, or not a JSON-RPC message: answer with this error.
    Invalid { id: Value, error: RpcError },
}

impl PartialEq for RpcError {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}

pub fn parse(line: &str) -> Incoming {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Incoming::Invalid { id: Value::Null, error: RpcError::new(PARSE_ERROR, e.to_string()) },
    };
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match (v.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Incoming::Request { id, method: method.to_string(), params },
        (Some(method), None) => Incoming::Notification { method: method.to_string(), params },
        (None, Some(id)) if v.get("error").is_some() => {
            let e = &v["error"];
            let error = RpcError {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL),
                message: e.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                data: e.get("data").cloned(),
            };
            Incoming::Response { id, result: Err(error) }
        }
        (None, Some(id)) if v.get("result").is_some() => Incoming::Response { id, result: Ok(v["result"].clone()) },
        (None, id) => Incoming::Invalid {
            id: id.unwrap_or(Value::Null),
            error: RpcError::new(INVALID_REQUEST, "not a JSON-RPC 2.0 message"),
        },
    }
}

/// Serialised writes to stdout: every task sends whole messages here.
#[derive(Clone)]
pub struct Out {
    tx: mpsc::UnboundedSender<Value>,
}

impl Out {
    /// Start the stdout writer. It stops once every `Out` is dropped.
    pub fn stdout() -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let task = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                let mut line = msg.to_string();
                line.push('\n');
                if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                    break;
                }
            }
        });
        (Self { tx }, task)
    }

    pub fn respond(&self, id: Value, result: RpcResult) {
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
        };
        let _ = self.tx.send(msg);
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_classified() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":3,"method":"search","params":{"query":"x"}}"#),
            Incoming::Request { id: json!(3), method: "search".into(), params: json!({"query": "x"}) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"output.changed","params":{}}"#),
            Incoming::Notification { method: "output.changed".into(), params: json!({}) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"a","result":null}"#),
            Incoming::Response { id: json!("a"), result: Ok(Value::Null) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32602,"message":"ms"}}"#),
            Incoming::Response { id: json!("a"), result: Err(RpcError::new(INVALID_PARAMS, "ms")) }
        );
        assert!(matches!(parse("{oops"), Incoming::Invalid { error: RpcError { code: PARSE_ERROR, .. }, .. }));
        assert!(matches!(parse(r#"{"id":1}"#), Incoming::Invalid { error: RpcError { code: INVALID_REQUEST, .. }, .. }));
    }

    #[test]
    fn http_errors_map_to_protocol_codes() {
        let http = |status, sub_status, retry_after| {
            anyhow::Error::from(HttpError { status, retry_after, sub_status, message: String::new() })
        };
        assert_eq!(RpcError::from(http(401, Some(11003), None)).code, AUTH_REQUIRED);
        assert_eq!(RpcError::from(http(401, Some(4005), None)).code, UNAVAILABLE, "about the track");
        assert_eq!(RpcError::from(http(404, None, None).context("albums/1")).code, NOT_FOUND);
        let limited = RpcError::from(http(429, None, Some(12)));
        assert_eq!(limited.code, RATE_LIMITED);
        assert_eq!(limited.data, Some(json!({"retry_after": 12})));
        assert_eq!(RpcError::from(http(429, None, None)).data, Some(json!({"retry_after": 30})));
        assert_eq!(RpcError::from(http(503, None, None)).code, NETWORK);
        assert_eq!(RpcError::from(anyhow::anyhow!("other")).code, INTERNAL);
        let refused = anyhow::Error::from(HttpError { status: 400, retry_after: None, sub_status: None, message: "invalid_grant".into() })
            .context(SignInNeeded("sign in again"));
        assert_eq!(RpcError::from(refused).code, AUTH_REQUIRED);
        let e = RpcError::from(anyhow::Error::from(Unavailable("encrypted stream".into())).context("track 7"));
        assert_eq!((e.code, e.message.as_str()), (UNAVAILABLE, "encrypted stream"));
    }
}
