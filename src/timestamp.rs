use jiff::{Timestamp, ZonedDateTime, tz::TimeZone};
use serde::{Serialize, Serializer};

pub fn local(timestamp: Timestamp) -> ZonedDateTime {
    timestamp.to_zoned_datetime(TimeZone::system())
}

pub fn rfc3339(timestamp: Timestamp) -> String {
    local(timestamp)
        .strftime("%Y-%m-%dT%H:%M:%S%.f%:z")
        .to_string()
}

// Status JSON predates Jiff and must remain RFC 3339 without a zone annotation.
pub fn serialize<S: Serializer>(timestamp: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&rfc3339(*timestamp))
}

pub fn serialize_optional<S: Serializer>(
    timestamp: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    timestamp.map(rfc3339).serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_serialization_preserves_the_instant_without_zone_annotations() {
        for input in ["2026-01-01T00:00:00Z", "2026-07-01T00:00:00.123456789Z"] {
            let timestamp: Timestamp = input.parse().unwrap();
            let encoded = rfc3339(timestamp);
            assert!(!encoded.contains('['));
            assert_eq!(encoded.parse::<Timestamp>().unwrap(), timestamp);
        }
    }

    #[test]
    fn required_serialization_preserves_rfc3339() {
        #[derive(Serialize)]
        struct Sample {
            #[serde(serialize_with = "serialize")]
            at: Timestamp,
        }
        let at: Timestamp = "2026-07-01T00:00:00.123456789Z".parse().unwrap();
        let value = serde_json::to_value(Sample { at }).unwrap();
        let encoded = value["at"].as_str().unwrap();
        assert!(!encoded.contains('['));
        assert_eq!(encoded.parse::<Timestamp>().unwrap(), at);
    }

    #[test]
    fn optional_serialization_preserves_null() {
        #[derive(Serialize)]
        struct Sample {
            #[serde(serialize_with = "serialize_optional")]
            at: Option<Timestamp>,
        }
        assert_eq!(
            serde_json::to_string(&Sample { at: None }).unwrap(),
            r#"{"at":null}"#
        );
        let timestamp: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let value = serde_json::to_value(Sample {
            at: Some(timestamp),
        })
        .unwrap();
        assert_eq!(
            value["at"].as_str().unwrap().parse::<Timestamp>().unwrap(),
            timestamp
        );
    }
}
