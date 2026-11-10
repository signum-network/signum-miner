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

    #[serde(deserialize_with = "from_str_or_nonzero_int")]
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

/// Like `from_str_or_int`, but refuses 0. The miner divides every deadline by
/// the base target, so a pool answering `baseTarget: 0` would otherwise crash
/// it with a division by zero. Refusing it here makes such an answer an
/// unusable mining info like any other.
fn from_str_or_nonzero_int<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: de::Deserializer<'de>,
{
    match from_str_or_int(deserializer)? {
        0 => Err(de::Error::custom("must not be 0")),
        v => Ok(v),
    }
}

#[cfg(test)]
mod mining_info_tests {
    use super::*;

    fn parse(json: &str) -> Result<MiningInfoResponse, PoolError> {
        parse_json_result(&Bytes::from(json.to_owned()))
    }

    #[test]
    fn a_zero_base_target_is_refused() {
        let gensig = "ab".repeat(32);
        for base_target in ["0", "\"0\""] {
            let json = format!(
                r#"{{"generationSignature": "{}", "baseTarget": {}, "height": 1}}"#,
                gensig, base_target
            );
            assert!(parse(&json).is_err(), "baseTarget {} accepted", base_target);
        }

        let json = format!(
            r#"{{"generationSignature": "{}", "baseTarget": "18325193", "height": 1}}"#,
            gensig
        );
        assert_eq!(parse(&json).ok().map(|m| m.base_target), Some(18325193));
    }
}

pub fn parse_json_result<T: DeserializeOwned>(body: &Bytes) -> Result<T, PoolError> {
    match serde_json::from_slice(body) {
        Ok(x) => Ok(x),
        _ => match serde_json::from_slice::<PoolErrorWrapper>(body) {
            Ok(x) => Err(x.error),
            _ => {
                let v = body.to_vec();
                Err(PoolError {
                    code: 0,
                    message: String::from_utf8_lossy(&v).to_string(),
                })
            }
        },
    }
}
