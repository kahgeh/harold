use std::ffi::OsString;
use std::fmt;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:50060";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    InvalidEncoding,
    InvalidEndpoint,
    MissingEndpoint,
    UnknownArgument,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidEncoding => "arguments must be valid Unicode",
            Self::InvalidEndpoint => "the Harold endpoint is not a valid URI",
            Self::MissingEndpoint => "--endpoint requires a URI",
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
    let mut endpoint = DEFAULT_ENDPOINT.to_owned();

    if let Some(argument) = args.next() {
        if argument != "--endpoint" {
            return Err(CliError::UnknownArgument);
        }
        endpoint = args
            .next()
            .ok_or(CliError::MissingEndpoint)?
            .into_string()
            .map_err(|_| CliError::InvalidEncoding)?;
    }

    if args.next().is_some() {
        return Err(CliError::UnknownArgument);
    }

    let parsed = tonic::transport::Endpoint::from_shared(endpoint.clone())
        .map_err(|_| CliError::InvalidEndpoint)?;
    if parsed.uri().scheme_str() != Some("http") || parsed.uri().authority().is_none() {
        return Err(CliError::InvalidEndpoint);
    }

    Ok(Options { endpoint })
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn defaults_to_loopback_harold() {
        assert_eq!(
            parse_args(["tmx-agent-dash"]).unwrap().endpoint,
            "http://127.0.0.1:50060"
        );
    }

    #[test]
    fn accepts_one_explicit_endpoint() {
        assert_eq!(
            parse_args(["tmx-agent-dash", "--endpoint", "http://127.0.0.1:6000",])
                .unwrap()
                .endpoint,
            "http://127.0.0.1:6000"
        );
    }

    #[test]
    fn rejects_missing_or_unknown_arguments() {
        assert!(parse_args(["tmx-agent-dash", "--endpoint"]).is_err());
        assert!(parse_args(["tmx-agent-dash", "--wat"]).is_err());
    }

    #[test]
    fn rejects_invalid_endpoint_syntax() {
        assert!(parse_args(["tmx-agent-dash", "--endpoint", "://bad"]).is_err());
    }
}
