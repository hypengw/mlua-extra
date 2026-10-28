use mlua::prelude::*;
use roxmltree::{Document, Node, ParsingOptions};

fn element_table(lua: &Lua, node: Node<'_, '_>, depth: usize) -> LuaResult<LuaTable> {
    if depth > 128 {
        return Err(LuaError::external("XML nesting exceeds 128 elements"));
    }
    let table = lua.create_table()?;
    table.set("tag", node.tag_name().name())?;
    table.set("namespace", node.tag_name().namespace())?;
    let attrs = lua.create_table()?;
    for attr in node.attributes() {
        let prefix = attr.namespace().and_then(|ns| node.lookup_prefix(ns));
        let name = match prefix {
            Some(prefix) => format!("{prefix}:{}", attr.name()),
            None => attr.name().to_owned(),
        };
        attrs.set(name, attr.value())?;
    }
    table.set("attrs", attrs)?;
    let children = lua.create_table()?;
    let mut text = String::new();
    for child in node.children() {
        if child.is_element() {
            children.raw_push(element_table(lua, child, depth + 1)?)?;
        } else if child.is_text() {
            text.push_str(child.text().unwrap_or_default());
        }
    }
    table.set("text", text)?;
    table.set("children", children)?;
    Ok(table)
}

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let module = lua.create_table()?;
    module.set(
        "parse",
        lua.create_function(|lua, input: String| {
            if input.len() > 1024 * 1024 {
                return Err(LuaError::external("XML input exceeds 1 MiB"));
            }
            let document = Document::parse_with_options(
                &input,
                ParsingOptions {
                    allow_dtd: true,
                    nodes_limit: 10_000,
                    // No resolver: never load external entities or DTDs.
                    entity_resolver: None,
                },
            )
            .map_err(LuaError::external)?;
            element_table(lua, document.root_element(), 0)
        })?,
    )?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lua: &Lua, input: &str) -> LuaResult<LuaTable> {
        create_module(lua)?.get::<LuaFunction>("parse")?.call(input)
    }

    #[test]
    fn parses_doctype_attributes_entities_and_children() {
        let lua = Lua::new();
        let root = parse(
            &lua,
            r#"<?xml version="1.0"?>
            <!DOCTYPE wallpapers SYSTEM "https://invalid.invalid/background.dtd">
            <wallpapers><wallpaper deleted="false">
              <name xml:lang="zh_CN">Light &amp; Dark</name>
              <filename><![CDATA[/images/a&b.png]]></filename>
            </wallpaper></wallpapers>"#,
        )
        .unwrap();
        assert_eq!(root.get::<String>("tag").unwrap(), "wallpapers");
        let wallpaper = root
            .get::<LuaTable>("children")
            .unwrap()
            .get::<LuaTable>(1)
            .unwrap();
        assert_eq!(
            wallpaper
                .get::<LuaTable>("attrs")
                .unwrap()
                .get::<String>("deleted")
                .unwrap(),
            "false"
        );
        let children = wallpaper.get::<LuaTable>("children").unwrap();
        let name = children.get::<LuaTable>(1).unwrap();
        assert_eq!(name.get::<String>("text").unwrap(), "Light & Dark");
        assert_eq!(
            name.get::<LuaTable>("attrs")
                .unwrap()
                .get::<String>("xml:lang")
                .unwrap(),
            "zh_CN"
        );
        assert_eq!(
            children
                .get::<LuaTable>(2)
                .unwrap()
                .get::<String>("text")
                .unwrap(),
            "/images/a&b.png"
        );
    }

    #[test]
    fn rejects_external_entities_malformed_and_excessive_documents() {
        let lua = Lua::new();
        for input in [
            r#"<!DOCTYPE root [<!ENTITY secret SYSTEM "file:///etc/passwd">]><root>&secret;</root>"#.to_owned(),
            "<root><child></root>".to_owned(),
            format!("{}{}", "<a>".repeat(130), "</a>".repeat(130)),
            format!("<root>{}</root>", "<a/>".repeat(10_001)),
        ] {
            assert!(parse(&lua, &input).is_err());
        }
    }
}
