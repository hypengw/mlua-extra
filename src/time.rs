use mlua::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "now",
        lua.create_function(|_, ()| {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(millis)
        })?,
    )?;
    Ok(t)
}
