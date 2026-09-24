use crate::panel_sync::PanelSyncConfig;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use url::Url;

#[derive(Parser, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[command(version, author, name = "anytls-server", about = "AnyTLS rust server")]
pub struct ServerArgs {
    /// Server listen port
    #[arg(short = 'l', long, value_name = "IP:PORT", default_value = "0.0.0.0:8443")]
    pub listen: SocketAddr,

    /// Password for anytls server authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// Padding scheme file
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    pub padding_scheme: Option<PathBuf>,

    /// TLS server name indication (SNI)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "SNI")]
    pub sni: Option<String>,

    /// Redirect unauthenticated TLS probes to a direct target URL instead of SNI probe fallback; accepts http:// or https:// URLs.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "URL")]
    pub forward: Option<Url>,

    /// Maximum logical streams accepted on one authenticated TLS session
    #[arg(short = 'm', long, value_name = "N", default_value_t = 1024)]
    pub max_streams_per_session: usize,

    /// TLS certificate PEM file (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    pub cert: Option<PathBuf>,

    /// TLS private key PEM file (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "FILE")]
    pub key: Option<PathBuf>,

    /// Panel webapi base url for sync
    #[arg(long, value_name = "URL")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel_webapi_url: Option<Url>,

    /// Panel webapi token
    #[arg(long, value_name = "TOKEN")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel_webapi_token: Option<String>,

    /// Panel node id
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "ID")]
    pub panel_node_id: Option<usize>,

    /// Panel API update interval seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long, value_name = "SECS")]
    pub panel_update_interval_secs: Option<u64>,

    /// Log level (off, error, warn, info, debug, trace)
    #[serde(skip, default = "default_log_level")]
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    pub log: log::LevelFilter,

    /// Print command line arguments
    #[serde(skip)]
    #[arg(long)]
    pub print_args: bool,

    /// Print URL for connecting to this server
    #[serde(skip)]
    #[arg(long)]
    pub print_url: bool,
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

impl ServerArgs {
    pub fn panel_sync_enabled(&self) -> bool {
        self.panel_webapi_url.is_some() && self.panel_webapi_token.is_some() && self.panel_node_id.is_some()
    }

    pub fn panel_sync_config(&self) -> std::io::Result<Option<PanelSyncConfig>> {
        use std::io::{Error, ErrorKind::InvalidInput};
        let has_required_value = self.panel_webapi_url.is_some() || self.panel_webapi_token.is_some() || self.panel_node_id.is_some();
        if !has_required_value && self.panel_update_interval_secs.is_none() {
            return Ok(None);
        }

        match (&self.panel_webapi_url, &self.panel_webapi_token, self.panel_node_id) {
            (Some(webapi_url), Some(webapi_token), Some(node_id)) => Ok(Some(PanelSyncConfig {
                webapi_url: webapi_url.clone(),
                webapi_token: webapi_token.clone(),
                node_id,
                update_interval_secs: self.panel_update_interval_secs.unwrap_or(10).max(5),
            })),
            _ => Err(Error::new(
                InvalidInput,
                "--panel-webapi-url, --panel-webapi-token, and --panel-node-id must be provided together",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServerArgs;
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn panel_sync_requires_all_connection_parameters_and_clamps_interval() {
        let args = ServerArgs::try_parse_from([
            "anytls-server",
            "--password",
            "secret",
            "--panel-webapi-url",
            "https://panel.example/api",
            "--panel-webapi-token",
            "token",
            "--panel-node-id",
            "42",
            "--panel-update-interval-secs",
            "2",
        ])
        .unwrap();
        assert_eq!(args.panel_sync_config().unwrap().unwrap().update_interval_secs, 5);

        let incomplete = ServerArgs::try_parse_from([
            "anytls-server",
            "--password",
            "secret",
            "--panel-webapi-url",
            "https://panel.example/api",
        ])
        .unwrap();
        assert_eq!(incomplete.panel_sync_config().unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn parses_padding_sni_and_forward_options() {
        let args = ServerArgs::try_parse_from([
            "anytls-server",
            "--password",
            "secret",
            "--padding-scheme",
            "scheme.json",
            "--sni",
            "probe.example.com",
            "--forward",
            "https://target.example.com:8443/path",
        ])
        .unwrap();

        assert_eq!(args.padding_scheme.as_deref(), Some(Path::new("scheme.json")));
        assert_eq!(args.sni.as_deref(), Some("probe.example.com"));
        assert_eq!(args.forward.unwrap().as_str(), "https://target.example.com:8443/path");
    }

    #[test]
    fn print_flags_are_not_serialized() {
        let args = ServerArgs::try_parse_from(["anytls-server", "--print-args", "--print-url"]).unwrap();
        assert!(args.print_args);
        assert!(args.print_url);
        let serialized = serde_json::to_value(args).unwrap();
        assert!(serialized.get("print_args").is_none());
        assert!(serialized.get("print_url").is_none());
    }
}
