use std::ffi::OsString;
use std::fmt;

use crate::text::HostMarker;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:50060";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub endpoint: String,
    pub host_marker: HostMarker,
    pub remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    InvalidEncoding,
    InvalidEndpoint,
    InvalidHostMarker,
    MissingEndpoint,
    MissingHostMarker,
    UnknownArgument,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidEncoding => "arguments must be valid Unicode",
            Self::InvalidEndpoint => "the Harold endpoint is not a valid URI",
            Self::InvalidHostMarker => "--host-marker takes glyph, emoji or text",
            Self::MissingEndpoint => "--endpoint requires a URI",
            Self::MissingHostMarker => "--host-marker requires glyph, emoji or text",
            Self::UnknownArgument => "unknown or repeated argument",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CliError {}

pub fn parse_args<I, S>(args: I) -> Result<Options, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    let mut endpoint = None;
    let mut host_marker = None;
    let mut remote = false;

    while let Some(argument) = args.next() {
        if argument == "--remote" && !remote {
            remote = true;
        } else if argument == "--endpoint" && endpoint.is_none() {
            endpoint = Some(
                args.next()
                    .ok_or(CliError::MissingEndpoint)?
                    .into_string()
                    .map_err(|_| CliError::InvalidEncoding)?,
            );
        } else if argument == "--host-marker" && host_marker.is_none() {
            let value = args
                .next()
                .ok_or(CliError::MissingHostMarker)?
                .into_string()
                .map_err(|_| CliError::InvalidEncoding)?;
            host_marker = Some(
                value
                    .parse::<HostMarker>()
                    .map_err(|()| CliError::InvalidHostMarker)?,
            );
        } else {
            return Err(CliError::UnknownArgument);
        }
    }
    let endpoint = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());

    let parsed = tonic::transport::Endpoint::from_shared(endpoint.clone())
        .map_err(|_| CliError::InvalidEndpoint)?;
    if parsed.uri().scheme_str() != Some("http") || parsed.uri().authority().is_none() {
        return Err(CliError::InvalidEndpoint);
    }

    Ok(Options {
        endpoint,
        host_marker: host_marker.unwrap_or_default(),
        remote,
    })
}

#[cfg(test)]
mod tests {
    use super::{CliError, parse_args};
    use crate::text::HostMarker;

    #[test]
    fn defaults_to_loopback_harold() {
        assert_eq!(
            parse_args(["control-tower"]).unwrap().endpoint,
            "http://127.0.0.1:50060"
        );
    }

    #[test]
    fn accepts_one_explicit_endpoint() {
        assert_eq!(
            parse_args(["control-tower", "--endpoint", "http://127.0.0.1:6000",])
                .unwrap()
                .endpoint,
            "http://127.0.0.1:6000"
        );
    }

    #[test]
    fn rejects_missing_or_unknown_arguments() {
        assert!(parse_args(["control-tower", "--endpoint"]).is_err());
        assert!(parse_args(["control-tower", "--wat"]).is_err());
    }

    #[test]
    fn rejects_invalid_endpoint_syntax() {
        assert!(parse_args(["control-tower", "--endpoint", "://bad"]).is_err());
    }

    #[test]
    fn host_marker_parses_each_name_and_defaults_to_glyph() {
        assert_eq!(
            parse_args(["control-tower"]).unwrap().host_marker,
            HostMarker::Glyph
        );
        for (name, marker) in [
            ("glyph", HostMarker::Glyph),
            ("emoji", HostMarker::Emoji),
            ("text", HostMarker::Text),
        ] {
            let options = parse_args(["control-tower", "--host-marker", name]).unwrap();
            assert_eq!(options.host_marker, marker);
            assert_eq!(options.endpoint, "http://127.0.0.1:50060");
        }
    }

    #[test]
    fn host_marker_takes_either_order_beside_the_endpoint() {
        let options = parse_args([
            "control-tower",
            "--endpoint",
            "http://127.0.0.1:9",
            "--host-marker",
            "text",
        ])
        .unwrap();
        assert_eq!(options.host_marker, HostMarker::Text);
        assert_eq!(options.endpoint, "http://127.0.0.1:9");
        let options = parse_args([
            "control-tower",
            "--host-marker",
            "emoji",
            "--endpoint",
            "http://127.0.0.1:9",
        ])
        .unwrap();
        assert_eq!(options.host_marker, HostMarker::Emoji);
    }

    #[test]
    fn a_bad_host_marker_is_a_usage_error() {
        assert_eq!(
            parse_args(["control-tower", "--host-marker", "icons"]),
            Err(CliError::InvalidHostMarker)
        );
        assert_eq!(
            parse_args(["control-tower", "--host-marker"]),
            Err(CliError::MissingHostMarker)
        );
        assert_eq!(
            parse_args([
                "control-tower",
                "--host-marker",
                "text",
                "--host-marker",
                "text"
            ]),
            Err(CliError::UnknownArgument)
        );
    }

    #[test]
    fn remote_takes_no_value_and_defaults_to_off() {
        assert!(!parse_args(["control-tower"]).unwrap().remote);
        let options = parse_args(["control-tower", "--remote"]).unwrap();
        assert!(options.remote);
        assert_eq!(options.endpoint, "http://127.0.0.1:50060");
    }

    #[test]
    fn remote_mixes_with_the_other_flags_in_any_order() {
        let options = parse_args([
            "control-tower",
            "--endpoint",
            "http://127.0.0.1:9",
            "--remote",
            "--host-marker",
            "text",
        ])
        .unwrap();
        assert!(options.remote);
        assert_eq!(options.endpoint, "http://127.0.0.1:9");
        assert_eq!(options.host_marker, HostMarker::Text);
        let options = parse_args([
            "control-tower",
            "--remote",
            "--host-marker",
            "emoji",
            "--endpoint",
            "http://127.0.0.1:9",
        ])
        .unwrap();
        assert!(options.remote);
        assert_eq!(options.host_marker, HostMarker::Emoji);
    }

    #[test]
    fn a_repeated_remote_is_an_error() {
        assert_eq!(
            parse_args(["control-tower", "--remote", "--remote"]),
            Err(CliError::UnknownArgument)
        );
    }
}
