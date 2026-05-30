use mlua::prelude::*;

pub fn create_query(lua: &Lua) -> LuaResult<LuaFunction> {
    lua.create_function(|lua, (html, selector): (String, String)| {
        let out = lua.create_table()?;
        let Ok(sel) = scraper::Selector::parse(&selector) else {
            return Ok(out);
        };
        let doc = scraper::Html::parse_document(&html);
        for (i, el) in doc.select(&sel).enumerate() {
            let t = lua.create_table()?;
            t.set("tag", el.value().name().to_string())?;
            t.set("text", el.text().collect::<String>())?;
            t.set("html", el.html())?;
            t.set("inner_html", el.inner_html())?;
            let attrs = lua.create_table()?;
            for (k, v) in el.value().attrs() {
                attrs.set(k.to_string(), v.to_string())?;
            }
            t.set("attrs", attrs)?;
            out.set(i + 1, t)?;
        }
        Ok(out)
    })
}
