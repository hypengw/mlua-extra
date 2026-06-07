use crate::util::to_lua;
use mlua::prelude::*;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub fn default(user_agent: &str) -> LuaHttpClient {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(user_agent.to_owned())
                .build()
                .expect("build reqwest client")
        })
        .clone();
    LuaHttpClient::new(client)
}

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

pub trait CookieHeaderProvider: Send + Sync {
    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue>;
}

pub struct LuaHttpClient {
    client: reqwest::Client,
    cookie_provider: Option<Arc<dyn CookieHeaderProvider>>,
}

impl LuaHttpClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            cookie_provider: None,
        }
    }

    pub fn with_cookie_provider(
        client: reqwest::Client,
        cookie_provider: Arc<dyn CookieHeaderProvider>,
    ) -> Self {
        Self {
            client,
            cookie_provider: Some(cookie_provider),
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
        methods.add_method("headers", |lua, this, ()| match this.0.as_ref() {
            Some(r) => header_map_to_table(lua, r.headers()),
            None => lua.create_table(),
        });
        methods.add_async_method_mut("text", |_, mut this, ()| async move {
            this.0
                .take()
                .ok_or(mlua::Error::UserDataBorrowError)?
                .text()
                .await
                .map_err(mlua::Error::external)
        });
        methods.add_async_method_mut("bytes", |lua, mut this, ()| async move {
            let bytes = this
                .0
                .take()
                .ok_or(mlua::Error::UserDataBorrowError)?
                .bytes()
                .await
                .map_err(mlua::Error::external)?;
            lua.create_string(bytes)
        });
        methods.add_async_method_mut("json", |lua, mut this, ()| async move {
            let value: serde_json::Value = this
                .0
                .take()
                .ok_or(mlua::Error::UserDataBorrowError)?
                .json()
                .await
                .map_err(mlua::Error::external)?;
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
