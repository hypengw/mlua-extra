use mlua::prelude::*;

#[derive(Debug, Clone)]
pub struct Error {
    pub domain: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn lua(domain: &'static str, code: &'static str, message: impl Into<String>) -> LuaError {
        LuaError::external(Self {
            domain,
            code,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}: {}", self.domain, self.code, self.message)
    }
}
impl std::error::Error for Error {}

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let module = lua.create_table()?;
    module.set(
        "inspect",
        lua.create_function(|lua, value: LuaValue| {
            let LuaValue::Error(error) = value else {
                return Ok(None);
            };
            let mut error = error.as_ref();
            while let LuaError::CallbackError { cause, .. } | LuaError::WithContext { cause, .. } =
                error
            {
                error = cause;
            }
            let Some(error) = error.downcast_ref::<Error>() else {
                return Ok(None);
            };
            let out = lua.create_table()?;
            out.set("domain", error.domain)?;
            out.set("code", error.code)?;
            out.set("message", error.message.as_str())?;
            Ok(Some(out))
        })?,
    )?;
    Ok(module)
}
