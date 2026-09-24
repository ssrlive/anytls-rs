use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};

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

pub async fn print_args<T: Serialize>(args: &T, listen_port: u16) -> std::io::Result<String> {
    let public_ip = retrieve_public_ip().await?;
    args_json_for_public_ip(args, listen_port, public_ip)
}

pub fn args_json_for_public_ip<T: Serialize>(args: &T, listen_port: u16, public_ip: IpAddr) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind::InvalidInput};
    let mut value = serde_json::to_value(args).map_err(Error::other)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::new(InvalidInput, "server arguments must serialize to an object"))?;
    object.insert(
        "listen".to_owned(),
        serde_json::Value::String(SocketAddr::new(public_ip, listen_port).to_string()),
    );
    serde_json::to_string_pretty(&value).map_err(Error::other)
}

pub fn format_anytls_url(server: SocketAddr, password: &str, sni: Option<&str>) -> String {
    let host = if server.ip().is_ipv6() {
        format!("[{}]", server.ip())
    } else {
        server.ip().to_string()
    };
    let authority = if server.port() == 443 {
        host
    } else {
        format!("{host}:{}", server.port())
    };
    let mut uri = String::from("anytls://");
    if !password.is_empty() {
        uri.push_str(&utf8_percent_encode(password, USERINFO_ENCODE_SET).to_string());
        uri.push('@');
    }
    uri.push_str(&authority);
    if let Some(sni) = sni {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("sni", sni);
        uri.push_str("/?");
        uri.push_str(&query.finish());
    }
    uri
}

pub async fn print_url(listen_port: u16, password: &str, sni: Option<&str>, panel_sync_enabled: bool) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind::InvalidInput};
    if panel_sync_enabled {
        return Err(Error::new(InvalidInput, "Cannot print AnyTLS URL when panel sync is enabled"));
    }
    let public_ip = retrieve_public_ip().await?;
    Ok(format_anytls_url(SocketAddr::new(public_ip, listen_port), password, sni))
}

async fn retrieve_public_ip() -> std::io::Result<IpAddr> {
    let ipv4_urls = [
        "https://api.ipify.org?format=text",
        "https://ifconfig.me",
        "https://icanhazip.com",
        "https://api-ipv4.ip.sb/ip",
    ];
    for endpoint in ipv4_urls {
        if let Ok(response) = reqwest::get(endpoint).await
            && let Ok(body) = response.text().await
            && let Ok(ip) = body.trim().parse::<IpAddr>()
            && ip.is_ipv4()
        {
            return Ok(ip);
        }
    }

    let ipv6_urls = [
        "https://api6.ipify.org?format=text",
        "https://ipv6.icanhazip.com",
        "https://api-ipv6.ip.sb/ip",
    ];
    for endpoint in ipv6_urls {
        if let Ok(response) = reqwest::get(endpoint).await
            && let Ok(body) = response.text().await
            && let Ok(ip) = body.trim().parse::<IpAddr>()
            && ip.is_ipv6()
        {
            return Ok(ip);
        }
    }
    Err(std::io::Error::other("Cannot retrieve public IP"))
}

#[cfg(test)]
mod tests {
    use super::{args_json_for_public_ip, format_anytls_url, print_url};
    use serde_json::{Value, json};
    use std::{net::IpAddr, str::FromStr};

    #[test]
    fn prints_args_with_public_ip_and_listen_port() {
        let args = json!({ "listen": "0.0.0.0:8443", "password": "secret" });
        let output = args_json_for_public_ip(&args, 8443, IpAddr::from_str("203.0.113.7").unwrap()).unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["listen"], "203.0.113.7:8443");
        assert_eq!(output["password"], "secret");
    }

    #[test]
    fn formats_share_urls_with_empty_password_ipv6_and_sni() {
        let url = format_anytls_url("[2001:db8::1]:443".parse().unwrap(), "", Some("edge.example"));
        assert_eq!(url, "anytls://[2001:db8::1]/?sni=edge.example");
    }

    #[test]
    fn formats_passwords_and_omits_default_port() {
        let url = format_anytls_url("203.0.113.8:443".parse().unwrap(), "a@b c", Some("edge.example"));
        assert_eq!(url, "anytls://a%40b%20c@203.0.113.8/?sni=edge.example");
    }

    #[tokio::test]
    async fn panel_managed_urls_are_refused_even_with_empty_password() {
        let error = print_url(443, "", None, true).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
