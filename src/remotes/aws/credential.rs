use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::str::FromStr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::header::{self, AUTHORIZATION};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Body;
use hyper::rt::{Executor, Read, Write};
use percent_encoding::utf8_percent_encode;
use thiserror::Error;
use url::Url;

use crate::remotes::aws::{STRICT_ENCODE_SET, STRICT_PATH_ENCODE_SET};
use crate::remotes::http::{Client, HttpError, HyperClient};

const EMPTY_SHA256_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const STREAMING_PAYLOAD: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";

#[derive(Debug)]
pub struct AwsCredential<'c> {
    /// AWS_ACCESS_KEY_ID
    pub key_id: &'c str,
    /// AWS_SECRET_ACCESS_KEY
    pub secret_key: &'c str,
    /// AWS_SESSION_TOKEN
    pub token: Option<&'c str>,
}

impl<'c> AwsCredential<'c> {
    /// Signs a string
    ///
    /// <https://docs.aws.amazon.com/general/latest/gr/sigv4-calculate-signature.html>
    fn sign(
        &self,
        to_sign: &'c str,
        date: DateTime<Utc>,
        region: &'c str,
        service: &'c str,
    ) -> String {
        let date_string = date.format("%Y%m%d").to_string();
        let date_hmac = hmac_sha256(format!("AWS4{}", self.secret_key), date_string);
        let region_hmac = hmac_sha256(date_hmac, region);
        let service_hmac = hmac_sha256(region_hmac, service);
        let signing_hmac = hmac_sha256(service_hmac, b"aws4_request");
        hex_encode(hmac_sha256(signing_hmac, to_sign).as_ref())
    }
}

fn hmac_sha256(secret: impl AsRef<[u8]>, bytes: impl AsRef<[u8]>) -> ring::hmac::Tag {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_ref());
    ring::hmac::sign(&key, bytes.as_ref())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // String writing is infallible
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Authorize a [`Request`] with an [`AwsCredential`] using [AWS SigV4]
///
/// [AWS SigV4]: https://docs.aws.amazon.com/general/latest/gr/sigv4-calculate-signature.html
#[derive(Debug)]
pub struct AwsAuthorizer<'a> {
    date: Option<DateTime<Utc>>,
    credential: &'a AwsCredential<'a>,
    service: &'a str,
    region: &'a str,
    token_header: Option<HeaderName>,
    sign_payload: bool,
}

static DATE_HEADER: HeaderName = HeaderName::from_static("x-amz-date");
static HASH_HEADER: HeaderName = HeaderName::from_static("x-amz-content-sha256");
static TOKEN_HEADER: HeaderName = HeaderName::from_static("x-amz-security-token");
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

impl<'a> AwsAuthorizer<'a> {
    /// Create a new [`AwsAuthorizer`]
    pub fn new(credential: &'a AwsCredential, service: &'a str, region: &'a str) -> Self {
        Self {
            credential,
            service,
            region,
            date: None,
            sign_payload: true,
            token_header: None,
        }
    }

    /// Controls whether this [`AwsAuthorizer`] will attempt to sign the request payload,
    /// the default is `true`
    pub fn with_sign_payload(mut self, signed: bool) -> Self {
        self.sign_payload = signed;
        self
    }

    /// Overrides the header name for security tokens, defaults to `x-amz-security-token`
    pub(crate) fn with_token_header(mut self, header: HeaderName) -> Self {
        self.token_header = Some(header);
        self
    }

    /// Authorize `request` with an optional pre-calculated SHA256 digest by attaching
    /// the relevant [AWS SigV4] headers
    ///
    /// # Payload Signature
    ///
    /// AWS SigV4 requests must contain the `x-amz-content-sha256` header, it is set as follows:
    ///
    /// * If not configured to sign payloads, it is set to `UNSIGNED-PAYLOAD`
    /// * If a `pre_calculated_digest` is provided, it is set to the hex encoding of it
    /// * If it is a streaming request, it is set to `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`
    /// * Otherwise it is set to the hex encoded SHA256 of the request body
    ///
    /// [AWS SigV4]: https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
    async fn authorize<
        B: Body<Data = Bytes, Error: std::error::Error + Send + Sync + 'static> + Unpin,
    >(
        &self,
        request: &mut Request<B>,
        pre_calculated_digest: Option<&[u8]>,
    ) -> Result<(), AutohrizeError> {
        if let Some(token) = self.credential.token {
            let token_val = HeaderValue::from_str(token)?;
            let header = self.token_header.as_ref().unwrap_or(&TOKEN_HEADER);
            request.headers_mut().insert(header, token_val);
        }

        let host = request
            .uri()
            .authority()
            .ok_or(AutohrizeError::NoHost)?
            .as_str();
        let host_val = HeaderValue::from_str(host)?;
        request.headers_mut().insert("host", host_val);

        let date = self.date.unwrap_or_else(Utc::now);
        let date_str = date.format("%Y%m%dT%H%M%SZ").to_string();
        let date_val = HeaderValue::from_str(&date_str)?;
        request.headers_mut().insert(&DATE_HEADER, date_val);

        let digest = match self.sign_payload {
            false => UNSIGNED_PAYLOAD.to_string(),
            true => match pre_calculated_digest {
                Some(digest) => hex_encode(digest),
                None => match request.body().size_hint().exact() {
                    Some(n) => match n {
                        0 => EMPTY_SHA256_HASH.to_string(),
                        _ => {
                            let bytes = request
                                .body_mut()
                                .frame()
                                .await
                                .ok_or(AutohrizeError::BodyNoFrame)?
                                .map_err(|e| {
                                    Box::new(e)
                                        as Box<dyn std::error::Error + Send + Sync + 'static>
                                })?;
                            hex_digest(bytes.data_ref().ok_or(AutohrizeError::BodyNoFrame)?)
                        }
                    },
                    None => STREAMING_PAYLOAD.to_string(),
                },
            },
        };

        let header_digest = HeaderValue::from_str(&digest)?;
        request.headers_mut().insert(&HASH_HEADER, header_digest);

        let (signed_headers, canonical_headers) = canonicalize_headers(request.headers());

        let scope = self.scope(date);

        let string_to_sign = self.string_to_sign(
            date,
            &scope,
            request.method(),
            &Url::parse(&request.uri().to_string())?,
            &canonical_headers,
            &signed_headers,
            &digest,
        );

        // sign the string
        let signature = self
            .credential
            .sign(&string_to_sign, date, self.region, self.service);

        // build the actual auth header
        let authorisation = format!(
            "{} Credential={}/{}, SignedHeaders={}, Signature={}",
            ALGORITHM, self.credential.key_id, scope, signed_headers, signature
        );

        let authorization_val = HeaderValue::from_str(&authorisation)?;
        request
            .headers_mut()
            .insert(&AUTHORIZATION, authorization_val);

        Ok(())
    }

    pub(crate) fn sign(&self, method: Method, url: &mut Url, expires_in: Duration) {
        let date = self.date.unwrap_or_else(Utc::now);
        let scope = self.scope(date);

        // https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-query-string-auth.html
        url.query_pairs_mut()
            .append_pair("X-Amz-Algorithm", ALGORITHM)
            .append_pair(
                "X-Amz-Credential",
                &format!("{}/{}", self.credential.key_id, scope),
            )
            .append_pair("X-Amz-Date", &date.format("%Y%m%dT%H%M%SZ").to_string())
            .append_pair("X-Amz-Expires", &expires_in.as_secs().to_string())
            .append_pair("X-Amz-SignedHeaders", "host");

        // For S3, you must include the X-Amz-Security-Token query parameter in the URL if
        // using credentials sourced from the STS service.
        if let Some(token) = self.credential.token {
            url.query_pairs_mut()
                .append_pair("X-Amz-Security-Token", token);
        }

        // We don't have a payload; the user is going to send the payload directly themselves.
        let digest = UNSIGNED_PAYLOAD;

        let host = &url[url::Position::BeforeHost..url::Position::AfterPort].to_string();
        let mut headers = HeaderMap::new();
        let host_val = HeaderValue::from_str(host).unwrap();
        headers.insert("host", host_val);

        let (signed_headers, canonical_headers) = canonicalize_headers(&headers);

        let string_to_sign = self.string_to_sign(
            date,
            &scope,
            &method,
            url,
            &canonical_headers,
            &signed_headers,
            digest,
        );

        let signature = self
            .credential
            .sign(&string_to_sign, date, self.region, self.service);

        url.query_pairs_mut()
            .append_pair("X-Amz-Signature", &signature);
    }

    #[allow(clippy::too_many_arguments)]
    fn string_to_sign(
        &self,
        date: DateTime<Utc>,
        scope: &str,
        request_method: &Method,
        url: &Url,
        canonical_headers: &str,
        signed_headers: &str,
        digest: &str,
    ) -> String {
        // Each path segment must be URI-encoded twice (except for Amazon S3 which only gets
        // URI-encoded once).
        // see https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html
        let canonical_uri = match self.service {
            "s3" => url.path().to_string(),
            _ => utf8_percent_encode(url.path(), &STRICT_PATH_ENCODE_SET).to_string(),
        };

        let canonical_query = canonicalize_query(url);

        // https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            request_method.as_str(),
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            digest
        );

        let hashed_canonical_request = hex_digest(canonical_request.as_bytes());

        format!(
            "{}\n{}\n{}\n{}",
            ALGORITHM,
            date.format("%Y%m%dT%H%M%SZ"),
            scope,
            hashed_canonical_request
        )
    }

    fn scope(&self, date: DateTime<Utc>) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            date.format("%Y%m%d"),
            self.region,
            self.service
        )
    }
}

/// Canonicalizes headers into the AWS Canonical Form.
///
/// <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
fn canonicalize_headers(header_map: &HeaderMap) -> (String, String) {
    let mut headers = BTreeMap::<&str, Vec<&str>>::new();
    let mut value_count = 0;
    let mut value_bytes = 0;
    let mut key_bytes = 0;

    for (key, value) in header_map {
        let key = key.as_str();
        if ["authorization", "content-length", "user-agent"].contains(&key) {
            continue;
        }

        let value = std::str::from_utf8(value.as_bytes()).unwrap();
        key_bytes += key.len();
        value_bytes += value.len();
        value_count += 1;
        headers.entry(key).or_default().push(value);
    }

    let mut signed_headers = String::with_capacity(key_bytes + headers.len());
    let mut canonical_headers =
        String::with_capacity(key_bytes + value_bytes + headers.len() + value_count);

    for (header_idx, (name, values)) in headers.into_iter().enumerate() {
        if header_idx != 0 {
            signed_headers.push(';');
        }

        signed_headers.push_str(name);
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        for (value_idx, value) in values.into_iter().enumerate() {
            if value_idx != 0 {
                canonical_headers.push(',');
            }
            canonical_headers.push_str(value.trim());
        }
        canonical_headers.push('\n');
    }

    (signed_headers, canonical_headers)
}

/// Canonicalizes query parameters into the AWS canonical form
///
/// <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
fn canonicalize_query(url: &Url) -> String {
    use std::fmt::Write;

    let capacity = match url.query() {
        Some(q) if !q.is_empty() => q.len(),
        _ => return String::new(),
    };
    let mut encoded = String::with_capacity(capacity + 1);

    let mut headers = url.query_pairs().collect::<Vec<_>>();
    headers.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    let mut first = true;
    for (k, v) in headers {
        if !first {
            encoded.push('&');
        }
        first = false;
        let _ = write!(
            encoded,
            "{}={}",
            utf8_percent_encode(k.as_ref(), &STRICT_ENCODE_SET),
            utf8_percent_encode(v.as_ref(), &STRICT_ENCODE_SET)
        );
    }
    encoded
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    hex_encode(digest.as_ref())
}

#[derive(Debug, Error)]
enum AutohrizeError {
    #[error("Invalid header value: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("No host in URL")]
    NoHost,
    #[error("Body frame error: {0}")]
    BodyFrameError(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("Body no frame")]
    BodyNoFrame,
}

/// <https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/iam-roles-for-amazon-ec2.html#instance-metadata-security-credentials>
async fn instance_creds<'c, C: Client>(
    client: &'c HyperClient<C>,
    endpoint: &'c str,
    imdsv1_fallback: bool,
) -> Result<(AwsCredential<'c>, Option<Instant>), HttpError> {
    const CREDENTIALS_PATH: &str = "latest/meta-data/iam/security-credentials";
    const AWS_EC2_METADATA_TOKEN_HEADER: &str = "X-aws-ec2-metadata-token";

    let token_url = format!("{endpoint}/latest/api/token");

    let request = Request::builder()
        .method(Method::PUT)
        .uri(token_url)
        .header("host", endpoint)
        .header("X-aws-ec2-metadata-token-ttl-seconds", "600")
        .body(Empty::<Bytes>::new())?;

    let token_result = client.send(request).await?;

    let token = match token_result.status() {
        StatusCode::OK => Some(token_result.collect().await?.to_bytes()),
        StatusCode::FORBIDDEN if imdsv1_fallback => None,
        _ => {
            return Err(HttpError::Io(io::Error::new(
                io::ErrorKind::Other,
                "Invalid token",
            )))
        }
    };

    let role_url = format!("{endpoint}/{CREDENTIALS_PATH}/");
    let mut role_request = Request::builder()
        .method(Method::GET)
        .uri(role_url)
        .header("host", endpoint);

    if let Some(token) = &token {
        role_request = role_request.header(
            AWS_EC2_METADATA_TOKEN_HEADER,
            String::from_utf8(token.to_vec()).unwrap(),
        );
    }

    let role = client
        .send(role_request.body(Empty::<Bytes>::new())?)
        .await?
        .collect()
        .await?
        .to_bytes();

    // let creds_url = format!("{endpoint}/{CREDENTIALS_PATH}/{role}");
    // let mut creds_request = client.request(Method::GET, creds_url);
    // if let Some(token) = &token {
    //     creds_request = creds_request.header(AWS_EC2_METADATA_TOKEN_HEADER, token);
    // }

    // let creds: InstanceCredentials = creds_request.send_retry(retry_config).await?.json().await?;

    // let now = Utc::now();
    // let ttl = (creds.expiration - now).to_std().unwrap_or_default();
    // Ok(TemporaryToken {
    //     token: Arc::new(creds.into()),
    //     expiry: Some(Instant::now() + ttl),
    // })

    todo!()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::time::Duration;

    use bytes::Bytes;
    use chrono::{DateTime, Utc};
    use http::header::AUTHORIZATION;
    use http::{Method, Request, StatusCode};
    use http_body_util::Empty;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpStream;
    use url::Url;

    use crate::remotes::aws::credential::{AwsAuthorizer, AwsCredential};

    // Test generated using https://docs.aws.amazon.com/general/latest/gr/sigv4-signed-request-examples.html
    #[tokio::test]
    async fn test_sign_with_signed_payload() {
        // Test credentials from https://docs.aws.amazon.com/AmazonS3/latest/userguide/RESTAuthentication.html
        let credential = AwsCredential {
            key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            token: None,
        };

        // method = 'GET'
        // service = 'ec2'
        // host = 'ec2.amazonaws.com'
        // region = 'us-east-1'
        // endpoint = 'https://ec2.amazonaws.com'
        // request_parameters = ''
        let date = DateTime::parse_from_rfc3339("2022-08-06T18:01:34Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut request = Request::builder()
            .uri("https://ec2.amazon.com/")
            .method(Method::GET)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let signer = AwsAuthorizer {
            date: Some(date),
            credential: &credential,
            service: "ec2",
            region: "us-east-1",
            sign_payload: true,
            token_header: None,
        };

        signer.authorize(&mut request, None).await.unwrap();
        assert_eq!(request.headers().get(&AUTHORIZATION).unwrap(), "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20220806/us-east-1/ec2/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=a3c787a7ed37f7fdfbfd2d7056a3d7c9d85e6d52a2bfbec73793c0be6e7862d4")
    }

    #[tokio::test]
    async fn test_sign_with_unsigned_payload() {
        // Test credentials from https://docs.aws.amazon.com/AmazonS3/latest/userguide/RESTAuthentication.html
        let credential = AwsCredential {
            key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            token: None,
        };

        // method = 'GET'
        // service = 'ec2'
        // host = 'ec2.amazonaws.com'
        // region = 'us-east-1'
        // endpoint = 'https://ec2.amazonaws.com'
        // request_parameters = ''
        let date = DateTime::parse_from_rfc3339("2022-08-06T18:01:34Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut request = Request::builder()
            .uri("https://ec2.amazon.com/")
            .method(Method::GET)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let authorizer = AwsAuthorizer {
            date: Some(date),
            credential: &credential,
            service: "ec2",
            region: "us-east-1",
            token_header: None,
            sign_payload: false,
        };

        authorizer.authorize(&mut request, None).await.unwrap();
        assert_eq!(request.headers().get(&AUTHORIZATION).unwrap(), "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20220806/us-east-1/ec2/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=653c3d8ea261fd826207df58bc2bb69fbb5003e9eb3c0ef06e4a51f2a81d8699");
    }

    #[test]
    fn signed_get_url() {
        // Values from https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-query-string-auth.html
        let credential = AwsCredential {
            key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            token: None,
        };

        let date = DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let authorizer = AwsAuthorizer {
            date: Some(date),
            credential: &credential,
            service: "s3",
            region: "us-east-1",
            token_header: None,
            sign_payload: false,
        };

        let mut url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        authorizer.sign(Method::GET, &mut url, Duration::from_secs(86400));

        assert_eq!(
            url,
            Url::parse(
                "https://examplebucket.s3.amazonaws.com/test.txt?\
                X-Amz-Algorithm=AWS4-HMAC-SHA256&\
                X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&\
                X-Amz-Date=20130524T000000Z&\
                X-Amz-Expires=86400&\
                X-Amz-SignedHeaders=host&\
                X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn test_sign_port() {
        let credential = AwsCredential {
            key_id: "H20ABqCkLZID4rLe",
            secret_key: "jMqRDgxSsBqqznfmddGdu1TmmZOJQxdM",
            token: None,
        };

        let date = DateTime::parse_from_rfc3339("2022-08-09T13:05:25Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut request = Request::builder()
            .uri("http://localhost:9000/tsm-schemas?delimiter=%2F&encoding-type=url&list-type=2&prefix=")
            .method(Method::GET)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let authorizer = AwsAuthorizer {
            date: Some(date),
            credential: &credential,
            service: "s3",
            region: "us-east-1",
            token_header: None,
            sign_payload: true,
        };

        authorizer.authorize(&mut request, None).await.unwrap();
        assert_eq!(request.headers().get(&AUTHORIZATION).unwrap(), "AWS4-HMAC-SHA256 Credential=H20ABqCkLZID4rLe/20220809/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=9ebf2f92872066c99ac94e573b4e1b80f4dbb8a32b1e8e23178318746e7d1b4d")
    }

    #[tokio::test]
    async fn test_instance_metadata() {
        if env::var("TEST_INTEGRATION").is_err() {
            eprintln!("skipping AWS integration test");
            return;
        }

        // For example https://github.com/aws/amazon-ec2-metadata-mock
        let endpoint = env::var("EC2_METADATA_ENDPOINT").unwrap();
        println!("{:?}", format!("{endpoint}/latest/meta-data/ami-id"));

        let stream = TcpStream::connect(endpoint.clone()).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        let request = Request::builder()
            .uri(format!("http://{endpoint}/latest/meta-data/ami-id"))
            .method(Method::GET)
            .header("host", endpoint)
            .body(Empty::<Bytes>::new())
            .unwrap();

        let resp = sender.send_request(request).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "Ensure metadata endpoint is set to only allow IMDSv2"
        );

        // let creds = instance_creds(&client, &retry_config, &endpoint, false)
        //     .await
        //     .unwrap();

        // let id = &creds.token.key_id;
        // let secret = &creds.token.secret_key;
        // let token = creds.token.token.as_ref().unwrap();

        // assert!(!id.is_empty());
        // assert!(!secret.is_empty());
        // assert!(!token.is_empty())
    }
}