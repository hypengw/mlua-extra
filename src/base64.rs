use base64::Engine as _;
use mlua::prelude::*;

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "decode",
        lua.create_function(|lua, value: String| {
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(value.as_bytes())
                .or_else(|_| base64::engine::general_purpose::STANDARD.decode(value.as_bytes()))
                .map_err(LuaError::external)?;
            lua.create_string(decoded)
        })?,
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_url_safe_and_standard_values() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let decode = module.get::<LuaFunction>("decode").unwrap();

        assert_eq!(decode.call::<LuaString>("_w").unwrap().as_bytes(), [0xff]);
        assert_eq!(decode.call::<LuaString>("/w==").unwrap().as_bytes(), [0xff]);
        assert!(decode.call::<LuaString>("!").is_err());
    }
}
