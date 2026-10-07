use mlua::prelude::*;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 200_000;
const KIND: &str = "__mlua_extra_json_kind";

fn error(code: &'static str, message: impl Into<String>) -> LuaError {
    crate::error::Error::lua("json", code, message)
}

fn mark(lua: &Lua, table: &LuaTable, kind: &str) -> LuaResult<()> {
    let meta = lua.create_table()?;
    meta.raw_set(KIND, kind)?;
    table.set_metatable(Some(meta));
    Ok(())
}

#[derive(Default)]
struct Budget {
    nodes: usize,
    bytes: usize,
}
impl Budget {
    fn visit(&mut self, depth: usize, bytes: usize) -> LuaResult<()> {
        self.nodes += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if depth > MAX_DEPTH || self.nodes > MAX_NODES || self.bytes > MAX_BYTES {
            return Err(error("limit", "JSON resource limit exceeded"));
        }
        Ok(())
    }
}

struct Seed<'a> {
    lua: &'a Lua,
    budget: &'a mut Budget,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = LuaValue;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<LuaValue, D::Error> {
        self.budget.visit(self.depth, 0).map_err(D::Error::custom)?;
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = LuaValue;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<LuaValue, E> {
        Ok(LuaValue::NULL)
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<LuaValue, E> {
        Ok(LuaValue::Boolean(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<LuaValue, E> {
        Ok(LuaValue::Integer(value))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<LuaValue, E> {
        i64::try_from(value)
            .map(LuaValue::Integer)
            .map_err(E::custom)
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<LuaValue, E> {
        check_number(value).map_err(E::custom)?;
        Ok(LuaValue::Number(value))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<LuaValue, E> {
        self.lua
            .create_string(value)
            .map(LuaValue::String)
            .map_err(E::custom)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<LuaValue, A::Error> {
        let out = self.lua.create_table().map_err(A::Error::custom)?;
        mark(self.lua, &out, "array").map_err(A::Error::custom)?;
        let mut index = 1;
        while let Some(value) = seq.next_element_seed(Seed {
            lua: self.lua,
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            out.raw_set(index, value).map_err(A::Error::custom)?;
            index += 1;
        }
        Ok(LuaValue::Table(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LuaValue, A::Error> {
        let out = self.lua.create_table().map_err(A::Error::custom)?;
        mark(self.lua, &out, "object").map_err(A::Error::custom)?;
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(A::Error::custom("duplicate JSON object key"));
            }
            let value = map.next_value_seed(Seed {
                lua: self.lua,
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            out.raw_set(key, value).map_err(A::Error::custom)?;
        }
        Ok(LuaValue::Table(out))
    }
}

fn check_number(value: f64) -> LuaResult<()> {
    if !value.is_finite() || (value.fract() == 0.0 && value.abs() > 9_007_199_254_740_991.0) {
        Err(error(
            "invalid_number",
            "number is non-finite or not a safe floating-point integer",
        ))
    } else {
        Ok(())
    }
}

pub fn decode(lua: &Lua, bytes: &[u8]) -> LuaResult<LuaValue> {
    if bytes.len() > MAX_BYTES {
        return Err(error("limit", "JSON byte limit exceeded"));
    }
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let mut budget = Budget::default();
    let value = Seed {
        lua,
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut parser)
    .map_err(|e| error("decode", e.to_string()))?;
    parser.end().map_err(|e| error("decode", e.to_string()))?;
    Ok(value)
}

fn value_to_json(
    value: &LuaValue,
    depth: usize,
    budget: &mut Budget,
    active: &mut HashSet<usize>,
) -> LuaResult<serde_json::Value> {
    budget.visit(depth, 0)?;
    Ok(match value {
        LuaValue::LightUserData(p) if p.0.is_null() => serde_json::Value::Null,
        LuaValue::Boolean(v) => (*v).into(),
        LuaValue::Integer(v) => (*v).into(),
        LuaValue::Number(v) => {
            check_number(*v)?;
            serde_json::Value::Number(serde_json::Number::from_f64(*v).unwrap())
        }
        LuaValue::String(v) => {
            budget.visit(depth, v.as_bytes().len())?;
            v.to_str()?.to_string().into()
        }
        LuaValue::Table(t) => {
            let ptr = t.to_pointer() as usize;
            if !active.insert(ptr) {
                return Err(error("invalid_value", "cyclic JSON table"));
            }
            let kind: Option<String> = t
                .metatable()
                .map(|m| m.raw_get(KIND))
                .transpose()?
                .flatten();
            let mut entries = Vec::new();
            for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                if entries.len() >= MAX_NODES {
                    return Err(error("limit", "JSON node limit exceeded"));
                }
                entries.push(pair?);
            }
            let array = match kind.as_deref() {
                Some("array") => true,
                Some("object") => false,
                Some(_) => return Err(error("invalid_value", "unknown JSON table kind")),
                None if entries.is_empty() => {
                    return Err(error(
                        "invalid_value",
                        "empty table requires json.array or json.object",
                    ))
                }
                None => entries
                    .iter()
                    .all(|(k, _)| matches!(k, LuaValue::Integer(_))),
            };
            let out = if array {
                let len = entries.len();
                if entries.iter().any(|(k,_)| !matches!(k, LuaValue::Integer(i) if *i >= 1 && (*i as u64) <= len as u64)) {
                    return Err(error("invalid_value", "JSON array must be a dense sequence"));
                }
                let mut out = Vec::with_capacity(len);
                for i in 1..=len {
                    out.push(value_to_json(&t.raw_get(i)?, depth + 1, budget, active)?);
                }
                serde_json::Value::Array(out)
            } else {
                let mut out = serde_json::Map::new();
                for (k, v) in entries {
                    let LuaValue::String(k) = k else {
                        return Err(error("invalid_value", "JSON object keys must be strings"));
                    };
                    budget.visit(depth, k.as_bytes().len())?;
                    out.insert(
                        k.to_str()?.to_string(),
                        value_to_json(&v, depth + 1, budget, active)?,
                    );
                }
                serde_json::Value::Object(out)
            };
            active.remove(&ptr);
            out
        }
        _ => return Err(error("invalid_value", "value is not representable in JSON")),
    })
}

pub fn encode(value: &LuaValue) -> LuaResult<Vec<u8>> {
    let value = value_to_json(value, 0, &mut Budget::default(), &mut HashSet::new())?;
    struct Output(Vec<u8>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > MAX_BYTES {
                return Err(std::io::Error::other("JSON byte limit exceeded"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Output(Vec::new());
    serde_json::to_writer(&mut out, &value).map_err(|e| error("limit", e.to_string()))?;
    Ok(out.0)
}

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let module = lua.create_table()?;
    module.set("null", LuaValue::NULL)?;
    module.set(
        "kind",
        lua.create_function(|_, value: LuaValue| {
            Ok(match value {
                LuaValue::Table(table) => table
                    .metatable()
                    .and_then(|m| m.raw_get::<Option<String>>(KIND).ok().flatten()),
                LuaValue::LightUserData(v) if v.0.is_null() => Some("null".into()),
                _ => None,
            })
        })?,
    )?;
    for kind in ["array", "object"] {
        module.set(
            kind,
            lua.create_function(move |lua, table: LuaTable| {
                mark(lua, &table, kind)?;
                Ok(table)
            })?,
        )?;
    }
    module.set(
        "encode",
        lua.create_function(|lua, value: LuaValue| lua.create_string(encode(&value)?))?,
    )?;
    module.set(
        "decode",
        lua.create_function(|lua, value: LuaString| decode(lua, &value.as_bytes()))?,
    )?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_round_trip_preserves_json_shapes_and_integer_precision() {
        let lua = Lua::new();
        lua.globals()
            .set("J", create_module(&lua).unwrap())
            .unwrap();
        lua.load(r#"
            local v = J.decode('{"a":[],"o":{},"n":null,"i":9223372036854775807,"xs":[null,1,null]}')
            assert(v.n == J.null and v.missing == nil and #v.xs == 3)
            assert(v.i == math.maxinteger)
            local again = J.decode(J.encode(v))
            assert(again.xs[3] == J.null and J.encode(again.a) == '[]' and J.encode(again.o) == '{}')
            assert(J.encode(J.array{}) == '[]' and J.encode(J.object{}) == '{}')
            assert(J.encode({false, 0, ''}) == '[false,0,""]')
        "#).exec().unwrap();
    }
    #[test]
    fn strict_rejects_ambiguous_and_invalid_values() {
        let lua = Lua::new();
        lua.globals()
            .set("J", create_module(&lua).unwrap())
            .unwrap();
        lua.load(r#"
            for _, s in ipairs({'{"a":1,"a":2}', '9223372036854775808', '-9223372036854775809', '18446744073709551616', '{} trailing'}) do
                assert(not pcall(J.decode, s), s)
            end
            for _, v in ipairs({{}, {[2]=1}, {[1]=1, a=2}, 0/0, math.huge, 1e20, J.array{a=1}, J.object{1}}) do
                assert(not pcall(J.encode, v))
            end
            local cycle = {}; cycle.a = cycle
            assert(not pcall(J.encode, cycle))
            assert(not pcall(J.encode, nil))
            assert(not pcall(J.decode, string.rep('[', 66)..'0'..string.rep(']', 66)))
        "#).exec().unwrap();
    }
}
