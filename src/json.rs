use crate::util::to_lua;
use mlua::prelude::*;

pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

pub fn parse(lua: &Lua, value: &str) -> LuaResult<LuaValue> {
    if value.len() > MAX_INPUT_BYTES {
        return Err(LuaError::external(format!(
            "json input exceeds {MAX_INPUT_BYTES}-byte limit"
        )));
    }
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(value) => json_to_lua(lua, &value),
        Err(_) => Ok(LuaValue::Nil),
    }
}

pub fn encode(value: &LuaValue) -> Option<String> {
    lua_to_json(value).and_then(|value| serde_json::to_string(&value).ok())
}

pub fn create_nullable_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "parse",
        lua.create_function(|lua, value: String| parse(lua, &value))?,
    )?;
    t.set(
        "encode",
        lua.create_function(|_, value: LuaValue| Ok(encode(&value)))?,
    )?;
    Ok(t)
}

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
            if s.len() > MAX_INPUT_BYTES {
                return Err(mlua::Error::external(format!(
                    "json input exceeds {MAX_INPUT_BYTES}-byte limit"
                )));
            }
            let v: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
            to_lua(lua, &v)
        })?,
    )?;
    Ok(t)
}

fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> LuaResult<LuaValue> {
    match value {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(value) => Ok(LuaValue::Boolean(*value)),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(LuaValue::Integer(value))
            } else {
                Ok(LuaValue::Number(value.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(value) => Ok(LuaValue::String(lua.create_string(value)?)),
        serde_json::Value::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, json_to_lua(lua, value)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        serde_json::Value::Object(values) => {
            let table = lua.create_table()?;
            for (key, value) in values {
                table.set(key.as_str(), json_to_lua(lua, value)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

fn lua_to_json(value: &LuaValue) -> Option<serde_json::Value> {
    match value {
        LuaValue::Nil => Some(serde_json::Value::Null),
        LuaValue::Boolean(value) => Some(serde_json::Value::Bool(*value)),
        LuaValue::Integer(value) => Some(serde_json::Value::Number((*value).into())),
        LuaValue::Number(value) => {
            serde_json::Number::from_f64(*value).map(serde_json::Value::Number)
        }
        LuaValue::String(value) => value
            .to_str()
            .ok()
            .map(|value| serde_json::Value::String(value.to_string())),
        LuaValue::Table(table) => {
            let len = table.raw_len();
            let mut all_int = len > 0;
            let mut count = 0;
            for pair in table.clone().pairs::<LuaValue, LuaValue>() {
                count += 1;
                let Ok((key, _)) = pair else {
                    all_int = false;
                    break;
                };
                if !matches!(key, LuaValue::Integer(_)) {
                    all_int = false;
                    break;
                }
            }
            if all_int && count == len {
                let mut values = Vec::with_capacity(len);
                for index in 1..=len {
                    let value: LuaValue = table.get(index).ok()?;
                    values.push(lua_to_json(&value)?);
                }
                Some(serde_json::Value::Array(values))
            } else {
                let mut values = serde_json::Map::new();
                for pair in table.clone().pairs::<LuaValue, LuaValue>() {
                    let (key, value) = pair.ok()?;
                    let key = match key {
                        LuaValue::String(value) => value.to_str().ok()?.to_string(),
                        LuaValue::Integer(value) => value.to_string(),
                        LuaValue::Number(value) => value.to_string(),
                        _ => continue,
                    };
                    values.insert(key, lua_to_json(&value)?);
                }
                Some(serde_json::Value::Object(values))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nullable_module_matches_lua_plugin_contract() {
        let lua = Lua::new();
        lua.globals()
            .set("json", create_nullable_module(&lua).unwrap())
            .unwrap();

        let (name, encoded): (String, String) = lua
            .load(r#"return json.parse('{"name":"wall"}').name, json.encode({1, 2})"#)
            .eval()
            .unwrap();
        assert_eq!(name, "wall");
        assert_eq!(encoded, "[1,2]");

        let (parsed, encoded): (LuaValue, LuaValue) = lua
            .load("return json.parse('{'), json.encode(function() end)")
            .eval()
            .unwrap();
        assert!(matches!(parsed, LuaValue::Nil));
        assert!(matches!(encoded, LuaValue::Nil));
    }

    #[test]
    fn parse_rejects_oversized_input() {
        let lua = Lua::new();
        let over = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(parse(&lua, &over).is_err());
    }

    #[test]
    fn decode_rejects_oversized_input() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let decode = module.get::<LuaFunction>("decode").unwrap();
        let over = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(decode.call::<LuaValue>(over).is_err());
    }
}
