use mlua::prelude::*;
use url::Url;

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "encode_path_segment",
        lua.create_function(|_, value: LuaString| Ok(encode_path_segment(&value.as_bytes())))?,
    )?;
    t.set(
        "encode_query_pairs",
        lua.create_function(|_, values: LuaTable| {
            serde_urlencoded::to_string(crate::http::string_pairs(&values)?)
                .map_err(LuaError::external)
        })?,
    )?;
    t.set(
        "append_api_path",
        lua.create_function(|_, (base, path): (String, String)| append_api_path(&base, &path))?,
    )?;

    t.set(
        "encode",
        lua.create_function(|_, v: LuaValue| {
            serde_urlencoded::to_string(&v).map_err(mlua::Error::external)
        })?,
    )?;

    t.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let out = lua.create_table()?;
            for (k, v) in url::form_urlencoded::parse(s.as_bytes()) {
                out.set(k.into_owned(), v.into_owned())?;
            }
            Ok(out)
        })?,
    )?;

    t.set(
        "encode_component",
        lua.create_function(|_, s: String| {
            Ok(url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>())
        })?,
    )?;

    t.set(
        "decode_component",
        lua.create_function(|_, s: String| {
            let encoded = format!("value={s}");
            Ok(url::form_urlencoded::parse(encoded.as_bytes())
                .find_map(|(key, value)| (key == "value").then(|| value.into_owned()))
                .unwrap_or_default())
        })?,
    )?;

    t.set(
        "host",
        lua.create_function(|_, u: String| {
            Ok(Url::parse(&u)
                .ok()
                .and_then(|p| p.host_str().map(str::to_owned)))
        })?,
    )?;

    t.set(
        "parse",
        lua.create_function(|lua, u: String| match Url::parse(&u) {
            Ok(p) => {
                let r = lua.create_table()?;
                r.set("scheme", p.scheme())?;
                if let Some(h) = p.host_str() {
                    r.set("host", h)?;
                }
                if let Some(port) = p.port_or_known_default() {
                    r.set("port", port)?;
                }
                r.set("path", p.path())?;
                if let Some(q) = p.query() {
                    r.set("query", q)?;
                }
                if let Some(f) = p.fragment() {
                    r.set("fragment", f)?;
                }
                Ok(LuaValue::Table(r))
            }
            Err(_) => Ok(LuaValue::Nil),
        })?,
    )?;

    t.set(
        "join",
        lua.create_function(|_, (base, rel): (String, String)| {
            Ok(Url::parse(&base)
                .and_then(|b| b.join(&rel))
                .map(|u| u.to_string())
                .ok())
        })?,
    )?;

    Ok(t)
}

pub fn encode_path_segment(value: &[u8]) -> String {
    let mut out = String::new();
    for &byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").unwrap();
        }
    }
    out
}

pub fn append_api_path(base: &str, path: &str) -> LuaResult<String> {
    let mut url = Url::parse(base).map_err(LuaError::external)?;
    let normalized = path
        .to_ascii_lowercase()
        .replace("%2e", ".")
        .replace("%5c", "\\");
    if !matches!(url.scheme(), "http" | "https")
        || url.query().is_some()
        || url.fragment().is_some()
        || path.starts_with("//")
        || path.contains(['?', '#'])
        || normalized.contains('\\')
        || normalized
            .split('/')
            .any(|part| part == "." || part == "..")
    {
        return Err(crate::error::Error::lua(
            "url",
            "invalid_path",
            "expected an API path without authority, query, fragment or dot segments",
        ));
    }
    let combined = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    url.set_path(&combined);
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_path_preserves_prefix_and_encoded_segments() {
        assert_eq!(encode_path_segment(b"a/b +~"), "a%2Fb%20%2B~");
        assert_eq!(
            append_api_path("https://example.com/jellyfin/", "/Items/a%2Fb").unwrap(),
            "https://example.com/jellyfin/Items/a%2Fb"
        );
        for path in [
            "//evil.com/items",
            "../items",
            "/%2E%2e/items",
            "/x?token=y",
            "/x#z",
            "/x\\y",
        ] {
            assert!(append_api_path("https://example.com/jellyfin", path).is_err());
        }
    }
}
