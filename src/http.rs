use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use url::Url;

#[derive(PartialEq)]
pub enum HttpVersion {
    Http1_0,
    Http1_1,
}

pub struct Client {
    http_version: HttpVersion,
    headers: Headers,
}

impl Client {
    pub fn new(http_version: HttpVersion) -> Self {
        Self {
            http_version,
            headers: Default::default(),
        }
    }

    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.headers
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        self.send("GET", url, None)
    }

    pub fn post(&self, url: &str, body: Vec<u8>) -> Result<Response> {
        self.send("POST", url, Some(body))
    }

    pub fn send(&self, method: &str, url: &str, body: Option<Vec<u8>>) -> Result<Response> {
        let url = Url::parse(url)?;

        let host = url
            .host()
            .ok_or_else(|| anyhow!("Given URL does not contain a host"))?;
        let port = url.port().unwrap_or(match url.scheme() {
            "http" => 80,
            "https" => 443,
            _ => bail!("Unknown scheme: {}", url.scheme()),
        });

        let mut headers = self.headers.clone();
        headers.insert("Host", &host);
        if !headers.contains("User-Agent") {
            headers.insert(
                "User-Agent",
                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            );
        }
        if self.http_version == HttpVersion::Http1_1 {
            headers.insert("Connection", "close");
        }

        let mut path = url.path().to_string();
        if let Some(query) = url.query() {
            path = format!("{path}?{query}");
        }
        let req = Request {
            method: method.to_string(),
            path,
            headers,
            body,
        };

        let mut request_bytes = Vec::new();
        match self.http_version {
            HttpVersion::Http1_0 => req.send_v10(&mut request_bytes)?,
            HttpVersion::Http1_1 => req.send_v11(&mut request_bytes)?,
        }

        println!(">>> {} bytes", request_bytes.len());
        println!(
            "{}",
            prefix_lines(&String::from_utf8_lossy(&request_bytes), ">>> "),
        );

        let mut stream = TcpStream::connect_timeout(
            &format!("{host}:{port}")
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| anyhow!("Failed to resolve given address"))?,
            Duration::from_secs(30),
        )?;

        let mut response_bytes = Vec::new();
        if url.scheme() == "https" {
            let root_store =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            let server_name = host.to_string().try_into()?;
            let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)?;
            let stream = rustls::Stream::new(&mut conn, &mut stream);

            let result = do_read_write(stream, &request_bytes, &mut response_bytes);
            if let Err(error) = &result {
                if error.kind() != std::io::ErrorKind::UnexpectedEof {
                    result?;
                }
            }
        } else {
            do_read_write(stream, &request_bytes, &mut response_bytes)?;
        }

        println!();
        println!("<<< {} bytes", response_bytes.len());
        println!(
            "{}",
            prefix_lines(&String::from_utf8_lossy(&response_bytes), "<<< "),
        );

        let response = Response::parse(&response_bytes)?;
        Ok(response)
    }
}

fn do_read_write<S>(
    mut stream: S,
    request_bytes: &[u8],
    response_bytes: &mut Vec<u8>,
) -> std::io::Result<()>
where
    S: Read + Write,
{
    stream.write_all(request_bytes)?;
    //stream.flush()?;
    stream.read_to_end(response_bytes)?;
    Ok(())
}

#[derive(Clone, Default)]
pub struct Headers(BTreeMap<String, String>);

impl Headers {
    pub fn insert(&mut self, key: impl ToString, val: impl ToString) {
        self.0
            .insert(key.to_string().to_lowercase(), val.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(&key.to_lowercase())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(&key.to_lowercase())
    }

    pub fn remove(&mut self, key: &str) {
        self.0.remove(&key.to_lowercase());
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

struct Request {
    method: String,
    path: String,
    headers: Headers,
    body: Option<Vec<u8>>,
}

impl Request {
    pub fn send_v10(mut self, mut writer: impl Write) -> Result<()> {
        let body = self.body.unwrap_or_default();
        self.headers.insert("Content-Length", body.len());

        writeln!(writer, "{} {} HTTP/1.0\r", self.method, self.path)?;

        for (key, value) in self.headers.iter() {
            writeln!(writer, "{key}: {value}\r")?;
        }

        writer.write_all(b"\r\n")?;

        if !body.is_empty() {
            writer.write_all(&body)?;
        }

        Ok(())
    }

    pub fn send_v11(mut self, mut writer: impl Write) -> Result<()> {
        let body = self.body.unwrap_or_default();
        self.headers.insert("Transfer-Encoding", "chunked");

        writeln!(writer, "{} {} HTTP/1.1\r", self.method, self.path)?;

        for (key, value) in self.headers.iter() {
            writeln!(writer, "{key}: {value}\r")?;
        }

        writer.write_all(b"\r\n")?;

        if !body.is_empty() {
            write!(writer, "{:x}\r\n", body.len())?;
            writer.write_all(&body)?;
            writer.write_all(b"\r\n")?;
        }

        writer.write_all(b"0\r\n\r\n")?;

        Ok(())
    }
}

pub struct Response {
    status_code: u16,
    status_message: String,
    headers: Headers,
    body: Vec<u8>,
}

impl Response {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut state = ResponseParserState::Status;

        let mut status_code = 0u16;
        let mut status_message = String::new();
        let mut headers = Headers::default();
        let mut content_length = 0usize;
        let mut body = Vec::new();
        let mut is_chunked = false;

        let mut pos = 0;
        for mut line in bytes.split(|&c| c == b'\n') {
            pos += line.len() + 1;

            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }

            match state {
                ResponseParserState::Status => {
                    let line = String::from_utf8(Vec::from(line))?;
                    let parts: Vec<&str> = line.splitn(3, ' ').collect();
                    status_code = parts[1].parse()?;
                    status_message = parts[2].to_owned();
                    state = ResponseParserState::Header;
                    continue;
                }
                ResponseParserState::Header => {
                    if line.is_empty() {
                        break;
                    }

                    let line = String::from_utf8(Vec::from(line))?;
                    let (key, value) = line
                        .split_once(": ")
                        .ok_or_else(|| anyhow!("Invalid header"))?;
                    headers.insert(key, value);
                    match key.to_lowercase().as_str() {
                        "content-length" => content_length = value.parse()?,
                        "transfer-encoding" => {
                            if value.to_lowercase() != "chunked" {
                                bail!("Unknown transfer-encoding value: {value}");
                            }

                            is_chunked = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        if is_chunked {
            let mut body_slice = &bytes[pos..];
            loop {
                let mut parts = body_slice.split(|&c| c == b'\r');
                let octets = parts.next().ok_or_else(|| anyhow!("Invalid chunk"))?;

                assert_eq!(&body_slice[octets.len()..octets.len() + 2], b"\r\n");
                let remaining = &body_slice[octets.len() + 2..];

                let length: usize =
                    usize::from_str_radix(&String::from_utf8(Vec::from(octets))?, 16)?;
                if length == 0 {
                    break;
                }

                let chunk = &remaining[..length];
                body.extend_from_slice(chunk);

                assert_eq!(&remaining[length..length + 2], b"\r\n");
                body_slice = &remaining[length + 2..];
            }
        } else {
            if content_length == 0 {
                content_length = bytes.len().saturating_sub(pos);
            }

            body.extend_from_slice(&bytes[pos..pos + content_length]);
        }

        Ok(Response {
            status_code,
            status_message,
            headers,
            body,
        })
    }

    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    pub fn status_message(&self) -> &str {
        &self.status_message
    }

    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

enum ResponseParserState {
    Status,
    Header,
}

fn prefix_lines(str: &str, prefix: &str) -> String {
    str.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<String>>()
        .join("\n")
}
