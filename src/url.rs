use mlua::prelude::*;
use url::Url;

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;

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
