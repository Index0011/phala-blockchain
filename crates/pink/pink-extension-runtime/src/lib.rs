use std::borrow::Cow;
use std::{fmt::Display, str::FromStr, time::Duration};

use pink_extension::chain_extension::{
    HttpRequest, HttpResponse, PinkExtBackend, PublicKeyForArgs, SigType, SignArgs,
    StorageQuotaExceeded, VerifyArgs,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method,
};
use reqwest_env_proxy::EnvProxyBuilder;
use sp_core::{ByteArray as _, Pair};

pub mod mock_ext;

pub trait PinkRuntimeEnv {
    type AccountId: AsRef<[u8]> + Display;

    fn address(&self) -> &Self::AccountId;
    fn call_elapsed(&self) -> Option<Duration>;
}

pub struct DefaultPinkExtension<'a, T, Error> {
    pub env: &'a T,
    _e: std::marker::PhantomData<Error>,
}

impl<'a, T, E> DefaultPinkExtension<'a, T, E> {
    pub fn new(env: &'a T) -> Self {
        Self {
            env,
            _e: std::marker::PhantomData,
        }
    }
}

impl<T: PinkRuntimeEnv, E: From<&'static str>> PinkExtBackend for DefaultPinkExtension<'_, T, E> {
    type Error = E;
    fn http_request(&self, request: HttpRequest) -> Result<HttpResponse, Self::Error> {
        // Hardcoded limitations for now
        const MAX_QUERY_TIME: u64 = 10; // seconds
        const MAX_BODY_SIZE: usize = 1024 * 256; // 256KB

        let elapsed = self.env.call_elapsed().ok_or("Invalid exec env")?;
        let timeout = Duration::from_secs(MAX_QUERY_TIME) - elapsed;

        let url: reqwest::Url = request.url.parse().or(Err("Invalid url"))?;

        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .env_proxy(url.host_str().unwrap_or_default())
            .build()
            .or(Err("Failed to create client"))?;

        let method: Method =
            FromStr::from_str(request.method.as_str()).or(Err("Invalid HTTP method"))?;
        let mut headers = HeaderMap::new();
        for (key, value) in &request.headers {
            let key = HeaderName::from_str(key.as_str()).or(Err("Invalid HTTP header key"))?;
            let value = HeaderValue::from_str(value).or(Err("Invalid HTTP header value"))?;
            headers.insert(key, value);
        }

        let mut response = client
            .request(method, url)
            .headers(headers)
            .body(request.body)
            .send()
            .map_err(|err| {
                log::info!("HTTP request error: {}", err);
                "Failed to send request"
            })?;

        let headers: Vec<_> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().into()))
            .collect();

        let mut body = Vec::new();
        let mut writer = LimitedWriter::new(&mut body, MAX_BODY_SIZE);

        response
            .copy_to(&mut writer)
            .or(Err("Failed to copy response body"))?;

        let response = HttpResponse {
            status_code: response.status().as_u16(),
            reason_phrase: response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .into(),
            body,
            headers,
        };
        Ok(response)
    }

    fn sign(&self, args: SignArgs) -> Result<Vec<u8>, Self::Error> {
        macro_rules! sign_with {
            ($sigtype:ident) => {{
                let pair =
                    sp_core::$sigtype::Pair::from_seed_slice(&args.key).or(Err("Invalid key"))?;
                let signature = pair.sign(&args.message);
                let signature: &[u8] = signature.as_ref();
                signature.to_vec()
            }};
        }

        Ok(match args.sigtype {
            SigType::Sr25519 => sign_with!(sr25519),
            SigType::Ed25519 => sign_with!(ed25519),
            SigType::Ecdsa => sign_with!(ecdsa),
        })
    }

    fn verify(&self, args: VerifyArgs) -> Result<bool, Self::Error> {
        macro_rules! verify_with {
            ($sigtype:ident) => {{
                sp_core::$sigtype::Pair::verify_weak(&args.signature, &args.message, &args.pubkey)
            }};
        }
        Ok(match args.sigtype {
            SigType::Sr25519 => verify_with!(sr25519),
            SigType::Ed25519 => verify_with!(ed25519),
            SigType::Ecdsa => verify_with!(ecdsa),
        })
    }

    fn derive_sr25519_key(&self, salt: Cow<[u8]>) -> Result<Vec<u8>, Self::Error> {
        // This default implementation is for unit tests. The host should override this.
        let mut seed: <sp_core::sr25519::Pair as Pair>::Seed = Default::default();
        let len = seed.len().min(salt.len());
        seed[..len].copy_from_slice(&salt[..len]);
        let key = sp_core::sr25519::Pair::from_seed(&seed);

        Ok(key.as_ref().secret.to_bytes().to_vec())
    }

    fn get_public_key(&self, args: PublicKeyForArgs) -> Result<Vec<u8>, Self::Error> {
        macro_rules! public_key_with {
            ($sigtype:ident) => {{
                sp_core::$sigtype::Pair::from_seed_slice(&args.key)
                    .or(Err("Invalid key"))?
                    .public()
                    .to_raw_vec()
            }};
        }
        let pubkey = match args.sigtype {
            SigType::Ed25519 => public_key_with!(ed25519),
            SigType::Sr25519 => public_key_with!(sr25519),
            SigType::Ecdsa => public_key_with!(ecdsa),
        };
        Ok(pubkey)
    }

    fn cache_set(
        &self,
        _key: Cow<[u8]>,
        _value: Cow<[u8]>,
    ) -> Result<Result<(), StorageQuotaExceeded>, Self::Error> {
        Ok(Ok(()))
    }

    fn cache_set_expire(&self, _key: Cow<[u8]>, _expire: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn cache_get(&self, _key: Cow<'_, [u8]>) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(None)
    }

    fn cache_remove(&self, _key: Cow<'_, [u8]>) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(None)
    }

    fn log(&self, level: u8, message: Cow<str>) -> Result<(), Self::Error> {
        let address = self.env.address();
        let level = match level {
            1 => log::Level::Error,
            2 => log::Level::Warn,
            3 => log::Level::Info,
            4 => log::Level::Debug,
            5 => log::Level::Trace,
            _ => log::Level::Error,
        };
        log::log!(target: "pink", level, "[{}] {}", address, message);
        Ok(())
    }

    fn getrandom(&self, length: u8) -> Result<Vec<u8>, Self::Error> {
        let mut buf = vec![0u8; length as _];
        getrandom::getrandom(&mut buf[..]).or(Err("Failed to get random bytes"))?;
        Ok(buf)
    }

    fn is_running_in_command(&self) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

struct LimitedWriter<W> {
    writer: W,
    written: usize,
    limit: usize,
}

impl<W> LimitedWriter<W> {
    fn new(writer: W, limit: usize) -> Self {
        Self {
            writer,
            written: 0,
            limit,
        }
    }
}

impl<W: std::io::Write> std::io::Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Buffer limit exceeded",
            ));
        }
        let wlen = self.writer.write(buf)?;
        self.written += wlen;
        Ok(wlen)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}
