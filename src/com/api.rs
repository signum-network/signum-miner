use bytes::Bytes;
use serde::de::{self, DeserializeOwned};
use std::fmt;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitNonceRequest<'a> {
    pub request_type: &'a str,
    pub account_id: u64,
    pub nonce: u64,
    pub secret_phrase: Option<&'a String>,
    pub blockheight: u64,
    pub deadline: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetMiningInfoRequest<'a> {
    pub request_type: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitNonceResponse {
    pub deadline: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MiningInfoResponse {
    pub generation_signature: String,

    #[serde(deserialize_with = "from_str_or_int")]
    pub base_target: u64,

    #[serde(deserialize_with = "from_str_or_int")]
    pub height: u64,

    #[serde(
        default = "default_target_deadline",
        deserialize_with = "from_str_or_int"
    )]
    pub target_deadline: u64,
}

fn default_target_deadline() -> u64 {
    std::u64::MAX
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolErrorWrapper {
    error: PoolError,
}

/// Some pools answer with a bare `{"result": "..."}` instead of an error
/// object, e.g. `{"result": "Deadline exceeds maximum allowed deadline"}` for
/// a share whose deadline was too high.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolResultWrapper {
    result: String,
}

/// `PoolError::code` used for a bare `{"result": "..."}` answer.
pub const POOL_RESULT_CODE: i32 = -1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug)]
pub enum FetchError {
    Http(reqwest::Error),
    Pool(PoolError),
}

impl From<reqwest::Error> for FetchError {
    fn from(err: reqwest::Error) -> FetchError {
        FetchError::Http(err)
    }
}

impl From<PoolError> for FetchError {
    fn from(err: PoolError) -> FetchError {
        FetchError::Pool(err)
    }
}

fn from_str_or_int<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct StringOrIntVisitor;

    impl<'de> de::Visitor<'de> for StringOrIntVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or int")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            v.parse::<u64>().map_err(de::Error::custom)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v)
        }
    }

    deserializer.deserialize_any(StringOrIntVisitor)
}

pub fn parse_json_result<T: DeserializeOwned>(body: &Bytes) -> Result<T, PoolError> {
    match serde_json::from_slice(body) {
        Ok(x) => Ok(x),
        _ => match serde_json::from_slice::<PoolErrorWrapper>(body) {
            Ok(x) => Err(x.error),
            // A pool can also answer with neither the expected payload nor an
            // `error` object, but with a bare `{"result": "..."}` - that is
            // how a refused share is reported. Without this branch such an
            // answer would surface as an opaque `code: 0` with the whole JSON
            // as the message.
            Err(_) => match serde_json::from_slice::<PoolResultWrapper>(body) {
                Ok(x) => Err(PoolError {
                    code: POOL_RESULT_CODE,
                    message: x.result,
                }),
                Err(_) => {
                    let v = body.to_vec();
                    Err(PoolError {
                        code: 0,
                        message: String::from_utf8_lossy(&v).to_string(),
                    })
                }
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn body(text: &str) -> Bytes {
        Bytes::from(text.to_owned())
    }

    /// `SubmitNonceResponse` has no `Debug`, and the tests need the error more
    /// than they need `expect_err`.
    fn error_from(text: &str) -> PoolError {
        match parse_json_result::<SubmitNonceResponse>(&body(text)) {
            Ok(_) => panic!("expected an error, got a success: {}", text),
            Err(err) => err,
        }
    }

    #[test]
    fn parses_the_expected_payload() {
        let parsed: SubmitNonceResponse =
            parse_json_result(&body(r#"{"deadline": 12345}"#)).expect("should parse");
        assert_eq!(parsed.deadline, 12345);
    }

    #[test]
    fn parses_an_error_object() {
        let err = error_from(
            r#"{"error": {"code": 2, "message": "submission is for different round"}}"#,
        );
        assert_eq!(err.code, 2);
        assert_eq!(err.message, "submission is for different round");
    }

    #[test]
    fn parses_a_bare_result_as_a_refused_share() {
        // What a pool answers to a share whose deadline is too high: HTTP 200
        // with neither a `deadline` nor an `error` object. Before this answer
        // was recognised it surfaced as `code: 0` with the whole JSON as the
        // message.
        let err = error_from(r#"{"result": "Deadline exceeds maximum allowed deadline"}"#);
        assert_eq!(err.code, POOL_RESULT_CODE);
        assert_eq!(err.message, "Deadline exceeds maximum allowed deadline");
    }

    #[test]
    fn falls_back_to_the_raw_body() {
        let err = error_from("not json at all");
        assert_eq!(err.code, 0);
        assert_eq!(err.message, "not json at all");
    }
}
