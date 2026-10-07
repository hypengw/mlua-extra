use super::{header_map_to_table, http_error, BodyOptions};
use crate::util::to_lua;
use mlua::prelude::*;
use reqwest::header::HeaderMap;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

const AVAILABLE: u8 = 0;
const READING: u8 = 1;
const CONSUMED: u8 = 2;
const TRANSFERRED: u8 = 3;
const CLOSED: u8 = 4;

pub struct LuaResponse {
    response: Option<reqwest::Response>,
    status: reqwest::StatusCode,
    url: reqwest::Url,
    headers: HeaderMap,
    state: Arc<AtomicU8>,
    options: BodyOptions,
}

struct ReadGuard(Arc<AtomicU8>);
impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.0.store(CONSUMED, Ordering::Release);
    }
}

impl LuaResponse {
    pub fn from_response(response: reqwest::Response) -> Self {
        Self::with_options(response, BodyOptions::default())
    }
    pub(super) fn with_options(response: reqwest::Response, options: BodyOptions) -> Self {
        Self {
            status: response.status(),
            url: response.url().clone(),
            headers: response.headers().clone(),
            response: Some(response),
            state: Arc::new(AtomicU8::new(AVAILABLE)),
            options,
        }
    }
    fn take(&mut self, state: u8) -> LuaResult<reqwest::Response> {
        let response = self.response.take().ok_or_else(|| {
            if self.state.load(Ordering::Acquire) == READING {
                http_error("busy", "response body is being read")
            } else {
                http_error("consumed", "response body is no longer available")
            }
        })?;
        self.state.store(state, Ordering::Release);
        Ok(response)
    }
    pub fn take_response(&mut self) -> LuaResult<reqwest::Response> {
        self.take(TRANSFERRED)
    }
    pub fn status(&self) -> reqwest::StatusCode {
        self.status
    }
    pub fn url(&self) -> &reqwest::Url {
        &self.url
    }
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    pub fn body_state(&self) -> &'static str {
        match self.state.load(Ordering::Acquire) {
            AVAILABLE => "available",
            READING => "reading",
            CONSUMED => "consumed",
            TRANSFERRED => "transferred",
            _ => "closed",
        }
    }
    pub fn close(&mut self) -> LuaResult<()> {
        if self.state.load(Ordering::Acquire) == READING {
            return Err(http_error("busy", "response body is being read"));
        }
        if self.response.take().is_some() {
            self.state.store(CLOSED, Ordering::Release);
        }
        Ok(())
    }
    fn read(
        &mut self,
        opts: Option<LuaTable>,
    ) -> LuaResult<(
        reqwest::Response,
        ReadGuard,
        BodyOptions,
        Option<std::time::Duration>,
    )> {
        let mut options = self.options;
        let mut timeout = None;
        if let Some(opts) = opts {
            if let Some(codec) = opts.get::<Option<String>>("codec")? {
                options.strict = super::parse_codec(&codec)?;
            }
            if let Some(limit) = opts.get::<Option<usize>>("max_bytes")? {
                options.limit = Some(options.limit.map_or(limit, |old| old.min(limit)));
            }
            if let Some(ms) = opts.get::<Option<u64>>("timeout_ms")? {
                if ms == 0 {
                    return Err(http_error("invalid_argument", "timeout must be positive"));
                }
                timeout = Some(std::time::Duration::from_millis(ms));
            }
        }
        if options.strict {
            options.limit = Some(
                options
                    .limit
                    .unwrap_or(8 * 1024 * 1024)
                    .min(8 * 1024 * 1024),
            );
        }
        let response = self.take(READING)?;
        Ok((response, ReadGuard(self.state.clone()), options, timeout))
    }
}

async fn read_body(
    mut response: reqwest::Response,
    options: BodyOptions,
    timeout: Option<std::time::Duration>,
) -> LuaResult<Vec<u8>> {
    let read = async move {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(super::transport_error)? {
            if options
                .limit
                .is_some_and(|limit| bytes.len().saturating_add(chunk.len()) > limit)
            {
                return Err(http_error("limit", "response body exceeds byte limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    };
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| http_error("timeout", "response read timed out"))?,
        None => read.await,
    }
}

impl LuaUserData for LuaResponse {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("status", |_, this, ()| Ok(this.status.as_u16()));
        methods.add_method("ok", |_, this, ()| Ok(this.status.is_success()));
        methods.add_method("url", |_, this, ()| Ok(this.url.to_string()));
        methods.add_method("headers", |lua, this, ()| {
            header_map_to_table(lua, &this.headers)
        });
        methods.add_method("header_pairs", |lua, this, ()| {
            let out = lua.create_table()?;
            for (index, (name, value)) in this.headers.iter().enumerate() {
                let pair = lua.create_table()?;
                pair.raw_set(1, name.as_str())?;
                pair.raw_set(2, lua.create_string(value.as_bytes())?)?;
                out.raw_set(index + 1, pair)?;
            }
            Ok(out)
        });
        methods.add_method("header_values", |lua, this, name: String| {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(LuaError::external)?;
            let out = lua.create_table()?;
            for (index, value) in this.headers.get_all(name).iter().enumerate() {
                out.raw_set(index + 1, lua.create_string(value.as_bytes())?)?;
            }
            Ok(out)
        });
        methods.add_method("body_state", |_, this, ()| Ok(this.body_state()));
        methods.add_method_mut("close", |_, this, ()| this.close());
        for mode in ["bytes", "text", "json"] {
            methods.add_async_function(
                mode,
                move |lua, (ud, opts): (LuaAnyUserData, Option<LuaTable>)| async move {
                    let strict_utf8 = opts
                        .as_ref()
                        .map(|t| t.get::<Option<bool>>("strict_utf8"))
                        .transpose()?
                        .flatten()
                        .unwrap_or(false);
                    let (response, _guard, options, timeout) =
                        ud.borrow_mut::<LuaResponse>()?.read(opts)?;
                    let bytes = read_body(response, options, timeout).await?;
                    match mode {
                        "json" if options.strict => crate::json::strict::decode(&lua, &bytes),
                        "json" => {
                            let value: serde_json::Value = serde_json::from_slice(&bytes)
                                .map_err(|e| http_error("decode", e.to_string()))?;
                            to_lua(&lua, &value)
                        }
                        "text" => {
                            let text = if strict_utf8 {
                                std::str::from_utf8(&bytes)
                                    .map(std::borrow::Cow::Borrowed)
                                    .map_err(|e| http_error("decode", e.to_string()))?
                            } else {
                                String::from_utf8_lossy(&bytes)
                            };
                            Ok(LuaValue::String(lua.create_string(text.as_bytes())?))
                        }
                        _ => Ok(LuaValue::String(lua.create_string(bytes)?)),
                    }
                },
            );
        }
    }
}
