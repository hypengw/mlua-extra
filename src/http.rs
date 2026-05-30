use crate::util::to_lua;
use mlua::prelude::*;
use reqwest::header::{HeaderMap, HeaderName};
use std::str::FromStr;
use std::sync::OnceLock;
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
    LuaHttpClient(client)
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

pub struct LuaHttpClient(pub reqwest::Client);

impl LuaHttpClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self(client)
    }
}

impl LuaUserData for LuaHttpClient {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |_, this, url: String| {
            Ok(LuaRequestBuilder(Some(this.0.get(url))))
        });
        methods.add_method("post", |_, this, url: String| {
            Ok(LuaRequestBuilder(Some(this.0.post(url))))
        });
        methods.add_method("put", |_, this, url: String| {
            Ok(LuaRequestBuilder(Some(this.0.put(url))))
        });
        methods.add_method("delete", |_, this, url: String| {
            Ok(LuaRequestBuilder(Some(this.0.delete(url))))
        });
    }
}

pub struct LuaRequestBuilder(Option<reqwest::RequestBuilder>);

impl LuaRequestBuilder {
    fn map(
        &mut self,
        f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> LuaResult<()> {
        let b = self.0.take().ok_or(mlua::Error::UserDataBorrowError)?;
        self.0 = Some(f(b));
        Ok(())
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
        methods.add_function_mut("body", |_, (ud, body): (LuaAnyUserData, LuaString)| {
            let bytes = body.as_bytes().to_vec();
            ud.borrow_mut::<LuaRequestBuilder>()?.map(|b| b.body(bytes))?;
            Ok(ud)
        });
        methods.add_function_mut("timeout", |_, (ud, secs): (LuaAnyUserData, f64)| {
            ud.borrow_mut::<LuaRequestBuilder>()?
                .map(|b| b.timeout(Duration::from_secs_f64(secs)))?;
            Ok(ud)
        });
        methods.add_async_method_mut("send", |_, mut this, ()| async move {
            let b = this.0.take().ok_or(mlua::Error::UserDataBorrowError)?;
            let rsp = b.send().await.map_err(mlua::Error::external)?;
            Ok(LuaResponse(Some(rsp)))
        });
    }
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
