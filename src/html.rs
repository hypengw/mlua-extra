use mlua::prelude::*;
use scraper::{ElementRef, Html, Selector};

fn element_to_table(lua: &Lua, el: ElementRef) -> LuaResult<LuaTable> {
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
    Ok(t)
}

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;

    t.set(
        "query",
        lua.create_function(|lua, (html, selector): (String, String)| {
            let out = lua.create_table()?;
            let Ok(sel) = Selector::parse(&selector) else {
                return Ok(out);
            };
            let doc = Html::parse_document(&html);
            for (i, el) in doc.select(&sel).enumerate() {
                out.set(i + 1, element_to_table(lua, el)?)?;
            }
            Ok(out)
        })?,
    )?;

    t.set(
        "query_one",
        lua.create_function(|lua, (html, selector): (String, String)| {
            let Ok(sel) = Selector::parse(&selector) else {
                return Ok(LuaValue::Nil);
            };
            let doc = Html::parse_document(&html);
            match doc.select(&sel).next() {
                Some(el) => Ok(LuaValue::Table(element_to_table(lua, el)?)),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    t.set(
        "text",
        lua.create_function(|_, html: String| {
            let doc = Html::parse_document(&html);
            Ok(doc.root_element().text().collect::<String>())
        })?,
    )?;

    Ok(t)
}
