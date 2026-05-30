use mlua::prelude::*;

pub fn create_encode(lua: &Lua) -> LuaResult<LuaFunction> {
    lua.create_function(|_, v: LuaValue| {
        serde_urlencoded::to_string(&v).map_err(mlua::Error::external)
    })
}
