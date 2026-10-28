//! The conditional-write forms used by the cache client. Unsupported conditions must never
//! be downgraded to an unconditional overwrite. Evaluation remains under Store's publication
//! lock; parsing alone does not perform a racy read/check/write sequence.

use hyper::{HeaderMap, header};

use crate::error::Error;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Condition {
    Any,
    None,
    Match(String),
}

pub(super) fn parse(headers: &HeaderMap) -> Result<Condition, Error> {
    if headers.contains_key(header::IF_UNMODIFIED_SINCE) {
        return Err(Error::malformed("conditional list writes require If-Match, not a date"));
    }
    let single = |name: header::HeaderName| -> Result<Option<String>, Error> {
        if headers.get_all(&name).iter().count() > 1 {
            return Err(Error::malformed(format!("{name} must occur once")));
        }
        headers
            .get(&name)
            .map(|value| {
                value
                    .to_str()
                    .map(|text| text.trim_matches([' ', '\t']).to_owned())
                    .map_err(|_| Error::malformed(format!("{name} is not an ASCII condition")))
            })
            .transpose()
    };
    let matching = single(header::IF_MATCH)?;
    let absent = single(header::IF_NONE_MATCH)?;
    if matching.is_some() && absent.is_some() {
        return Err(Error::malformed("use either If-Match or If-None-Match for a list write"));
    }
    if let Some(absent) = absent {
        return if absent == "*" {
            Ok(Condition::None)
        } else {
            Err(Error::malformed("list writes support only If-None-Match: *"))
        };
    }
    let Some(matching) = matching else { return Ok(Condition::Any) };
    let tag = matching
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .filter(|text| text.bytes().all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte)))
        .ok_or_else(|| Error::malformed("If-Match must be one quoted strong entity tag"))?;
    Ok(Condition::Match(tag.to_owned()))
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;

    use super::*;

    #[test]
    fn supported_conditions_remain_explicit() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse(&headers).unwrap(), Condition::Any);
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static(" * "));
        assert_eq!(parse(&headers).unwrap(), Condition::None);
        headers.clear();
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"a-tag\""));
        assert_eq!(parse(&headers).unwrap(), Condition::Match("a-tag".to_owned()));
        // A comma inside one quoted tag is data, not a second condition.
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"a,b\""));
        assert_eq!(parse(&headers).unwrap(), Condition::Match("a,b".to_owned()));
    }

    #[test]
    fn unsupported_and_malformed_conditions_are_not_unconditional() {
        for (name, value) in [
            (header::IF_NONE_MATCH, "\"existing\""),
            (header::IF_NONE_MATCH, "W/\"existing\""),
            (header::IF_MATCH, "*"),
            (header::IF_MATCH, "W/\"existing\""),
            (header::IF_MATCH, "unquoted"),
            (header::IF_MATCH, "\"one\", \"two\""),
            (header::IF_MATCH, "\"unterminated"),
            (header::IF_UNMODIFIED_SINCE, "Tue, 01 Jan 2030 00:00:00 GMT"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_str(value).unwrap());
            assert!(parse(&headers).is_err(), "{value} became an unconditional write");
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MATCH, HeaderValue::from_bytes(&[0xff]).unwrap());
        assert!(parse(&headers).is_err());
        headers.clear();
        headers.append(header::IF_MATCH, HeaderValue::from_static("\"one\""));
        headers.append(header::IF_MATCH, HeaderValue::from_static("\"two\""));
        assert!(parse(&headers).is_err());
        headers.clear();
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"one\""));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(parse(&headers).is_err());
    }
}
