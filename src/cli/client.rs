use clap::Parser;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use socks5_impl::protocol::Address;
use std::{net::SocketAddr, path::PathBuf};
use url::Url;
use uuid::Uuid;

#[derive(Parser, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[command(version, author, name = "anytls-client", about = "AnyTLS rust client")]
pub struct ClientArgs {
    /// AnyTLS URI in the format anytls://[auth@]hostname[:port]/?[key=value]&[key=value]...#fragment
    #[arg(short = 'u', long, value_name = "URL")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Local address to listen for incoming SOCKS5 and HTTP connections
    #[arg(short = 'l', long, value_name = "IP:PORT", default_value = "127.0.0.1:1080")]
    pub listen: SocketAddr,

    /// Server address
    #[arg(short = 's', long, value_name = "IP:PORT")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<Address>,

    /// Password for anytls server authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// Client UUID for panel-managed access
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "UUID")]
    pub client_id: Option<Uuid>,

    /// Root CA certificate PEM file to verify server (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    pub root_cert: Option<PathBuf>,

    /// Allow an insecure TLS connection
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_parser = clap::value_parser!(bool))]
    pub insecure: Option<bool>,

    /// Optional TLS server name indication (SNI); defaults to the server host
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "DOMAIN")]
    pub sni: Option<String>,

    /// Padding scheme file
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    pub padding_scheme: Option<PathBuf>,

    /// Maximum logical streams per AnyTLS session, if it is 1 then multiplexing is disabled
    #[arg(short = 'm', long, value_name = "N", default_value_t = 16)]
    #[serde(skip)]
    pub max_streams_per_session: usize,

    /// Print the equivalent AnyTLS URI and exit
    #[arg(long)]
    #[serde(skip)]
    pub print_url: bool,

    #[serde(skip)]
    #[arg(skip)]
    pub display_name: Option<String>,

    /// Log level (off, error, warn, info, debug, trace)
    #[serde(skip, default = "default_log_level")]
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    pub log: log::LevelFilter,
}

impl ClientArgs {
    pub fn resolve(mut self) -> std::io::Result<Self> {
        if let Some(raw_url) = self.url.clone() {
            let parsed = parse_client_url(&raw_url)?;
            self.server = self.server.or(parsed.server);
            self.password = self.password.or(parsed.password);
            self.sni = self.sni.or(parsed.sni);
            self.client_id = self.client_id.or(parsed.client_id);
            self.insecure = self.insecure.or(parsed.insecure);
            self.display_name = parsed.display_name;
        }

        if self.server.as_ref().is_none_or(|server| server.port() == 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Server address is required (use --server or --url)",
            ));
        }
        Ok(self)
    }

    pub fn format_url(&self) -> std::io::Result<String> {
        use std::io::{Error, ErrorKind::InvalidInput};
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| Error::new(InvalidInput, "Server address is required (use --server or --url)"))?;
        let mut host = server.host();
        if host.contains('%') {
            host = host.replace('%', "%25");
        }
        if server.is_ipv6() || host.contains(':') {
            host = format!("[{host}]");
        }
        if server.port() != 443 {
            host.push_str(&format!(":{}", server.port()));
        }

        let mut uri = String::from("anytls://");
        if let Some(password) = self.password.as_deref().filter(|password| !password.is_empty()) {
            uri.push_str(&utf8_percent_encode(password, USERINFO_ENCODE_SET).to_string());
            uri.push('@');
        }
        uri.push_str(&host);
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(sni) = &self.sni {
            query.append_pair("sni", sni);
        }
        if self.insecure.unwrap_or(false) {
            query.append_pair("insecure", "1");
        }
        if let Some(client_id) = &self.client_id {
            query.append_pair("client_id", &client_id.to_string());
        }
        let query = query.finish();
        if !query.is_empty() {
            uri.push_str("/?");
            uri.push_str(&query);
        }
        if let Some(display_name) = &self.display_name {
            uri.push('#');
            uri.push_str(&utf8_percent_encode(display_name, FRAGMENT_ENCODE_SET).to_string());
        }
        Ok(uri)
    }
}

impl Default for ClientArgs {
    fn default() -> Self {
        Self {
            url: None,
            listen: SocketAddr::from(([127, 0, 0, 1], 1080)),
            server: None,
            password: None,
            client_id: None,
            root_cert: None,
            insecure: None,
            sni: None,
            padding_scheme: None,
            max_streams_per_session: 16,
            print_url: false,
            display_name: None,
            log: log::LevelFilter::Info,
        }
    }
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

const USERINFO_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'%')
    .add(b':');

const FRAGMENT_ENCODE_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'%').add(b'<').add(b'>').add(b'`');

fn parse_client_url(raw_url: &str) -> std::io::Result<ClientArgs> {
    if is_ipv6_zone_url(raw_url) {
        return parse_ipv6_zone_url(raw_url);
    }

    use std::io::{Error, ErrorKind::InvalidInput};
    let url = Url::parse(raw_url).map_err(|error| Error::new(InvalidInput, error))?;
    if url.scheme() != "anytls" {
        return Err(Error::new(InvalidInput, "URL scheme must be anytls"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::new(InvalidInput, "AnyTLS URL must include a host"))?;
    let host = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
    let port = url.port().unwrap_or(443);
    let server = Address::from((host, port));
    let encoded_password = if url.username().is_empty() {
        url.password().unwrap_or_default()
    } else {
        url.username()
    };
    let password = percent_decode_str(encoded_password).decode_utf8_lossy().into_owned();
    build_client_args(server, password, url.query().unwrap_or(""), url.fragment())
}

fn build_client_args(server: Address, password: String, query: &str, fragment: Option<&str>) -> std::io::Result<ClientArgs> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let mut args = ClientArgs {
        server: Some(server),
        password: Some(password),
        display_name: fragment
            .map(|fragment| percent_decode_str(fragment).decode_utf8_lossy().into_owned())
            .filter(|fragment| !fragment.is_empty()),
        ..Default::default()
    };

    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "password" => {
                return Err(Error::new(
                    InvalidInput,
                    "Password must be provided in the URI auth field, not in query parameters",
                ));
            }
            "sni" => args.sni = Some(value.into_owned()),
            "insecure" => match value.as_ref() {
                "1" => args.insecure = Some(true),
                "0" => args.insecure = Some(false),
                other => return Err(Error::new(InvalidInput, format!("Invalid insecure value in AnyTLS URL: {other}"))),
            },
            "client_id" if !value.is_empty() => {
                args.client_id = Some(value.parse::<Uuid>().map_err(|error| Error::new(InvalidInput, error))?);
            }
            _ => {}
        }
    }
    Ok(args)
}

fn is_ipv6_zone_url(raw_url: &str) -> bool {
    let raw = raw_url.strip_prefix("anytls://").unwrap_or(raw_url);
    let authority_end = raw.find(['/', '?', '#']).unwrap_or(raw.len());
    let authority = &raw[..authority_end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let Some(host_start) = host.find('[') else { return false };
    let Some(host_end) = host[host_start..].find(']') else {
        return false;
    };
    host[host_start..host_start + host_end].contains('%')
}

fn parse_ipv6_zone_url(raw_url: &str) -> std::io::Result<ClientArgs> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let without_scheme = raw_url
        .strip_prefix("anytls://")
        .ok_or_else(|| Error::new(InvalidInput, "URL scheme must be anytls"))?;
    let authority_end = without_scheme.find(['/', '?', '#']).unwrap_or(without_scheme.len());
    let (authority, rest) = without_scheme.split_at(authority_end);
    let (userinfo, host_port) = authority.rsplit_once('@').unwrap_or(("", authority));
    let host_start = host_port
        .find('[')
        .ok_or_else(|| Error::new(InvalidInput, "AnyTLS URL must include a host"))?;
    let host_end = host_port[host_start + 1..]
        .find(']')
        .map(|index| host_start + 1 + index)
        .ok_or_else(|| Error::new(InvalidInput, "Invalid IPv6 host in AnyTLS URL"))?;
    let host = percent_decode_str(&host_port[host_start + 1..host_end])
        .decode_utf8_lossy()
        .into_owned();
    let after_host = &host_port[host_end + 1..];
    let port = if let Some(port_and_rest) = after_host.strip_prefix(':') {
        let end = port_and_rest.find(['/', '?', '#']).unwrap_or(port_and_rest.len());
        port_and_rest[..end]
            .parse::<u16>()
            .map_err(|error| Error::new(InvalidInput, format!("Invalid port in AnyTLS URL: {error}")))?
    } else {
        443
    };
    let encoded_password = userinfo.rsplit_once(':').map_or(
        userinfo,
        |(username, password)| {
            if username.is_empty() { password } else { username }
        },
    );
    let password = percent_decode_str(encoded_password).decode_utf8_lossy().into_owned();
    let query = rest
        .split_once('?')
        .map(|(_, query)| query.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    let fragment = rest.split_once('#').map(|(_, fragment)| fragment);
    let server = Address::from((host, port));
    build_client_args(server, password, query, fragment)
}

#[cfg(test)]
mod tests {
    use super::{ClientArgs, parse_client_url};
    use clap::Parser;

    #[test]
    fn parses_client_url_fields_and_resolves_cli_overrides() {
        let args = ClientArgs::try_parse_from([
            "anytls-client",
            "--url",
            "anytls://p%40ss%3Aword@example.com:8443/?sni=edge.example&insecure=1&client_id=f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7#node%201",
            "--sni",
            "override.example",
        ])
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(args.server.as_ref().unwrap().to_string(), "example.com:8443");
        assert_eq!(args.password.as_deref(), Some("p@ss:word"));
        assert_eq!(args.sni.as_deref(), Some("override.example"));
        assert_eq!(args.insecure, Some(true));
        assert_eq!(args.display_name.as_deref(), Some("node 1"));
        assert_eq!(
            args.format_url().unwrap(),
            "anytls://p%40ss%3Aword@example.com:8443/?sni=override.example&insecure=1&client_id=f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7#node%201"
        );
    }

    #[test]
    fn parses_client_url_ipv6_zone_ids() {
        let args = parse_client_url("anytls://secret@[fe80::abcd%25eth0]:9443/?sni=edge.example").unwrap();
        assert_eq!(args.server.as_ref().unwrap().host(), "fe80::abcd%eth0");
        assert_eq!(args.password.as_deref(), Some("secret"));
        assert_eq!(args.sni.as_deref(), Some("edge.example"));
        assert_eq!(
            args.format_url().unwrap(),
            "anytls://secret@[fe80::abcd%25eth0]:9443/?sni=edge.example"
        );
    }

    #[test]
    fn requires_server_from_url_or_cli() {
        let args = ClientArgs::try_parse_from(["anytls-client"]).unwrap();
        assert_eq!(args.resolve().unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn parses_insecure_flag_with_optional_boolean_value() {
        let args = ClientArgs::try_parse_from(["anytls-client", "--server", "127.0.0.1:443", "--password", "secret"]).unwrap();
        assert_eq!(args.insecure, None);

        let args =
            ClientArgs::try_parse_from(["anytls-client", "--server", "127.0.0.1:443", "--password", "secret", "--insecure"]).unwrap();
        assert_eq!(args.insecure, Some(true));

        let args = ClientArgs::try_parse_from(["anytls-client", "--server", "127.0.0.1:443", "--insecure", "false"]).unwrap();
        assert_eq!(args.insecure, Some(false));
    }

    #[test]
    fn parses_url_and_print_url_flags() {
        let args = ClientArgs::try_parse_from(["anytls-client", "-u", "anytls://secret@example.com", "--print-url"]).unwrap();
        assert_eq!(args.url.as_deref(), Some("anytls://secret@example.com"));
        assert!(args.print_url);
    }

    #[test]
    fn explicit_cli_values_override_url_values() {
        let args = ClientArgs::try_parse_from([
            "anytls-client",
            "--url",
            "anytls://url-pass@example.com/?sni=url.example&insecure=1",
            "--server",
            "127.0.0.1:9443",
            "--password",
            "cli-pass",
            "--sni",
            "cli.example",
            "--insecure",
            "false",
        ])
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(args.server.as_ref().unwrap().to_string(), "127.0.0.1:9443");
        assert_eq!(args.password.as_deref(), Some("cli-pass"));
        assert_eq!(args.sni.as_deref(), Some("cli.example"));
        assert_eq!(args.insecure, Some(false));
    }

    #[test]
    fn parses_optional_client_uuid() {
        let args = ClientArgs::try_parse_from([
            "anytls-client",
            "--server",
            "127.0.0.1:443",
            "--password",
            "secret",
            "--client-id",
            "f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7",
        ])
        .unwrap();
        assert_eq!(args.client_id.unwrap().to_string(), "f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7");
    }
}
