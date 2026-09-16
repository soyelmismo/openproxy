use std::io;
use tokio::net::TcpStream;

#[derive(Debug, Clone)]
pub(crate) struct ProxyConfig {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) auth: Option<String>,
}

fn extract_proxy_auth(uri: &http::Uri) -> Option<String> {
    let a = uri.authority()?;
    let (user_pass, _) = a.as_str().split_once('@')?;
    Some(user_pass.to_string())
}

pub(crate) fn parse_proxy_url(url: &str) -> Result<ProxyConfig, String> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e: http::uri::InvalidUri| format!("Invalid proxy URL: {e}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| "Missing proxy scheme".to_string())?
        .to_string();
    let host = uri
        .host()
        .ok_or_else(|| "Missing proxy host".to_string())?
        .to_string();
    let port = uri
        .port_u16()
        .ok_or_else(|| "Missing proxy port".to_string())?;
    let auth = extract_proxy_auth(&uri);

    Ok(ProxyConfig {
        scheme,
        host,
        port,
        auth,
    })
}

fn socks5_build_connect_req(dest_host: &str, dest_port: u16) -> Vec<u8> {
    let dest_host_bytes = dest_host.as_bytes();
    if let Ok(ip) = dest_host.parse::<std::net::Ipv4Addr>() {
        let mut req = Vec::with_capacity(10);
        req.extend_from_slice(&[0x05, 0x01, 0x00, 0x01]);
        req.extend_from_slice(&ip.octets());
        req.extend_from_slice(&dest_port.to_be_bytes());
        req
    } else if let Ok(ip) = dest_host.parse::<std::net::Ipv6Addr>() {
        let mut req = Vec::with_capacity(22);
        req.extend_from_slice(&[0x05, 0x01, 0x00, 0x04]);
        req.extend_from_slice(&ip.octets());
        req.extend_from_slice(&dest_port.to_be_bytes());
        req
    } else {
        let mut req = Vec::with_capacity(7 + dest_host_bytes.len());
        req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, dest_host_bytes.len() as u8]);
        req.extend(dest_host_bytes);
        req.extend_from_slice(&dest_port.to_be_bytes());
        req
    }
}

async fn socks5_tunnel(
    mut stream: TcpStream,
    dest_host: &str,
    dest_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x05 || resp[1] != 0x00 {
        return Err(io::Error::other("SOCKS5 auth negotiation failed").into());
    }

    let req = socks5_build_connect_req(dest_host, dest_port);
    stream.write_all(&req).await?;

    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 0x05 || reply[1] != 0x00 {
        return Err(io::Error::other(format!("SOCKS5 connect failed: code={}", reply[1])).into());
    }

    match reply[3] {
        0x01 => {
            let mut addr = [0u8; 4 + 2];
            stream.read_exact(&mut addr).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut domain).await?;
        }
        0x04 => {
            let mut addr = [0u8; 16 + 2];
            stream.read_exact(&mut addr).await?;
        }
        _ => return Err(io::Error::other("SOCKS5 invalid atyp").into()),
    }

    Ok(stream)
}

async fn socks4_tunnel(
    mut stream: TcpStream,
    dest_host: &str,
    dest_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut req = Vec::new();
    req.push(0x04);
    req.push(0x01);
    req.extend_from_slice(&dest_port.to_be_bytes());

    let is_ip = if let Ok(ip) = dest_host.parse::<std::net::Ipv4Addr>() {
        req.extend_from_slice(&ip.octets());
        true
    } else {
        req.extend_from_slice(&[0, 0, 0, 1]);
        false
    };

    req.push(0x00);

    if !is_ip {
        req.extend_from_slice(dest_host.as_bytes());
        req.push(0x00);
    }

    stream.write_all(&req).await?;

    let mut resp = [0u8; 8];
    stream.read_exact(&mut resp).await?;
    if resp[1] != 0x5a {
        return Err(
            io::Error::other(format!("SOCKS4 connection rejected: code={}", resp[1])).into(),
        );
    }
    Ok(stream)
}

async fn read_http_connect_headers(
    stream: &mut TcpStream,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    let mut headers_buf = Vec::new();
    let mut byte_buf = [0u8; 1];
    loop {
        stream.read_exact(&mut byte_buf).await?;
        headers_buf.push(byte_buf[0]);
        if headers_buf.ends_with(b"\r\n\r\n") {
            return Ok(headers_buf);
        }
        if headers_buf.len() > 8192 {
            return Err(io::Error::other("HTTP CONNECT response headers too long").into());
        }
    }
}

async fn http_connect_tunnel(
    mut stream: TcpStream,
    proxy: &ProxyConfig,
    dest_host: &str,
    dest_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt;

    let auth_header = if let Some(ref auth) = proxy.auth {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth.as_bytes());
        format!("Proxy-Authorization: Basic {encoded}\r\n")
    } else {
        String::new()
    };
    let request = format!(
        "CONNECT {dest_host}:{dest_port} HTTP/1.1\r\nHost: {dest_host}:{dest_port}\r\nProxy-Connection: Keep-Alive\r\n{auth_header}\r\n"
    );
    stream.write_all(request.as_bytes()).await?;

    let headers_buf = read_http_connect_headers(&mut stream).await?;
    let resp_str = String::from_utf8_lossy(&headers_buf);
    let first_line = resp_str.lines().next().unwrap_or("");
    if !first_line.contains(" 200 ") {
        return Err(
            io::Error::other(format!("HTTP CONNECT proxy returned error: {first_line}")).into(),
        );
    }
    Ok(stream)
}

pub(crate) async fn run_proxy_tunnel(
    stream: TcpStream,
    proxy: &ProxyConfig,
    dest_host: &str,
    dest_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    let scheme = proxy.scheme.as_str();
    if scheme.eq_ignore_ascii_case("socks5") {
        socks5_tunnel(stream, dest_host, dest_port).await
    } else if scheme.eq_ignore_ascii_case("socks4") {
        socks4_tunnel(stream, dest_host, dest_port).await
    } else if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        http_connect_tunnel(stream, proxy, dest_host, dest_port).await
    } else {
        Err(io::Error::other(format!("Unsupported proxy scheme: {}", proxy.scheme)).into())
    }
}
