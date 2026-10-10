mod batch;
pub use batch::LuaBatch;
mod response;
use mlua::prelude::*;
use reqwest::cookie::CookieStore as ReqwestCookieStore;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
pub use response::LuaResponse;
use std::collections::HashMap;
use std::convert::Infallible;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

fn header_map_to_table(lua: &Lua, headers: &HeaderMap) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            t.set(name.as_str(), v)?;
        }
    }
    Ok(t)
}

fn table_to_header_map(t: &LuaTable) -> LuaResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for pair in t.pairs::<String, String>() {
        let (k, v) = pair?;
        map.insert(
            HeaderName::from_str(&k).map_err(mlua::Error::external)?,
            v.parse().map_err(mlua::Error::external)?,
        );
    }
    Ok(map)
}

fn table_to_multipart_form(t: &LuaTable) -> LuaResult<reqwest::multipart::Form> {
    let mut form = reqwest::multipart::Form::new();
    for pair in t.pairs::<String, LuaValue>() {
        let (key, value) = pair?;
        let value = match value {
            LuaValue::String(value) => value.to_str()?.to_owned(),
            LuaValue::Integer(value) => value.to_string(),
            LuaValue::Number(value) => value.to_string(),
            LuaValue::Boolean(value) => value.to_string(),
            _ => return Err(mlua::Error::runtime("multipart values must be scalar")),
        };
        form = form.text(key, value);
    }
    Ok(form)
}

pub trait CookieHeaderProvider: Send + Sync {
    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue>;
}

#[derive(Debug)]
pub struct SessionCookieStore {
    inner: RwLock<cookie_store::CookieStore>,
    revision: AtomicU64,
    clean_revision: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCookieSnapshot {
    pub value: String,
    pub revision: u64,
}

impl Default for SessionCookieStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(cookie_store::CookieStore::default()),
            revision: AtomicU64::new(0),
            clean_revision: AtomicU64::new(0),
        }
    }
}

impl SessionCookieStore {
    fn read(&self) -> RwLockReadGuard<'_, cookie_store::CookieStore> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, cookie_store::CookieStore> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn advance_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn is_dirty(&self) -> bool {
        self.revision() != self.clean_revision.load(Ordering::Acquire)
    }

    pub fn mark_clean(&self, revision: u64) -> bool {
        self.clean_revision.store(revision, Ordering::Release);
        self.revision() == revision
    }

    pub fn snapshot(&self) -> Result<SessionCookieSnapshot, cookie_store::Error> {
        let store = self.read();
        let revision = self.revision();
        let unexpired = cookie_store::CookieStore::from_cookies(
            store.iter_unexpired().cloned().map(Ok::<_, Infallible>),
            false,
        )
        .expect("infallible cookie snapshot copy");
        let mut value = Vec::new();
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&unexpired, &mut value)?;
        Ok(SessionCookieSnapshot {
            value: String::from_utf8(value).expect("cookie_store JSON is UTF-8"),
            revision,
        })
    }

    pub fn restore(&self, snapshot: &str) -> Result<(), cookie_store::Error> {
        let restored = cookie_store::serde::json::load(snapshot.as_bytes())?;
        let mut store = self.write();
        *store = restored;
        let revision = self.advance_revision();
        self.clean_revision.store(revision, Ordering::Release);
        Ok(())
    }

    pub fn cookie(&self, url: &reqwest::Url, name: &str) -> Option<String> {
        self.read()
            .get_request_values(url)
            .find_map(|(cookie_name, value)| (cookie_name == name).then(|| value.to_owned()))
    }

    pub fn insert(
        &self,
        url: &reqwest::Url,
        cookie: &str,
    ) -> Result<(), cookie_store::CookieError> {
        let mut store = self.write();
        store.parse(cookie, url)?;
        self.advance_revision();
        Ok(())
    }

    pub fn clear(&self) {
        let mut store = self.write();
        if store.iter_any().next().is_some() {
            store.clear();
            self.advance_revision();
        }
    }
}

impl ReqwestCookieStore for SessionCookieStore {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        let mut store = self.write();
        let mut changed = false;
        for cookie in cookie_headers.filter_map(|header| {
            let value = header.to_str().ok()?;
            cookie_store::RawCookie::parse(value.to_owned())
                .ok()
                .map(cookie_store::RawCookie::into_owned)
        }) {
            changed |= store.insert_raw(&cookie, url).is_ok();
        }
        if changed {
            self.advance_revision();
        }
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        let value = self
            .read()
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        (!value.is_empty())
            .then(|| HeaderValue::from_str(&value).ok())
            .flatten()
    }
}

impl CookieHeaderProvider for SessionCookieStore {
    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        ReqwestCookieStore::cookies(self, url)
    }
}

#[derive(Clone)]
pub struct LuaHttpClient {
    client: reqwest::Client,
    cookie_provider: Option<Arc<dyn CookieHeaderProvider>>,
    session_store: Option<Arc<SessionCookieStore>>,
    stream_client: Option<reqwest::Client>,
}

impl LuaHttpClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            cookie_provider: None,
            session_store: None,
            stream_client: None,
        }
    }

    pub fn with_cookie_provider(
        client: reqwest::Client,
        cookie_provider: Arc<dyn CookieHeaderProvider>,
    ) -> Self {
        Self {
            client,
            cookie_provider: Some(cookie_provider),
            session_store: None,
            stream_client: None,
        }
    }

    pub fn with_session(client: reqwest::Client, store: Arc<SessionCookieStore>) -> Self {
        let cookie_provider: Arc<dyn CookieHeaderProvider> = store.clone();
        Self {
            client,
            cookie_provider: Some(cookie_provider),
            session_store: Some(store),
            stream_client: None,
        }
    }

    /// The supplied client must have no total request timeout and share the host's network policy.
    pub fn with_stream_client(mut self, client: reqwest::Client) -> Self {
        self.stream_client = Some(client);
        self
    }

    pub fn cookie(&self, url: &str, name: &str) -> LuaResult<Option<String>> {
        let url = reqwest::Url::parse(url).map_err(mlua::Error::external)?;
        Ok(self
            .session_store
            .as_ref()
            .and_then(|store| store.cookie(&url, name)))
    }

    pub fn set_cookie(&self, url: &str, cookie: &str) -> LuaResult<()> {
        let url = reqwest::Url::parse(url).map_err(mlua::Error::external)?;
        let store = self
            .session_store
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("HTTP client has no cookie store"))?;
        store.insert(&url, cookie).map_err(mlua::Error::external)
    }

    pub fn clear_cookies(&self) {
        if let Some(store) = &self.session_store {
            store.clear();
        }
    }

    fn prepare_description(&self, desc: LuaTable) -> LuaResult<PreparedRequest> {
        for pair in desc.clone().pairs::<String, LuaValue>() {
            let (key, _) = pair?;
            if !matches!(
                key.as_str(),
                "method"
                    | "url"
                    | "headers"
                    | "header_pairs"
                    | "query"
                    | "query_pairs"
                    | "body"
                    | "timeout"
                    | "json_codec"
                    | "response_limit"
                    | "stream"
            ) {
                return Err(http_error(
                    "invalid_argument",
                    format!("unknown request field: {key}"),
                ));
            }
        }
        let mut builder = self.request(&desc.get::<String>("method")?, desc.get("url")?)?;
        for (map, pairs) in [("query", "query_pairs"), ("headers", "header_pairs")] {
            if desc.contains_key(map)? && desc.contains_key(pairs)? {
                return Err(http_error(
                    "invalid_argument",
                    "map and pairs forms are mutually exclusive",
                ));
            }
        }
        if let Some(codec) = desc.get::<Option<String>>("json_codec")? {
            builder.options.strict = parse_codec(&codec)?;
        }
        builder.options.limit = desc.get("response_limit")?;
        if let Some(headers) = desc.get::<Option<LuaTable>>("headers")? {
            let headers = table_to_header_map(&headers)?;
            builder.map(|b| b.headers(headers))?;
        }
        if let Some(pairs) = desc.get::<Option<LuaTable>>("header_pairs")? {
            let mut headers = Vec::new();
            for (k, v) in byte_pairs(&pairs)? {
                headers.push((
                    HeaderName::from_bytes(&k.as_bytes()).map_err(LuaError::external)?,
                    HeaderValue::from_bytes(&v.as_bytes()).map_err(LuaError::external)?,
                ));
            }
            builder.map(|mut b| {
                for (k, v) in headers {
                    b = b.header(k, v);
                }
                b
            })?;
        }
        if let Some(query) = desc.get::<Option<LuaTable>>("query")? {
            builder.map(|b| b.query(&query))?;
        }
        if let Some(query) = desc.get::<Option<LuaTable>>("query_pairs")? {
            let pairs = string_pairs(&query)?;
            builder.map(|b| b.query(&pairs))?;
        }
        if let Some(body) = desc.get::<Option<LuaTable>>("body")? {
            let kind: String = body.get("kind")?;
            let value: LuaValue = body.get("value")?;
            match kind.as_str() {
                "bytes" => {
                    let LuaValue::String(bytes) = value else {
                        return Err(http_error(
                            "invalid_argument",
                            "bytes body must be a string",
                        ));
                    };
                    builder.map(|b| b.body(bytes.as_bytes().to_vec()))?;
                }
                "json" => {
                    let bytes = if builder.options.strict {
                        crate::json::strict::encode(&value)?
                    } else {
                        serde_json::to_vec(&value).map_err(LuaError::external)?
                    };
                    builder.map(|b| {
                        b.header(header::CONTENT_TYPE, "application/json")
                            .body(bytes)
                    })?;
                }
                "form" => {
                    let LuaValue::Table(table) = value else {
                        return Err(http_error("invalid_argument", "form body must be a table"));
                    };
                    builder.map(|b| b.form(&table))?;
                }
                "form_pairs" => {
                    let LuaValue::Table(table) = value else {
                        return Err(http_error("invalid_argument", "form pairs must be a table"));
                    };
                    let pairs = string_pairs(&table)?;
                    builder.map(|b| b.form(&pairs))?;
                }
                _ => return Err(http_error("invalid_argument", "unknown body kind")),
            }
        }
        if let Some(seconds) = desc.get::<Option<f64>>("timeout")? {
            let duration = Duration::try_from_secs_f64(seconds)
                .map_err(|e| http_error("invalid_argument", e.to_string()))?;
            if duration.is_zero() {
                return Err(http_error("invalid_argument", "timeout must be positive"));
            }
            builder.map(|b| b.timeout(duration))?;
        }
        if let Some(stream) = desc.get::<Option<LuaTable>>("stream")? {
            let ms: u64 = stream.get("headers_timeout_ms")?;
            if ms == 0 {
                return Err(http_error(
                    "invalid_argument",
                    "headers timeout must be positive",
                ));
            }
            builder.headers_timeout = Some(Duration::from_millis(ms));
        }
        builder.prepare()
    }

    pub fn request(&self, method: &str, url: String) -> LuaResult<LuaRequestBuilder> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| http_error("invalid_argument", e.to_string()))?;
        let parsed =
            reqwest::Url::parse(&url).map_err(|e| http_error("invalid_argument", e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(http_error("invalid_argument", "expected an HTTP URL"));
        }
        Ok(self.builder(self.client.request(method, parsed)))
    }

    pub fn get(&self, url: String) -> LuaRequestBuilder {
        self.builder(self.client.get(url))
    }

    pub fn post(&self, url: String) -> LuaRequestBuilder {
        self.builder(self.client.post(url))
    }

    pub fn put(&self, url: String) -> LuaRequestBuilder {
        self.builder(self.client.put(url))
    }

    pub fn delete(&self, url: String) -> LuaRequestBuilder {
        self.builder(self.client.delete(url))
    }

    fn builder(&self, builder: reqwest::RequestBuilder) -> LuaRequestBuilder {
        LuaRequestBuilder {
            builder: Some(builder),
            cookie_provider: self.cookie_provider.clone(),
            options: BodyOptions::default(),
            stream_client: self.stream_client.clone(),
            headers_timeout: None,
        }
    }
}

impl LuaUserData for LuaHttpClient {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("new_batch", |_, _, options: Option<LuaTable>| {
            LuaBatch::new(options)
        });
        methods.add_method("request", |_, this, (method, url): (String, String)| {
            this.request(&method, url)
        });
        methods.add_async_function(
            "send",
            |_, (client, value): (LuaAnyUserData, LuaValue)| async move {
                let prepared = match value {
                    LuaValue::UserData(ud) => take_request(&ud)?,
                    LuaValue::Table(description) => client
                        .borrow::<LuaHttpClient>()?
                        .prepare_description(description)?,
                    _ => {
                        return Err(http_error(
                            "invalid_argument",
                            "expected a request or descriptor",
                        ))
                    }
                };
                prepared.send().await
            },
        );
        methods.add_method("get", |_, this, url: String| Ok(this.get(url)));
        methods.add_method("post", |_, this, url: String| Ok(this.post(url)));
        methods.add_method("put", |_, this, url: String| Ok(this.put(url)));
        methods.add_method("delete", |_, this, url: String| Ok(this.delete(url)));
        methods.add_method("cookie", |_, this, (url, name): (String, String)| {
            this.cookie(&url, &name)
        });
        methods.add_method("set_cookie", |_, this, (url, cookie): (String, String)| {
            this.set_cookie(&url, &cookie)
        });
        methods.add_method("clear_cookies", |_, this, ()| {
            this.clear_cookies();
            Ok(())
        });
    }
}

#[derive(Clone, Copy, Default)]
struct BodyOptions {
    strict: bool,
    limit: Option<usize>,
}

fn http_error(code: &'static str, message: impl Into<String>) -> LuaError {
    crate::error::Error::lua("http", code, message)
}
fn transport_error(error: reqwest::Error) -> LuaError {
    let code = if error.is_timeout() {
        "timeout"
    } else {
        "transport"
    };
    http_error(code, error.without_url().to_string())
}
fn parse_codec(codec: &str) -> LuaResult<bool> {
    match codec {
        "strict" => Ok(true),
        "legacy" => Ok(false),
        _ => Err(http_error("invalid_argument", "unknown JSON codec")),
    }
}

pub struct LuaRequestBuilder {
    stream_client: Option<reqwest::Client>,
    headers_timeout: Option<Duration>,
    options: BodyOptions,
    builder: Option<reqwest::RequestBuilder>,
    cookie_provider: Option<Arc<dyn CookieHeaderProvider>>,
}

impl LuaRequestBuilder {
    fn map(
        &mut self,
        f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> LuaResult<()> {
        let builder = self.take_builder()?;
        self.builder = Some(f(builder));
        Ok(())
    }

    fn take_builder(&mut self) -> LuaResult<reqwest::RequestBuilder> {
        self.builder
            .take()
            .ok_or_else(|| http_error("consumed", "request builder has been consumed"))
    }

    fn merge_cookies(&self, req: &mut reqwest::Request) -> LuaResult<()> {
        let Some(cookie_provider) = &self.cookie_provider else {
            return Ok(());
        };
        let url = req.url().clone();
        let headers = req.headers_mut();
        let Some(req_cookie) = headers.get_mut(header::COOKIE) else {
            return Ok(());
        };
        let Some(stored_cookie) = cookie_provider.cookies(&url) else {
            return Ok(());
        };
        let req_cookie_str = req_cookie.to_str().unwrap_or_default();
        let stored_cookie_str = stored_cookie.to_str().unwrap_or_default();
        let merged_cookie = merge_cookie_header(stored_cookie_str, req_cookie_str);
        *req_cookie = merged_cookie.parse().map_err(mlua::Error::external)?;
        Ok(())
    }

    pub fn build_split(&mut self) -> LuaResult<(reqwest::Client, reqwest::Request)> {
        let (client, req) = self.take_builder()?.build_split();
        let mut req = req.map_err(mlua::Error::external)?;
        self.merge_cookies(&mut req)?;
        Ok((client, req))
    }

    pub fn build(&mut self) -> LuaResult<reqwest::Request> {
        let mut req = self
            .take_builder()?
            .build()
            .map_err(mlua::Error::external)?;
        self.merge_cookies(&mut req)?;
        Ok(req)
    }

    /// Transfers a request for host-managed streaming and response processing.
    pub fn take_parts(
        &mut self,
    ) -> LuaResult<(reqwest::Client, reqwest::Request, Option<Duration>)> {
        let prepared = self.prepare()?;
        Ok((prepared.client, prepared.request, prepared.headers_timeout))
    }

    fn prepare(&mut self) -> LuaResult<PreparedRequest> {
        let (mut client, request) = self.build_split()?;
        if self.headers_timeout.is_some() {
            if request.timeout().is_some() {
                return Err(http_error(
                    "invalid_argument",
                    "stream and total timeout are mutually exclusive",
                ));
            }
            client = self.stream_client.clone().ok_or_else(|| {
                http_error("invalid_state", "host did not configure a streaming client")
            })?;
        }
        Ok(PreparedRequest {
            client,
            request,
            options: self.options,
            headers_timeout: self.headers_timeout,
        })
    }
    pub async fn send(&mut self) -> LuaResult<LuaResponse> {
        self.prepare()?.send().await
    }
}

impl LuaUserData for LuaRequestBuilder {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_function_mut(
            "header",
            |_, (ud, k, v): (LuaAnyUserData, String, String)| {
                let k = HeaderName::from_bytes(&k.as_bytes()).map_err(LuaError::external)?;
                let v = HeaderValue::from_bytes(&v.as_bytes()).map_err(LuaError::external)?;
                ud.borrow_mut::<LuaRequestBuilder>()?
                    .map(|b| b.header(k, v))?;
                Ok(ud)
            },
        );
        methods.add_function_mut("headers", |_, (ud, t): (LuaAnyUserData, LuaTable)| {
            let headers = table_to_header_map(&t)?;
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|b| b.headers(headers))?;
            Ok(ud)
        });
        methods.add_function_mut("query", |_, (ud, t): (LuaAnyUserData, LuaTable)| {
            ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| b.query(&t))?;
            Ok(ud)
        });
        methods.add_function_mut("form", |_, (ud, t): (LuaAnyUserData, LuaTable)| {
            ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| b.form(&t))?;
            Ok(ud)
        });
        methods.add_function_mut("multipart", |_, (ud, t): (LuaAnyUserData, LuaTable)| {
            let form = table_to_multipart_form(&t)?;
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|builder| builder.multipart(form))?;
            Ok(ud)
        });
        methods.add_function_mut("json", |_, (ud, v): (LuaAnyUserData, LuaValue)| {
            let strict = ud.borrow::<LuaRequestBuilder>()?.options.strict;
            let bytes = if strict {
                crate::json::strict::encode(&v)?
            } else {
                serde_json::to_vec(&v).map_err(LuaError::external)?
            };
            ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| {
                b.header(header::CONTENT_TYPE, "application/json")
                    .body(bytes)
            })?;
            Ok(ud)
        });
        methods.add_function_mut("json_codec", |_, (ud, codec): (LuaAnyUserData, String)| {
            let strict = parse_codec(&codec)?;
            let mut this = ud.borrow_mut::<LuaRequestBuilder>()?;
            if this.builder.is_none() {
                return Err(http_error("consumed", "request builder has been consumed"));
            }
            this.options.strict = strict;
            drop(this);
            Ok(ud)
        });
        methods.add_function_mut(
            "response_limit",
            |_, (ud, limit): (LuaAnyUserData, usize)| {
                let mut this = ud.borrow_mut::<LuaRequestBuilder>()?;
                if this.builder.is_none() {
                    return Err(http_error("consumed", "request builder has been consumed"));
                }
                this.options.limit = Some(limit);
                drop(this);
                Ok(ud)
            },
        );
        for kind in ["query_pairs", "form_pairs", "header_pairs"] {
            methods.add_function_mut(kind, move |_, (ud, pairs): (LuaAnyUserData, LuaTable)| {
                if kind == "header_pairs" {
                    let mut headers = Vec::new();
                    for (key, value) in byte_pairs(&pairs)? {
                        headers.push((
                            HeaderName::from_bytes(&key.as_bytes()).map_err(LuaError::external)?,
                            HeaderValue::from_bytes(&value.as_bytes())
                                .map_err(LuaError::external)?,
                        ));
                    }
                    ud.borrow_mut::<LuaRequestBuilder>()?.map(|mut b| {
                        for (k, v) in headers {
                            b = b.header(k, v);
                        }
                        b
                    })?;
                } else {
                    let pairs = string_pairs(&pairs)?;
                    ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| {
                        if kind == "query_pairs" {
                            b.query(&pairs)
                        } else {
                            b.form(&pairs)
                        }
                    })?;
                }
                Ok(ud)
            });
        }
        methods.add_function_mut("body", |_, (ud, body): (LuaAnyUserData, LuaValue)| {
            match body {
                LuaValue::String(s) => {
                    let bytes = s.as_bytes().to_vec();
                    ud.borrow_mut::<LuaRequestBuilder>()?
                        .map(|b| b.body(bytes))?;
                }
                LuaValue::Table(t) => {
                    let json = serde_json::to_string(&t).map_err(mlua::Error::external)?;
                    ud.borrow_mut::<LuaRequestBuilder>()?
                        .map(|b| b.header("Content-Type", "application/json").body(json))?;
                }
                _ => return Err(mlua::Error::runtime("Invalid body type")),
            }
            Ok(ud)
        });
        methods.add_function_mut("stream", |_, (ud, options): (LuaAnyUserData, LuaTable)| {
            let ms: u64 = options.get("headers_timeout_ms")?;
            if ms == 0 {
                return Err(http_error(
                    "invalid_argument",
                    "headers timeout must be positive",
                ));
            }
            let mut this = ud.borrow_mut::<LuaRequestBuilder>()?;
            if this.builder.is_none() {
                return Err(http_error("consumed", "request builder has been consumed"));
            }
            if this.stream_client.is_none() {
                return Err(http_error(
                    "invalid_state",
                    "host did not configure a streaming client",
                ));
            }
            this.headers_timeout = Some(Duration::from_millis(ms));
            drop(this);
            Ok(ud)
        });
        methods.add_function_mut("timeout", |_, (ud, secs): (LuaAnyUserData, f64)| {
            let duration = Duration::try_from_secs_f64(secs)
                .map_err(|e| http_error("invalid_argument", e.to_string()))?;
            if duration.is_zero() {
                return Err(http_error("invalid_argument", "timeout must be positive"));
            }
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|b| b.timeout(duration))?;
            Ok(ud)
        });
        methods.add_function_mut("version", |_, (ud, version): (LuaAnyUserData, String)| {
            let version = match version.as_str() {
                "HTTP/1.1" => reqwest::Version::HTTP_11,
                "HTTP/2" => reqwest::Version::HTTP_2,
                _ => return Err(mlua::Error::runtime("Unsupported HTTP version")),
            };
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|b| b.version(version))?;
            Ok(ud)
        });
        methods.add_function_mut("build", |_, ud: LuaAnyUserData| {
            Ok(LuaRequest(Some(
                ud.borrow_mut::<LuaRequestBuilder>()?.prepare()?,
            )))
        });
        methods.add_async_function("send", |_, ud: LuaAnyUserData| async move {
            let prepared = ud.borrow_mut::<LuaRequestBuilder>()?.prepare()?;
            prepared.send().await
        });
    }
}

pub struct LuaRequest(Option<PreparedRequest>);

impl LuaRequest {
    /// Transfers a request for host-managed streaming and response processing.
    pub fn take_parts(
        &mut self,
    ) -> LuaResult<(reqwest::Client, reqwest::Request, Option<Duration>)> {
        let prepared = self
            .0
            .take()
            .ok_or_else(|| http_error("consumed", "request already consumed"))?;
        Ok((prepared.client, prepared.request, prepared.headers_timeout))
    }
}

struct PreparedRequest {
    client: reqwest::Client,
    request: reqwest::Request,
    options: BodyOptions,
    headers_timeout: Option<Duration>,
}
impl PreparedRequest {
    async fn send(self) -> LuaResult<LuaResponse> {
        let send = self.client.execute(self.request);
        let response = match self.headers_timeout {
            Some(timeout) => tokio::time::timeout(timeout, send)
                .await
                .map_err(|_| http_error("timeout", "response headers timed out"))?,
            None => send.await,
        }
        .map_err(transport_error)?;
        Ok(LuaResponse::with_options(response, self.options))
    }
}
fn take_request(ud: &LuaAnyUserData) -> LuaResult<PreparedRequest> {
    if ud.is::<LuaRequestBuilder>() {
        ud.borrow_mut::<LuaRequestBuilder>()?.prepare()
    } else {
        ud.borrow_mut::<LuaRequest>()?
            .0
            .take()
            .ok_or_else(|| http_error("consumed", "request has been consumed"))
    }
}

pub(crate) fn string_pairs(table: &LuaTable) -> LuaResult<Vec<(String, String)>> {
    byte_pairs(table)?
        .into_iter()
        .map(|(name, value)| Ok((name.to_str()?.to_owned(), value.to_str()?.to_owned())))
        .collect()
}
fn byte_pairs(table: &LuaTable) -> LuaResult<Vec<(LuaString, LuaString)>> {
    let len = table.raw_len();
    let mut count = 0;
    for pair in table.clone().pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        if !matches!(key, LuaValue::Integer(i) if i >= 1 && (i as u64) <= len as u64) {
            return Err(http_error(
                "invalid_argument",
                "pairs must be a dense array",
            ));
        }
        count += 1;
    }
    if count != len {
        return Err(http_error(
            "invalid_argument",
            "pairs must be a dense array",
        ));
    }
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        let pair: LuaTable = table.raw_get(i)?;
        if pair.clone().pairs::<LuaValue, LuaValue>().count() != 2 {
            return Err(http_error("invalid_argument", "expected a name/value pair"));
        }
        out.push((pair.raw_get::<LuaString>(1)?, pair.raw_get::<LuaString>(2)?));
    }
    Ok(out)
}

impl LuaUserData for LuaRequest {
    fn add_methods<M: LuaUserDataMethods<Self>>(_methods: &mut M) {}
}

fn merge_cookie_header(stored_cookie: &str, req_cookie: &str) -> String {
    let mut cookie_map = HashMap::new();
    for cookie in stored_cookie.split(';') {
        if let Some((key, value)) = cookie.trim().split_once('=') {
            cookie_map.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    for cookie in req_cookie.split(';') {
        if let Some((key, value)) = cookie.trim().split_once('=') {
            cookie_map.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    let mut pairs: Vec<_> = cookie_map.iter().collect();
    pairs.sort_by_key(|(key, _)| *key);
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn prepared_request_parts_preserve_headers_timeout_and_consumption() {
        let lua = Lua::new();
        let client = reqwest::Client::new();
        lua.globals()
            .set(
                "http",
                LuaHttpClient::new(client.clone()).with_stream_client(client),
            )
            .unwrap();
        let ud=lua.load(r#"return http:get('https://example.invalid/media'):header_pairs{{'If-None-Match','"one"'}}:stream{headers_timeout_ms=1234}:build()"#).eval::<LuaAnyUserData>().unwrap();
        let mut request = ud.borrow_mut::<LuaRequest>().unwrap();
        let (_, request_value, timeout) = request.take_parts().unwrap();
        assert_eq!(request_value.headers()["if-none-match"], "\"one\"");
        assert_eq!(timeout, Some(Duration::from_millis(1234)));
        assert!(request.take_parts().is_err());
        let ud = lua
            .load(
                "return http:get('https://example.invalid/media'):stream{headers_timeout_ms=4321}",
            )
            .eval::<LuaAnyUserData>()
            .unwrap();
        let mut builder = ud.borrow_mut::<LuaRequestBuilder>().unwrap();
        assert_eq!(
            builder.take_parts().unwrap().2,
            Some(Duration::from_millis(4321))
        );
        assert!(builder.take_parts().is_err());
    }

    #[test]
    fn merged_cookies_have_stable_order_and_request_precedence() {
        for _ in 0..16 {
            assert_eq!(merge_cookie_header("b=2; a=1", "b=3; c=4"), "a=1; b=3; c=4");
        }
    }

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    fn session() -> (LuaHttpClient, Arc<SessionCookieStore>) {
        let store = Arc::new(SessionCookieStore::default());
        let client = reqwest::Client::builder()
            .user_agent("mlua-extra-test")
            .cookie_provider(store.clone())
            .build()
            .unwrap();
        (LuaHttpClient::with_session(client, store.clone()), store)
    }

    #[test]
    fn session_cookie_store_obeys_scope_and_supports_named_access() {
        let store = SessionCookieStore::default();
        let origin = reqwest::Url::parse("https://example.com/login").unwrap();
        let headers = [
            header("secure_token=token; Domain=example.com; Path=/; Secure; HttpOnly"),
            header("scoped=value; Path=/workshop"),
            header("expired=value; Path=/; Max-Age=0"),
        ];
        ReqwestCookieStore::set_cookies(&store, &mut headers.iter(), &origin);
        assert!(store.is_dirty());

        let workshop = reqwest::Url::parse("https://example.com/workshop/item").unwrap();
        let other_site = reqwest::Url::parse("https://example.net/").unwrap();
        let insecure = reqwest::Url::parse("http://example.com/workshop/item").unwrap();
        assert_eq!(
            store.cookie(&workshop, "secure_token").as_deref(),
            Some("token")
        );
        assert_eq!(store.cookie(&workshop, "scoped").as_deref(), Some("value"));
        assert_eq!(store.cookie(&workshop, "expired"), None);
        assert_eq!(store.cookie(&other_site, "secure_token"), None);
        assert_eq!(store.cookie(&insecure, "secure_token"), None);

        store.clear();
        assert_eq!(store.cookie(&workshop, "secure_token"), None);
    }

    #[test]
    fn session_cookie_store_inserts_and_builds_request_header() {
        let store = SessionCookieStore::default();
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        store
            .insert(
                &url,
                "sessionid=abc; Domain=example.com; Path=/; Secure; SameSite=None",
            )
            .unwrap();

        let cookies = ReqwestCookieStore::cookies(&store, &url)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(cookies, "sessionid=abc");
    }

    #[test]
    fn injected_sessions_are_isolated_and_views_share_their_store() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionCookieStore>();

        let (first, first_store) = session();
        let first_view = LuaHttpClient::with_session(first.client.clone(), first_store);
        let (second, _) = session();
        let url = "https://example.com/";
        first
            .set_cookie(url, "sessionid=first; Domain=example.com; Path=/")
            .unwrap();

        assert_eq!(
            first.cookie(url, "sessionid").unwrap().as_deref(),
            Some("first")
        );
        assert_eq!(
            first_view.cookie(url, "sessionid").unwrap().as_deref(),
            Some("first")
        );
        assert_eq!(second.cookie(url, "sessionid").unwrap(), None);
    }

    #[test]
    fn cookie_snapshot_round_trip_tracks_revision_and_drops_expired_entries() {
        let store = SessionCookieStore::default();
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        store
            .insert(&url, "session=ready; Domain=example.com; Path=/; Secure")
            .unwrap();
        let revision = store.revision();
        assert!(store.is_dirty());

        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.revision, revision);
        assert!(store.mark_clean(snapshot.revision));
        assert!(!store.is_dirty());

        store
            .insert(&url, "obsolete=value; Domain=example.com; Path=/")
            .unwrap();
        store
            .insert(&url, "obsolete=gone; Domain=example.com; Path=/; Max-Age=0")
            .unwrap();
        assert!(store.is_dirty());
        let snapshot_without_expired = store.snapshot().unwrap();

        let restored = SessionCookieStore::default();
        restored.restore(&snapshot.value).unwrap();
        assert_eq!(restored.cookie(&url, "session").as_deref(), Some("ready"));
        assert_eq!(restored.cookie(&url, "expired"), None);
        assert!(!restored.is_dirty());

        let restored_without_expired = SessionCookieStore::default();
        restored_without_expired
            .restore(&snapshot_without_expired.value)
            .unwrap();
        assert_eq!(restored_without_expired.cookie(&url, "obsolete"), None);
    }

    #[test]
    fn multipart_accepts_scalar_lua_fields() {
        let lua = Lua::new();
        let values = lua.create_table().unwrap();
        values.set("nonce", "token").unwrap();
        values.set("attempt", 1).unwrap();
        let form = table_to_multipart_form(&values).unwrap();
        let request = reqwest::Client::new()
            .post("https://example.com")
            .multipart(form)
            .build()
            .unwrap();

        assert!(request.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("multipart/form-data; boundary="));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_follows_redirects_and_returns_stored_cookies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let response = match index {
                    0 => {
                        assert!(request.starts_with("GET /start "));
                        "HTTP/1.1 302 Found\r\nLocation: /finish\r\nSet-Cookie: redirected=yes; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    1 => {
                        assert!(request.starts_with("GET /finish "));
                        assert!(request.contains("redirected=yes"));
                        "HTTP/1.1 200 OK\r\nSet-Cookie: session=ready; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    _ => {
                        assert!(request.starts_with("GET /check "));
                        assert!(request.contains("redirected=yes"));
                        assert!(request.contains("session=ready"));
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let (session, _) = session();
        let start = format!("http://{address}/start");
        let response = session.get(start).send().await.unwrap();
        assert_eq!(response.url().path(), "/finish");
        let root = format!("http://{address}/");
        assert_eq!(
            session.cookie(&root, "redirected").unwrap().as_deref(),
            Some("yes")
        );
        assert_eq!(
            session.cookie(&root, "session").unwrap().as_deref(),
            Some("ready")
        );

        session
            .get(format!("http://{address}/check"))
            .send()
            .await
            .unwrap();
        server.join().unwrap();
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn server(count: usize) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let body = br#"{"xs":[null],"n":9223372036854775807}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Test: one\r\nX-Test: two\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }
    fn lua() -> Lua {
        let lua = Lua::new();
        lua.globals()
            .set("http", LuaHttpClient::new(reqwest::Client::new()))
            .unwrap();
        lua.globals()
            .set("J", crate::json::strict::create_module(&lua).unwrap())
            .unwrap();
        lua.globals()
            .set("errors", crate::error::create_module(&lua).unwrap())
            .unwrap();
        lua
    }
    #[test]
    fn request_pairs_and_validation_preserve_builder_on_error() {
        let lua = lua();
        let ud: LuaAnyUserData = lua.load(r#"
            local b = http:request('PATCH','http://example.com/api'):query_pairs{{'id','a/b'},{'id','a b'},{'empty',''}}
            assert(not pcall(function() b:timeout(-1) end))
            assert(not pcall(function() b:query_pairs{[2]={'x','y'}} end))
            return b:json_codec('strict'):json({ids=J.array{}, value=J.null}):build()
        "#).eval().unwrap();
        let request = take_request(&ud).unwrap();
        assert_eq!(request.request.method(), reqwest::Method::PATCH);
        assert_eq!(
            request.request.url().query(),
            Some("id=a%2Fb&id=a+b&empty=")
        );
        assert_eq!(
            request.request.body().unwrap().as_bytes().unwrap(),
            br#"{"ids":[],"value":null}"#
        );
        assert!(take_request(&ud).is_err());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn response_metadata_survives_reads_and_transfer() {
        let (url, server) = server(3);
        let lua = lua();
        lua.globals().set("url", url).unwrap();
        lua.load(
            r#"
            local request = http:get(url):json_codec('strict'):build()
            local rsp = http:send(request)
            local value = rsp:json()
            assert(value.xs[1] == J.null and value.n == math.maxinteger)
            assert(rsp:status()==200 and rsp:ok() and rsp:body_state()=='consumed')
            assert(#rsp:header_values('x-test')==2)
            local ok, err = pcall(function() rsp:bytes() end)
            assert(not ok and errors.inspect(err).code=='consumed')
            local limited = http:get(url):response_limit(1):send()
            local ok, err = pcall(function() limited:bytes() end)
            assert(not ok and errors.inspect(err).code=='limit')
            assert(limited:body_state()=='consumed' and limited:status()==200)
        "#,
        )
        .exec_async()
        .await
        .unwrap();
        let ud: LuaAnyUserData = lua
            .load("return http:get(url):send()")
            .eval_async()
            .await
            .unwrap();
        let mut response = ud.borrow_mut::<LuaResponse>().unwrap();
        let transferred = response.take_response().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.body_state(), "transferred");
        assert!(response.take_response().is_err());
        drop(transferred);
        server.join().unwrap();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn response_batch_refills_drains_and_preserves_rejected_requests() {
        let (url, server) = server(3);
        let lua = lua();
        lua.globals().set("url", url).unwrap();
        lua.load(
            r#"
            local b = http:new_batch{results='response',limit=2}
            assert(not pcall(function() b:wait_one() end))
            b:add('one',http:get(url))
            b:add('two',http:get(url))
            local tail = http:get(url):build()
            assert(not pcall(function() b:add('three',tail) end))
            local seen = {}
            local first = b:wait_one()
            assert(first.response and not first.error)
            seen[first.key]=true; first.response:close()
            assert(not pcall(function() b:add(first.key,tail) end))
            b:add('three',tail); b:seal()
            while true do
                local result = b:wait_one()
                if result == nil then break end
                assert(not seen[result.key] and result.response)
                seen[result.key]=true; result.response:close()
            end
            assert(seen.one and seen.two and seen.three)
            b:close()
            local empty = http:new_batch{results='response'}
            empty:seal(); assert(empty:wait_one()==nil)
        "#,
        )
        .exec_async()
        .await
        .unwrap();
        server.join().unwrap();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn batch_transport_errors_keep_their_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let lua = lua();
        lua.globals()
            .set("url", format!("http://{address}"))
            .unwrap();
        lua.load(
            r#"
            local b=http:new_batch{results='response'}
            b:add('failed',http:get(url)); b:seal()
            local result=b:wait_one()
            assert(result.key=='failed' and result.error and not result.response)
            assert(errors.inspect(result.error).code=='transport')
            assert(b:wait_one()==nil)
            b:close()
        "#,
        )
        .exec_async()
        .await
        .unwrap();
    }
    #[test]
    fn descriptor_and_builder_prepare_the_same_request() {
        let lua = lua();
        let desc: LuaTable = lua
            .load(
                r#"return {
            method='POST',url='http://example.com/prefix',json_codec='strict',response_limit=100,
            query_pairs={{'tag','one'},{'tag','two'}},body={kind='json',value={x=J.null}},timeout=5
        }"#,
            )
            .eval()
            .unwrap();
        let client = LuaHttpClient::new(reqwest::Client::new());
        let prepared = client.prepare_description(desc).unwrap();
        let ud: LuaAnyUserData = lua
            .load(
                r#"return http:post('http://example.com/prefix')
            :json_codec('strict'):response_limit(100):query_pairs{{'tag','one'},{'tag','two'}}
            :json({x=J.null}):timeout(5):build()"#,
            )
            .eval()
            .unwrap();
        let built = take_request(&ud).unwrap();
        assert_eq!(prepared.request.url(), built.request.url());
        assert_eq!(prepared.request.headers(), built.request.headers());
        assert_eq!(
            prepared.request.body().unwrap().as_bytes(),
            built.request.body().unwrap().as_bytes()
        );
        assert_eq!(prepared.request.timeout(), built.request.timeout());
        assert_eq!(prepared.options.limit, built.options.limit);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_wait_reclaims_batch_tasks() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let lua = lua();
        lua.globals()
            .set("url", format!("http://{address}"))
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            lua.load(
                r#"
            b=http:new_batch{results='response'}
            b:add('slow',http:get(url)); b:seal(); b:wait_one()
        "#,
            )
            .exec_async(),
        )
        .await;
        assert!(result.is_err());
        lua.load(
            r#"
            local ok,err=pcall(function() b:wait_one() end)
            assert(not ok and errors.inspect(err).code=='cancelled')
            b:close(); b:close()
        "#,
        )
        .exec_async()
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn stream_requires_host_configuration_and_headers_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let lua = lua();
        lua.globals()
            .set("url", format!("http://{address}"))
            .unwrap();
        lua.load("assert(not pcall(function() http:get(url):stream{headers_timeout_ms=10} end))")
            .exec()
            .unwrap();
        lua.globals()
            .set(
                "http",
                LuaHttpClient::new(reqwest::Client::new())
                    .with_stream_client(reqwest::Client::new()),
            )
            .unwrap();
        lua.load(r#"
            assert(not pcall(function() http:get(url):timeout(1):stream{headers_timeout_ms=10}:build() end))
            local ok,err=pcall(function() http:get(url):stream{headers_timeout_ms=10}:send() end)
            assert(not ok and errors.inspect(err).code=='timeout')
        "#).exec_async().await.unwrap();
    }
}
