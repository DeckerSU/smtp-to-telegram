use anyhow::{Context, Result};
use clap::Parser;
use mail_parser::{MessageParser, MimeHeaders};
use smtp_proto::Request;
use smtp_proto::Response;
use std::borrow::Cow;
use std::collections::HashSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use ammonia::{Builder, UrlRelative};
use once_cell::sync::Lazy;
use std::collections::HashMap;

static TELEGRAM_HTML_SANITIZER: Lazy<Builder<'static>> = Lazy::new(|| {
    // Allowed tags according to Telegram HTML-style
    let mut tags: HashSet<&'static str> = HashSet::new();
    for t in [
        "b",
        "strong",
        "i",
        "em",
        "u",
        "ins",
        "s",
        "strike",
        "del",
        "span",
        "tg-spoiler",
        "a",
        "code",
        "pre",
    ] {
        tags.insert(t);
    }

    // Build tag attributes map
    let mut tag_attrs: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
    let mut a_attrs = HashSet::new();
    a_attrs.insert("href");
    tag_attrs.insert("a", a_attrs);

    let mut span_attrs = HashSet::new();
    span_attrs.insert("class");
    tag_attrs.insert("span", span_attrs);

    let mut code_attrs = HashSet::new();
    code_attrs.insert("class");
    tag_attrs.insert("code", code_attrs);

    let mut pre_attrs = HashSet::new();
    pre_attrs.insert("class");
    tag_attrs.insert("pre", pre_attrs);

    // Builder for sanitizer
    let mut builder = Builder::default();

    builder
        // Allow only listed tags
        .tags(tags)
        // By default ammonia allows many attributes,
        // here we reset everything and explicitly set what we need
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attrs)
        // Don't touch relative URLs (Telegram will handle them)
        .url_relative(UrlRelative::PassThrough)
        // Don't add/change rel on links
        .link_rel(None)
        // Filter attribute values (keep only allowed ones)
        .attribute_filter(|tag, attr, value| {
            match (tag, attr) {
                // Allow only class="tg-spoiler" on <span>
                ("span", "class") => {
                    let has_spoiler = value.split_whitespace().any(|cls| cls == "tg-spoiler");
                    if has_spoiler {
                        // Telegram only needs one tg-spoiler class
                        Some(Cow::Borrowed("tg-spoiler"))
                    } else {
                        None
                    }
                }
                // On <code>/<pre> keep only class="language-..."
                ("code", "class") | ("pre", "class") => {
                    if value.starts_with("language-") {
                        Some(Cow::Borrowed(value))
                    } else {
                        None
                    }
                }
                // Other allowed attributes keep as is
                _ => Some(Cow::Borrowed(value)),
            }
        });

    builder
});

const COPYRIGHT: &str = "Decker + ChatGPT/Cursor/Manus";

// Macro to create version string with copyright
// Uses COPYRIGHT constant value - must match COPYRIGHT constant above
const VERSION_WITH_COPYRIGHT: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nCopyright (c) ",
    "Decker + ChatGPT/Cursor/Manus" // Must match COPYRIGHT constant
);

#[derive(Parser, Debug)]
#[command(
    author = COPYRIGHT,
    version = VERSION_WITH_COPYRIGHT,
    about = "SMTP to Telegram forwarder - forwards email messages to Telegram bot",
    long_about = concat!(
        "SMTP to Telegram forwarder\n\n",
        "Forwards email messages received via SMTP to a Telegram bot.\n\n",
        "Copyright (c) Decker + ChatGPT/Cursor/Manus"
    ),
    override_usage = "smtp-to-telegram --token <TOKEN> --chat-id <CHAT_ID> [--port <PORT>] [--bind <ADDRESS>]"
)]
struct Args {
    /// Telegram Bot Token
    #[arg(short, long, env = "TELEGRAM_TOKEN")]
    token: String,

    /// Telegram Chat ID
    #[arg(short, long, env = "TELEGRAM_CHAT_ID")]
    chat_id: String,

    /// SMTP server port
    #[arg(short, long, default_value = "2525", env = "SMTP_PORT")]
    port: u16,

    /// Bind address for SMTP server
    #[arg(short, long, default_value = "0.0.0.0", env = "SMTP_BIND")]
    bind: String,
}

struct SmtpSession {
    stream: TcpStream,
    telegram_token: String,
    telegram_chat_id: String,
    buffer: Vec<u8>,
}

impl SmtpSession {
    fn new(stream: TcpStream, telegram_token: String, telegram_chat_id: String) -> Self {
        Self {
            stream,
            telegram_token,
            telegram_chat_id,
            buffer: Vec::new(),
        }
    }

    async fn send_response(&mut self, response: Response<String>) -> Result<()> {
        let mut buf = Vec::new();
        response
            .write(&mut buf)
            .context("Failed to format response")?;
        self.stream
            .write_all(&buf)
            .await
            .context("Failed to write response")?;
        Ok(())
    }

    async fn read_line_bytes(&mut self) -> Result<Vec<u8>> {
        let mut buf = [0u8; 1];
        let mut line = Vec::new();

        loop {
            let n = self
                .stream
                .read_exact(&mut buf)
                .await
                .context("Failed to read from stream")?;

            if n == 0 {
                return Err(anyhow::anyhow!("Connection closed"));
            }

            line.push(buf[0]);

            if line.len() >= 2 && line[line.len() - 2] == b'\r' && line[line.len() - 1] == b'\n' {
                return Ok(line);
            }
        }
    }

    async fn send_to_telegram(&self, text: &str, parse_mode: Option<&str>) -> Result<()> {
        self.send_to_telegram_internal(text, parse_mode).await
    }

    async fn send_to_telegram_internal(&self, text: &str, parse_mode: Option<&str>) -> Result<()> {
        // Telegram API limit: 1-4096 characters after entities parsing
        const MAX_MESSAGE_LENGTH: usize = 4096;

        // Check if text is empty or too short
        if text.trim().is_empty() {
            return Err(anyhow::anyhow!("Message text is empty"));
        }

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.telegram_token
        );

        let client = reqwest::Client::new();

        // Build form data
        let mut form_data = vec![("chat_id", self.telegram_chat_id.as_str()), ("text", text)];

        // Add parse_mode if specified
        if let Some(mode) = parse_mode {
            form_data.push(("parse_mode", mode));
        }

        // If message fits in one part, send it directly
        if text.chars().count() <= MAX_MESSAGE_LENGTH {
            let response = client
                .post(&url)
                .form(&form_data)
                .send()
                .await
                .context("Failed to send request to Telegram")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("Telegram API error: {} - {}", status, body));
            }

            return Ok(());
        }

        // Split long message into chunks
        // Try to split at line boundaries first, then at word boundaries
        let mut chunks = Vec::new();
        let mut remaining = text;

        while !remaining.is_empty() {
            let char_count = remaining.chars().count();

            if char_count <= MAX_MESSAGE_LENGTH {
                chunks.push(remaining.to_string());
                break;
            }

            // Find byte position for MAX_MESSAGE_LENGTH characters
            let max_byte_pos = remaining
                .char_indices()
                .nth(MAX_MESSAGE_LENGTH)
                .map(|(byte_pos, _)| byte_pos)
                .unwrap_or(remaining.len());

            // Try to find a good split point (prefer line break, then space)
            // Look back from the limit to find a natural break point (up to 500 bytes)
            let search_start = max_byte_pos.saturating_sub(500.min(max_byte_pos));
            let search_end = max_byte_pos;

            let mut split_pos = max_byte_pos;

            // First, try to find a line break
            if let Some(byte_pos) = remaining[search_start..search_end]
                .rfind('\n')
                .map(|pos| search_start + pos + 1)
            {
                split_pos = byte_pos;
            }
            // If no line break, try to find a space
            else if let Some(byte_pos) = remaining[search_start..search_end]
                .rfind(char::is_whitespace)
                .map(|pos| search_start + pos + 1)
            {
                split_pos = byte_pos;
            }

            // Split at the found position
            let (chunk, rest) = remaining.split_at(split_pos);
            chunks.push(chunk.to_string());
            remaining = rest;
        }

        // Send each chunk
        for (index, chunk) in chunks.iter().enumerate() {
            let chunk_text = if chunks.len() > 1 {
                format!("[{}/{}]\n\n{}", index + 1, chunks.len(), chunk)
            } else {
                chunk.clone()
            };

            // Ensure chunk is within limits (with prefix)
            let final_text = if chunk_text.chars().count() > MAX_MESSAGE_LENGTH {
                // If prefix makes it too long, truncate the chunk
                let prefix_len = format!("[{}/{}]\n\n", index + 1, chunks.len())
                    .chars()
                    .count();
                let max_chunk_len = MAX_MESSAGE_LENGTH.saturating_sub(prefix_len);
                let truncated_chunk: String = chunk.chars().take(max_chunk_len).collect();
                format!("[{}/{}]\n\n{}", index + 1, chunks.len(), truncated_chunk)
            } else {
                chunk_text
            };

            // Build form data for chunk
            let mut chunk_form_data = vec![
                ("chat_id", self.telegram_chat_id.as_str()),
                ("text", &final_text),
            ];

            // Add parse_mode if specified
            if let Some(mode) = parse_mode {
                chunk_form_data.push(("parse_mode", mode));
            }

            let response = client
                .post(&url)
                .form(&chunk_form_data)
                .send()
                .await
                .context(format!(
                    "Failed to send chunk {}/{} to Telegram",
                    index + 1,
                    chunks.len()
                ))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "Telegram API error for chunk {}/{}: {} - {}",
                    index + 1,
                    chunks.len(),
                    status,
                    body
                ));
            }

            // Small delay between messages to avoid rate limiting
            if index < chunks.len() - 1 {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        Ok(())
    }

    fn convert_html_to_telegram(&self, html: &str) -> String {
        TELEGRAM_HTML_SANITIZER.clean(html).to_string()
    }

    fn extract_text_from_email(&self, email_data: &[u8]) -> (String, Option<String>) {
        // Use mail-parser to parse the email message
        // mail-parser automatically handles all encodings (base64, quoted-printable, etc.)
        // and returns text in UTF-8
        let parser = MessageParser::default();

        if let Some(msg) = parser.parse(email_data) {
            // Get Content-Type
            let content_type = msg.content_type().and_then(|ct| {
                ct.subtype().map(|subtype| {
                    let mime_type = format!("{}/{}", ct.ctype(), subtype);
                    mime_type.to_lowercase()
                })
            });

            if let Some(ref ct) = content_type {
                println!("Found Content-Type: {}", ct);
            } else {
                println!("Content-Type not found in email headers");
            }

            // Get subject in UTF-8
            let subject = if let Some(subj) = msg.subject() {
                println!("Subject: {}", subj);
                subj.to_string()
            } else {
                println!("Subject not found in email headers");
                String::new()
            };

            // Get body - use HTML if Content-Type is text/html, otherwise use text
            let body = if let Some(ref ct) = content_type {
                if ct.starts_with("text/html") {
                    // Use HTML body for HTML content
                    msg.body_html(0).unwrap_or_default()
                } else {
                    // Use text body for other content types
                    msg.body_text(0).unwrap_or_default()
                }
            } else {
                // If no Content-Type, try text first, then HTML
                let text_body = msg.body_text(0).unwrap_or_default();
                if !text_body.is_empty() {
                    text_body
                } else {
                    msg.body_html(0).unwrap_or_default()
                }
            };

            if !body.is_empty() {
                // Clean up extra whitespace from body
                let cleaned_body = body
                    .lines()
                    .map(|line| line.trim())
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");

                // Combine subject and body
                let cleaned = if !subject.is_empty() {
                    format!("Subject: {}\n\n{}", subject, cleaned_body)
                } else {
                    cleaned_body
                };

                return (cleaned, content_type);
            }

            // Fallback: return empty
            (String::new(), content_type)
        } else {
            eprintln!("Failed to parse email message");
            // Fallback to string conversion
            let text = String::from_utf8_lossy(email_data).to_string();
            (text, None)
        }
    }

    async fn handle(&mut self) -> Result<()> {
        // Send greeting
        self.send_response(Response::new(
            220,
            0,
            0,
            0,
            "SMTP to Telegram Service Ready".to_string(),
        ))
        .await?;

        let mut mail_from: Option<String> = None;
        let mut rcpt_to: Option<String> = None;
        let mut in_data = false;

        loop {
            if in_data {
                // In DATA mode, read bytes directly to preserve UTF-8 encoding
                let line_bytes = self.read_line_bytes().await?;

                // Check if this is the end of DATA (single dot on a line)
                if line_bytes.len() == 3
                    && line_bytes[0] == b'.'
                    && line_bytes[1] == b'\r'
                    && line_bytes[2] == b'\n'
                {
                    // End of DATA
                    in_data = false;

                    // Process the received message - decode as UTF-8
                    let total_bytes = self.buffer.len();
                    println!("Received email message: {} bytes", total_bytes);

                    // Use mail-parser which handles all encodings automatically
                    let (text, content_type) = self.extract_text_from_email(&self.buffer);

                    if !text.is_empty() {
                        // Determine parse_mode based on Content-Type and convert HTML if needed
                        let (processed_text, parse_mode) = if let Some(ct) = &content_type {
                            if ct.starts_with("text/html") {
                                println!("Converting HTML to Telegram-compatible format");
                                let converted = self.convert_html_to_telegram(&text);
                                (converted, Some("HTML"))
                            } else {
                                (text, None)
                            }
                        } else {
                            (text, None)
                        };

                        // Format message for Telegram
                        let telegram_message =
                            if let (Some(from), Some(to)) = (&mail_from, &rcpt_to) {
                                format!("From: {}\nTo: {}\n\n{}", from, to, processed_text)
                            } else {
                                processed_text
                            };

                        if let Some(mode) = parse_mode {
                            println!(
                                "Detected Content-Type: {}, using parse_mode: {}",
                                content_type.as_ref().unwrap(),
                                mode
                            );
                        }

                        let message_bytes = telegram_message.len();
                        println!(
                            "Message to send: {} bytes ({} characters)",
                            message_bytes,
                            telegram_message.chars().count()
                        );

                        if let Err(e) = self.send_to_telegram(&telegram_message, parse_mode).await {
                            eprintln!("Failed to send to Telegram: {}", e);
                        } else {
                            println!("Message forwarded to Telegram successfully");
                        }
                    }

                    self.buffer.clear();
                    mail_from = None;
                    rcpt_to = None;

                    self.send_response(Response::new(250, 0, 0, 0, "OK".to_string()))
                        .await?;
                } else {
                    // If line starts with "..", remove the first dot (SMTP escaping)
                    let processed_bytes = if line_bytes.len() >= 3
                        && line_bytes[0] == b'.'
                        && line_bytes[1] == b'.'
                        && line_bytes[line_bytes.len() - 2] == b'\r'
                        && line_bytes[line_bytes.len() - 1] == b'\n'
                    {
                        // Remove the first dot and keep the rest (including \r\n)
                        line_bytes[1..].to_vec()
                    } else {
                        // Keep the line as is (including \r\n)
                        line_bytes
                    };

                    // Continue reading data - store bytes directly
                    self.buffer.extend_from_slice(&processed_bytes);
                }
                continue;
            }

            // Parse SMTP command
            // Request::parse requires a complete line with \r\n, so we read bytes directly
            let line_bytes = self.read_line_bytes().await?;
            let mut iter = line_bytes.iter();
            let request = Request::parse(&mut iter)
                .map_err(|e| anyhow::anyhow!("Failed to parse SMTP request: {:?}", e))?;

            match request {
                Request::Helo { host } | Request::Ehlo { host } => {
                    self.send_response(Response::new(
                        250,
                        0,
                        0,
                        0,
                        format!("Hello {}", host.into_owned()),
                    ))
                    .await?;
                }
                Request::Lhlo { host } => {
                    self.send_response(Response::new(
                        250,
                        0,
                        0,
                        0,
                        format!("Hello {}", host.into_owned()),
                    ))
                    .await?;
                }
                Request::Mail { from } => {
                    mail_from = Some(from.address.into_owned());
                    self.send_response(Response::new(250, 0, 0, 0, "OK".to_string()))
                        .await?;
                }
                Request::Rcpt { to } => {
                    rcpt_to = Some(to.address.into_owned());
                    self.send_response(Response::new(250, 0, 0, 0, "OK".to_string()))
                        .await?;
                }
                Request::Data => {
                    if mail_from.is_none() || rcpt_to.is_none() {
                        self.send_response(Response::new(
                            503,
                            0,
                            0,
                            0,
                            "Need MAIL and RCPT first".to_string(),
                        ))
                        .await?;
                        continue;
                    }
                    in_data = true;
                    self.buffer.clear();
                    self.send_response(Response::new(
                        354,
                        0,
                        0,
                        0,
                        "End data with <CR><LF>.<CR><LF>".to_string(),
                    ))
                    .await?;
                }
                Request::Rset => {
                    mail_from = None;
                    rcpt_to = None;
                    self.buffer.clear();
                    self.send_response(Response::new(250, 0, 0, 0, "OK".to_string()))
                        .await?;
                }
                Request::Quit => {
                    self.send_response(Response::new(221, 0, 0, 0, "Bye".to_string()))
                        .await?;
                    break;
                }
                Request::Noop { .. } => {
                    self.send_response(Response::new(250, 0, 0, 0, "OK".to_string()))
                        .await?;
                }
                Request::Vrfy { .. } => {
                    self.send_response(Response::new(
                        502,
                        0,
                        0,
                        0,
                        "Command not implemented".to_string(),
                    ))
                    .await?;
                }
                Request::Expn { .. } => {
                    self.send_response(Response::new(
                        502,
                        0,
                        0,
                        0,
                        "Command not implemented".to_string(),
                    ))
                    .await?;
                }
                Request::Help { .. } => {
                    self.send_response(Response::new(214, 0, 0, 0, "Help text".to_string()))
                        .await?;
                }
                Request::StartTls => {
                    self.send_response(Response::new(
                        502,
                        0,
                        0,
                        0,
                        "TLS not supported".to_string(),
                    ))
                    .await?;
                }
                Request::Auth { .. } => {
                    self.send_response(Response::new(
                        502,
                        0,
                        0,
                        0,
                        "Auth not supported".to_string(),
                    ))
                    .await?;
                }
                Request::Bdat { .. }
                | Request::Burl { .. }
                | Request::Etrn { .. }
                | Request::Atrn { .. } => {
                    self.send_response(Response::new(
                        502,
                        0,
                        0,
                        0,
                        "Command not implemented".to_string(),
                    ))
                    .await?;
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Print version and copyright
    println!("smtp-to-telegram v{}", env!("CARGO_PKG_VERSION"));
    println!("Copyright (c) {}", COPYRIGHT);
    println!();

    // Validate bind address
    args.bind
        .parse::<std::net::IpAddr>()
        .context(format!("Invalid bind address: {}", args.bind))?;

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = TcpListener::bind(&addr)
        .await
        .context(format!("Failed to bind to {}", addr))?;

    println!("SMTP to Telegram server listening on {}", addr);
    println!("Token: {}...", &args.token[..args.token.len().min(10)]);
    println!("Chat ID: {}", args.chat_id);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                println!("New connection from {}", addr);

                let telegram_token = args.token.clone();
                let telegram_chat_id = args.chat_id.clone();

                tokio::spawn(async move {
                    let mut session = SmtpSession::new(stream, telegram_token, telegram_chat_id);
                    if let Err(e) = session.handle().await {
                        eprintln!("Error handling session: {}", e);
                    }
                });
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
            }
        }
    }
}
