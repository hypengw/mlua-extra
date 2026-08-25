use crate::util::to_lua;
use mlua::prelude::*;
use reqwest::cookie::CookieStore as ReqwestCookieStore;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
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
}

impl LuaHttpClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            cookie_provider: None,
            session_store: None,
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
        }
    }

    pub fn with_session(client: reqwest::Client, store: Arc<SessionCookieStore>) -> Self {
        let cookie_provider: Arc<dyn CookieHeaderProvider> = store.clone();
        Self {
            client,
            cookie_provider: Some(cookie_provider),
            session_store: Some(store),
        }
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
        }
    }
}

impl LuaUserData for LuaHttpClient {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
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

pub struct LuaRequestBuilder {
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
        self.builder.take().ok_or(mlua::Error::UserDataBorrowError)
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

    pub async fn send(&mut self) -> LuaResult<LuaResponse> {
        let (client, req) = self.build_split()?;
        let rsp = client.execute(req).await.map_err(mlua::Error::external)?;
        Ok(LuaResponse(Some(rsp)))
    }
}

impl LuaUserData for LuaRequestBuilder {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_function_mut(
            "header",
            |_, (ud, k, v): (LuaAnyUserData, String, String)| {
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
            ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| b.json(&v))?;
            Ok(ud)
        });
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
        methods.add_function_mut("timeout", |_, (ud, secs): (LuaAnyUserData, f64)| {
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|b| b.timeout(Duration::from_secs_f64(secs)))?;
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
            let req = ud.borrow_mut::<LuaRequestBuilder>()?.build()?;
            Ok(LuaRequest(req))
        });
        methods.add_async_method_mut("send", |_, mut this, ()| async move { this.send().await });
    }
}

pub struct LuaRequest(pub reqwest::Request);

impl LuaUserData for LuaRequest {
    fn add_methods<M: LuaUserDataMethods<Self>>(_methods: &mut M) {}
}

pub struct LuaResponse(pub Option<reqwest::Response>);

pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

// Stream the body, stopping before it exceeds `cap` decoded bytes.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> LuaResult<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(mlua::Error::external)? {
        if chunk.len() > cap - buf.len() {
            return Err(mlua::Error::external("response body exceeds limit"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

impl LuaUserData for LuaResponse {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("status", |_, this, ()| {
            Ok(this.0.as_ref().map(|r| r.status().as_u16()).unwrap_or(0))
        });
        methods.add_method("ok", |_, this, ()| {
            Ok(this
                .0
                .as_ref()
                .map(|r| r.status().is_success())
                .unwrap_or(false))
        });
        methods.add_method("url", |_, this, ()| {
            Ok(this.0.as_ref().map(|r| r.url().to_string()))
        });
        methods.add_method("headers", |lua, this, ()| match this.0.as_ref() {
            Some(r) => header_map_to_table(lua, r.headers()),
            None => lua.create_table(),
        });
        methods.add_async_method_mut("text", |_, mut this, ()| async move {
            let resp = this.0.take().ok_or(mlua::Error::UserDataBorrowError)?;
            let bytes = read_capped(resp, MAX_TEXT_BYTES).await?;
            // Lossy like reqwest's text(): never reject on invalid UTF-8.
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        });
        methods.add_async_method_mut("bytes", |lua, mut this, ()| async move {
            let resp = this.0.take().ok_or(mlua::Error::UserDataBorrowError)?;
            let bytes = read_capped(resp, MAX_BYTES).await?;
            lua.create_string(bytes)
        });
        methods.add_async_method_mut("json", |lua, mut this, ()| async move {
            let resp = this.0.take().ok_or(mlua::Error::UserDataBorrowError)?;
            let bytes = read_capped(resp, MAX_TEXT_BYTES).await?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(mlua::Error::external)?;
            to_lua(&lua, &value)
        });
    }
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
    cookie_map
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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
        assert_eq!(response.0.as_ref().unwrap().url().path(), "/finish");
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

    fn serve_body(body: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap_or(0);
                if count == 0 {
                    break;
                }
                if buffer[..count].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });
        (address, handle)
    }

    async fn fetch_raw(address: std::net::SocketAddr) -> reqwest::Response {
        let (session, _) = session();
        session
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
            .0
            .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_capped_rejects_body_over_cap() {
        let (address, server) = serve_body(vec![b'x'; 4096]);
        let resp = fetch_raw(address).await;
        assert!(read_capped(resp, 1000).await.is_err());
        let _ = server.join();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_capped_passes_body_through_under_cap() {
        let (address, server) = serve_body(b"hello world".to_vec());
        let resp = fetch_raw(address).await;
        let bytes = read_capped(resp, 1_000_000).await.unwrap();
        assert_eq!(bytes, b"hello world");
        server.join().unwrap();
    }
}
