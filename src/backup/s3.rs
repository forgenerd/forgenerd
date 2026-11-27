//! Minimal S3 client (PutObject, GetObject, ListObjectsV2, DeleteObject) with AWS Signature V4,
//! built on the crate's existing `reqwest` + `hmac`/`sha2`. `rust-s3` was evaluated but
//! pulls a second `reqwest` major and a duplicate crypto stack into the binary.
//!
//! Path-style addressing (`<endpoint>/<bucket>/<key>`) works with AWS S3, Cloudflare R2,
//! Backblaze B2 and MinIO. Single PUT uploads are limited to 5 GiB by S3.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

const UNSIGNED: &str = "UNSIGNED-PAYLOAD";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const MAX_PUT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct S3Client {
    http: reqwest::Client,
    /// `scheme://host[:port]`, no trailing slash or path.
    endpoint: String,
    host: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
}

/// Validate and split an endpoint into (`scheme://host[:port]`, `host[:port]`).
pub fn parse_endpoint(endpoint: &str) -> Result<(String, String)> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .ok_or_else(|| {
            anyhow!("endpoint must start with https:// (or http:// for local servers)")
        })?;
    let valid = !rest.is_empty()
        && !rest.contains(['/', '?', '#', '@'])
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c));
    if !valid {
        bail!("endpoint must be scheme://host[:port] without a path");
    }
    Ok((endpoint.to_string(), rest.to_string()))
}

impl S3Client {
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self> {
        let (endpoint, host) = parse_endpoint(endpoint)?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60 * 60))
            .connect_timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            endpoint,
            host,
            bucket: bucket.to_string(),
            region: region.to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
        })
    }

    fn object_path(&self, key: &str) -> String {
        format!(
            "/{}/{}",
            uri_encode(&self.bucket, true),
            uri_encode(key, false)
        )
    }

    fn signed_headers(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        payload_hash: &str,
        now: DateTime<Utc>,
    ) -> Vec<(String, String)> {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let headers = vec![
            ("host".to_string(), self.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ("x-amz-date".to_string(), amz_date),
        ];
        let authorization = sign_v4(&SigningInput {
            method,
            path,
            query,
            headers: &headers,
            payload_hash,
            region: &self.region,
            access_key: &self.access_key,
            secret_key: &self.secret_key,
            now,
        });
        let mut out: Vec<(String, String)> =
            headers.into_iter().filter(|(k, _)| k != "host").collect();
        out.push(("authorization".to_string(), authorization));
        out
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        payload_hash: &str,
        body: Option<(reqwest::Body, u64)>,
    ) -> Result<reqwest::Response> {
        let headers = self.signed_headers(method.as_str(), path, query, payload_hash, Utc::now());
        let mut url = format!("{}{}", self.endpoint, path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query(query));
        }
        let mut req = self.http.request(method, url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some((body, len)) = body {
            req = req.header(reqwest::header::CONTENT_LENGTH, len).body(body);
        }
        let res = req.send().await.context("S3 request failed")?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            bail!("S3 returned {status}: {}", s3_error_message(&text));
        }
        Ok(res)
    }

    /// Stream a local file to `key`.
    pub async fn put_file(&self, key: &str, file: &Path) -> Result<u64> {
        let handle = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("opening {}", file.display()))?;
        let len = handle.metadata().await?.len();
        if len > MAX_PUT_BYTES {
            bail!("backup is {len} bytes; single uploads are limited to 5 GiB");
        }
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(handle));
        self.send(
            reqwest::Method::PUT,
            &self.object_path(key),
            &[],
            UNSIGNED,
            Some((body, len)),
        )
        .await?;
        Ok(len)
    }

    /// All objects under `prefix` (follows pagination).
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let path = format!("/{}", uri_encode(&self.bucket, true));
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            let res = self
                .send(reqwest::Method::GET, &path, &query, EMPTY_SHA256, None)
                .await?;
            let xml = res.text().await?;
            out.extend(parse_list(&xml));
            token = xml_tag(&xml, "NextContinuationToken").map(|t| xml_unescape(&t));
            if xml_tag(&xml, "IsTruncated").as_deref() != Some("true") || token.is_none() {
                return Ok(out);
            }
        }
    }

    /// Download `key` into a new 0600 file at `out`; returns the bytes written.
    pub async fn get_to_file(&self, key: &str, out: &Path) -> Result<u64> {
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncWriteExt;

        let mut res = self
            .send(
                reqwest::Method::GET,
                &self.object_path(key),
                &[],
                EMPTY_SHA256,
                None,
            )
            .await?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(out)
            .with_context(|| format!("creating {}", out.display()))?;
        let mut file = tokio::fs::File::from_std(file);
        let mut written = 0u64;
        while let Some(chunk) = res.chunk().await.context("downloading backup")? {
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(written)
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.send(
            reqwest::Method::DELETE,
            &self.object_path(key),
            &[],
            EMPTY_SHA256,
            None,
        )
        .await?;
        Ok(())
    }
}

struct SigningInput<'a> {
    method: &'a str,
    /// Already URI-encoded path.
    path: &'a str,
    query: &'a [(&'a str, &'a str)],
    /// Lowercase header names with values; all are signed.
    headers: &'a [(String, String)],
    payload_hash: &'a str,
    region: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
    now: DateTime<Utc>,
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    // HMAC accepts keys of any length, so this cannot fail; if it ever did, the empty
    // result yields a signature S3 rejects (fail closed).
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data.as_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// AWS Signature Version 4 `Authorization` header value for service `s3`.
fn sign_v4(input: &SigningInput<'_>) -> String {
    let amz_date = input.now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = input.now.format("%Y%m%d").to_string();
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.path,
        canonical_query(input.query),
        canonical_headers,
        signed_headers,
        input.payload_hash
    );
    let scope = format!("{date}/{}/s3/aws4_request", input.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{}", input.secret_key).as_bytes(), &date);
    let k_region = hmac(&k_date, input.region);
    let k_service = hmac(&k_region, "s3");
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex::encode(hmac(&k_signing, &string_to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        input.access_key
    )
}

fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// RFC 3986 encoding as SigV4 requires: unreserved characters kept, `/` kept in paths.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].to_string())
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Extract `<Contents><Key>…</Key>…<Size>…</Size></Contents>` entries.
fn parse_list(xml: &str) -> Vec<ObjectInfo> {
    xml.split("<Contents>")
        .skip(1)
        .filter_map(|chunk| {
            let chunk = chunk.split("</Contents>").next()?;
            Some(ObjectInfo {
                key: xml_unescape(&xml_tag(chunk, "Key")?),
                size: xml_tag(chunk, "Size")?.parse().ok()?,
            })
        })
        .collect()
}

fn s3_error_message(xml: &str) -> String {
    match (xml_tag(xml, "Code"), xml_tag(xml, "Message")) {
        (Some(code), Some(msg)) => format!("{code}: {msg}"),
        (Some(code), None) => code,
        _ => xml.chars().take(200).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn example_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// "Example: GET Object" from the AWS S3 Signature Version 4 documentation.
    #[test]
    fn aws_example_get_object() {
        let auth = sign_v4(&SigningInput {
            method: "GET",
            path: "/test.txt",
            query: &[],
            headers: &headers(&[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ]),
            payload_hash: EMPTY_SHA256,
            region: "us-east-1",
            access_key: ACCESS,
            secret_key: SECRET,
            now: example_time(),
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// "Example: List Objects" (query parameters) from the same documentation.
    #[test]
    fn aws_example_list_objects() {
        let auth = sign_v4(&SigningInput {
            method: "GET",
            path: "/",
            query: &[("max-keys", "2"), ("prefix", "J")],
            headers: &headers(&[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ]),
            payload_hash: EMPTY_SHA256,
            region: "us-east-1",
            access_key: ACCESS,
            secret_key: SECRET,
            now: example_time(),
        });
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
    }

    #[test]
    fn encoding_and_endpoints() {
        assert_eq!(
            uri_encode("backups/a b/c+d.tar.gz", false),
            "backups/a%20b/c%2Bd.tar.gz"
        );
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(
            canonical_query(&[("prefix", "x/"), ("list-type", "2")]),
            "list-type=2&prefix=x%2F"
        );
        assert!(parse_endpoint("https://s3.us-east-1.amazonaws.com").is_ok());
        assert_eq!(
            parse_endpoint("http://127.0.0.1:9000/").unwrap().1,
            "127.0.0.1:9000"
        );
        for bad in [
            "s3.amazonaws.com",
            "ftp://x",
            "https://x/path",
            "https://user@x",
            "https://",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_list_responses() {
        let xml = "<ListBucketResult><IsTruncated>true</IsTruncated>\
            <Contents><Key>backups/a/1.dump</Key><Size>10</Size></Contents>\
            <Contents><Key>backups/a/2 &amp; b.dump</Key><Size>20</Size></Contents>\
            <NextContinuationToken>abc</NextContinuationToken></ListBucketResult>";
        assert_eq!(
            parse_list(xml),
            vec![
                ObjectInfo {
                    key: "backups/a/1.dump".into(),
                    size: 10
                },
                ObjectInfo {
                    key: "backups/a/2 & b.dump".into(),
                    size: 20
                },
            ]
        );
        assert_eq!(
            xml_tag(xml, "NextContinuationToken").as_deref(),
            Some("abc")
        );
        assert_eq!(
            s3_error_message("<Error><Code>AccessDenied</Code><Message>nope</Message></Error>"),
            "AccessDenied: nope"
        );
    }
}
