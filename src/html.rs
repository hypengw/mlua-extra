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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_accepts_selector_lists_and_preserves_attributes() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let query = module.get::<LuaFunction>("query").unwrap();
        let elements = query
            .call::<LuaTable>((
                r#"<button id="account"></button>
                    <div class="option add selected" id="active"></div>
                    <div class="option" id="inactive"></div>"#,
                "#account, #active, #missing",
            ))
            .unwrap();

        assert_eq!(elements.raw_len(), 2);
        let active = elements.get::<LuaTable>(2).unwrap();
        let attrs = active.get::<LuaTable>("attrs").unwrap();
        assert_eq!(attrs.get::<String>("id").unwrap(), "active");
        assert_eq!(attrs.get::<String>("class").unwrap(), "option add selected");
    }
}
