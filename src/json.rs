use crate::util::to_lua;
use mlua::prelude::*;

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "encode",
        lua.create_function(|_, v: LuaValue| {
            serde_json::to_string(&v).map_err(mlua::Error::external)
        })?,
    )?;
    t.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let v: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
            to_lua(&lua, &v)
        })?,
    )?;
    Ok(t)
}
