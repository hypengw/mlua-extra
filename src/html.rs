use mlua::prelude::*;
use scraper::{ElementRef, Html, Selector};

pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MATCHES: usize = 10_000;

// Returns the table plus its serialized byte size.
fn element_to_table(lua: &Lua, el: ElementRef) -> LuaResult<(LuaTable, usize)> {
    let t = lua.create_table()?;
    let tag = el.value().name().to_string();
    let text = el.text().collect::<String>();
    let html = el.html();
    let inner_html = el.inner_html();
    let mut bytes = tag.len() + text.len() + html.len() + inner_html.len();
    t.set("tag", tag)?;
    t.set("text", text)?;
    t.set("html", html)?;
    t.set("inner_html", inner_html)?;
    let attrs = lua.create_table()?;
    for (k, v) in el.value().attrs() {
        bytes += k.len() + v.len();
        attrs.set(k.to_string(), v.to_string())?;
    }
    t.set("attrs", attrs)?;
    Ok((t, bytes))
}

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;

    t.set(
        "query",
        lua.create_function(|lua, (html, selector): (String, String)| {
            if html.len() > MAX_INPUT_BYTES {
                return Err(mlua::Error::external(format!(
                    "html input exceeds {MAX_INPUT_BYTES}-byte limit"
                )));
            }
            let out = lua.create_table()?;
            let Ok(sel) = Selector::parse(&selector) else {
                return Ok(out);
            };
            let doc = Html::parse_document(&html);
            let mut output_bytes = 0usize;
            for (i, el) in doc.select(&sel).enumerate() {
                if i >= MAX_MATCHES {
                    return Err(mlua::Error::external(format!(
                        "html query exceeds {MAX_MATCHES}-match limit"
                    )));
                }
                let (row, bytes) = element_to_table(lua, el)?;
                output_bytes += bytes;
                if output_bytes > MAX_OUTPUT_BYTES {
                    return Err(mlua::Error::external(format!(
                        "html query output exceeds {MAX_OUTPUT_BYTES}-byte limit"
                    )));
                }
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;

    t.set(
        "query_one",
        lua.create_function(|lua, (html, selector): (String, String)| {
            if html.len() > MAX_INPUT_BYTES {
                return Err(mlua::Error::external(format!(
                    "html input exceeds {MAX_INPUT_BYTES}-byte limit"
                )));
            }
            let Ok(sel) = Selector::parse(&selector) else {
                return Ok(LuaValue::Nil);
            };
            let doc = Html::parse_document(&html);
            match doc.select(&sel).next() {
                Some(el) => {
                    let (row, bytes) = element_to_table(lua, el)?;
                    if bytes > MAX_OUTPUT_BYTES {
                        return Err(mlua::Error::external(format!(
                            "html query output exceeds {MAX_OUTPUT_BYTES}-byte limit"
                        )));
                    }
                    Ok(LuaValue::Table(row))
                }
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    t.set(
        "text",
        lua.create_function(|_, html: String| {
            if html.len() > MAX_INPUT_BYTES {
                return Err(mlua::Error::external(format!(
                    "html input exceeds {MAX_INPUT_BYTES}-byte limit"
                )));
            }
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

    #[test]
    fn query_rejects_oversized_input() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let query = module.get::<LuaFunction>("query").unwrap();
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        assert!(query.call::<LuaTable>((big, "div")).is_err());
    }

    #[test]
    fn query_enforces_match_limit() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let query = module.get::<LuaFunction>("query").unwrap();
        let at_cap = "<a></a>".repeat(MAX_MATCHES);
        let rows = query.call::<LuaTable>((at_cap, "a")).unwrap();
        assert_eq!(rows.raw_len() as usize, MAX_MATCHES);
        let over = "<a></a>".repeat(MAX_MATCHES + 1);
        assert!(query.call::<LuaTable>((over, "a")).is_err());
    }

    #[test]
    fn query_rejects_oversized_output() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let query = module.get::<LuaFunction>("query").unwrap();
        // A few large elements: well under the match limit, but their copied text +
        // html + inner_html blow past the output budget.
        let blob = "y".repeat(140 * 1024);
        let doc = format!("<p>{blob}</p>").repeat(50);
        assert!(query.call::<LuaTable>((doc, "p")).is_err());
    }
}
