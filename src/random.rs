use mlua::prelude::*;

const MAX_RANDOM_BYTES: usize = 4096;

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "hex",
        lua.create_function(|_, size: usize| {
            if size > MAX_RANDOM_BYTES {
                return Err(LuaError::external("random byte count exceeds limit"));
            }

            let mut bytes = vec![0; size];
            getrandom::fill(&mut bytes)
                .map_err(|error| LuaError::external(format!("random source failed: {error}")))?;

            let mut encoded = String::with_capacity(size * 2);
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for byte in bytes {
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
            Ok(encoded)
        })?,
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_requested_hex_length() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let value = module
            .get::<LuaFunction>("hex")
            .unwrap()
            .call::<String>(12)
            .unwrap();

        assert_eq!(value.len(), 24);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
