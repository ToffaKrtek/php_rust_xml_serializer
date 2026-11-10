use ext_php_rs::convert::IntoZval;
use ext_php_rs::exception::PhpException;
use ext_php_rs::flags::DataType;
use ext_php_rs::prelude::*;
use ext_php_rs::types::{ArrayKey, ZendHashTable, Zval};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use roxmltree::{Document, Node};
use std::collections::HashMap;
use std::io::Cursor;

type PhpResult<T> = Result<T, PhpException>;

#[derive(Debug, Clone)]
pub enum XmlValue {
    Text(String),
    Map(HashMap<String, XmlValue>),
    List(Vec<XmlValue>),
}

impl IntoZval for XmlValue {
    const TYPE: DataType = DataType::Mixed;
    const NULLABLE: bool = false;

    fn set_zval(self, zv: &mut Zval, persistent: bool) -> ext_php_rs::error::Result<()> {
        match self {
            XmlValue::Text(s) => {
                zv.set_string(&s, persistent)?;
                Ok(())
            }
            XmlValue::Map(m) => {
                let mut ht = ZendHashTable::new();
                for (k, v) in m {
                    ht.insert(k.as_str(), v)?;
                }
                zv.set_hashtable(ht);
                Ok(())
            }
            XmlValue::List(vec) => {
                let mut ht = ZendHashTable::new();
                for v in vec {
                    ht.push(v)?;
                }
                zv.set_hashtable(ht);
                Ok(())
            }
        }
    }
}

fn node_to_xml_value(node: &Node) -> XmlValue {
    if node.is_text() {
        return XmlValue::Text(node.text().unwrap_or("").to_string());
    }

    if node.is_element() {
        let mut map = HashMap::new();

        let attrs: Vec<_> = node.attributes().collect();
        if !attrs.is_empty() {
            let mut attr_map = HashMap::new();
            for attr in attrs {
                attr_map.insert(
                    attr.name().to_string(),
                    XmlValue::Text(attr.value().to_string()),
                );
            }
            map.insert("@attributes".to_string(), XmlValue::Map(attr_map));
        }

        let mut text_parts = Vec::new();
        let mut children_map: HashMap<String, Vec<XmlValue>> = HashMap::new();

        for child in node.children() {
            if child.is_text() {
                let txt = child.text().unwrap_or("").trim();
                if !txt.is_empty() {
                    text_parts.push(txt);
                }
            } else if child.is_element() {
                let tag = child.tag_name().name().to_string();
                let val = node_to_xml_value(&child);
                children_map.entry(tag).or_default().push(val);
            }
        }

        if !text_parts.is_empty() {
            map.insert("#text".to_string(), XmlValue::Text(text_parts.join(" ")));
        }

        for (tag, mut vals) in children_map {
            if vals.len() == 1 {
                map.insert(tag, vals.remove(0));
            } else {
                map.insert(tag, XmlValue::List(vals));
            }
        }

        return XmlValue::Map(map);
    }

    XmlValue::Text("".to_string())
}

#[php_function]
pub fn xml_decode(xml: &str) -> PhpResult<XmlValue> {
    let doc = Document::parse(xml)
        .map_err(|e| PhpException::default(format!("XML parse error: {}", e)))?;
    let root = doc.root_element();
    Ok(node_to_xml_value(&root))
}

fn zval_to_xml_value(zv: &Zval) -> PhpResult<XmlValue> {
    if let Some(s) = zv.string() {
        return Ok(XmlValue::Text(s.to_string()));
    }
    if let Some(l) = zv.long() {
        return Ok(XmlValue::Text(l.to_string()));
    }
    if let Some(d) = zv.double() {
        return Ok(XmlValue::Text(d.to_string()));
    }
    if let Some(b) = zv.bool() {
        return Ok(XmlValue::Text(if b { "1" } else { "0" }.to_string()));
    }

    if let Some(ht) = zv.array() {
        let mut is_list = true;
        let mut expected_idx = 0i64;

        for (key, _) in ht.iter() {
            if let ArrayKey::Long(idx) = key {
                if idx != expected_idx {
                    is_list = false;
                    break;
                }
                expected_idx += 1;
            } else {
                is_list = false;
                break;
            }
        }

        if is_list && expected_idx > 0 {
            let mut list = Vec::new();
            for (_, val) in ht.iter() {
                list.push(zval_to_xml_value(val)?);
            }
            return Ok(XmlValue::List(list));
        } else {
            let mut map = HashMap::new();
            for (key, val) in ht.iter() {
                // ArrayKey реализует трейт Display, поэтому мы можем просто вызвать .to_string()
                // Это элегантно обрабатывает все варианты: Long, String, Str и ZendString
                let k = key.to_string();
                map.insert(k, zval_to_xml_value(val)?);
            }
            return Ok(XmlValue::Map(map));
        }
    }

    Ok(XmlValue::Text("".to_string()))
}

fn write_xml_value(
    data: &XmlValue,
    tag_name: &str,
    writer: &mut Writer<Cursor<Vec<u8>>>,
) -> PhpResult<()> {
    match data {
        XmlValue::Text(s) => {
            let start = BytesStart::new(tag_name);
            writer
                .write_event(Event::Start(start))
                .map_err(|e| PhpException::default(e.to_string()))?;
            writer
                .write_event(Event::Text(BytesText::new(s)))
                .map_err(|e| PhpException::default(e.to_string()))?;
            writer
                .write_event(Event::End(BytesEnd::new(tag_name)))
                .map_err(|e| PhpException::default(e.to_string()))?;
        }
        XmlValue::Map(map) => {
            let mut start = BytesStart::new(tag_name);
            let mut text_content: Option<String> = None;
            let mut children: Vec<(&String, &XmlValue)> = Vec::new();

            for (k, v) in map {
                if k == "@attributes" {
                    if let XmlValue::Map(attrs) = v {
                        for (attr_k, attr_v) in attrs {
                            let attr_str: &str = if let XmlValue::Text(s) = attr_v {
                                s
                            } else {
                                "true"
                            };
                            // Передаем кортеж (&str, &str), что точно соответствует реализации From для Attribute
                            start.push_attribute((attr_k.as_str(), attr_str));
                        }
                    }
                } else if k == "#text" {
                    if let XmlValue::Text(s) = v {
                        text_content = Some(s.clone());
                    }
                } else {
                    children.push((k, v));
                }
            }

            writer
                .write_event(Event::Start(start))
                .map_err(|e| PhpException::default(e.to_string()))?;

            if let Some(txt) = text_content {
                writer
                    .write_event(Event::Text(BytesText::new(&txt)))
                    .map_err(|e| PhpException::default(e.to_string()))?;
            }

            for (child_tag, child_data) in children {
                match child_data {
                    XmlValue::List(list) => {
                        for item in list {
                            write_xml_value(item, child_tag, writer)?;
                        }
                    }
                    _ => {
                        write_xml_value(child_data, child_tag, writer)?;
                    }
                }
            }

            writer
                .write_event(Event::End(BytesEnd::new(tag_name)))
                .map_err(|e| PhpException::default(e.to_string()))?;
        }
        XmlValue::List(list) => {
            for item in list {
                write_xml_value(item, tag_name, writer)?;
            }
        }
    }
    Ok(())
}

#[php_function]
pub fn xml_encode(data: &Zval, root_tag: Option<String>) -> PhpResult<String> {
    let root = root_tag.unwrap_or_else(|| "root".to_string());
    let php_data = zval_to_xml_value(data)?;

    let mut writer = Writer::new(Cursor::new(Vec::new()));
    let decl = BytesDecl::new("1.0", Some("UTF-8"), None);
    writer
        .write_event(Event::Decl(decl))
        .map_err(|e| PhpException::default(e.to_string()))?;

    write_xml_value(&php_data, &root, &mut writer)?;

    let result = writer.into_inner().into_inner();
    String::from_utf8(result)
        .map_err(|e| PhpException::default(format!("UTF-8 encoding error: {}", e)))
}

#[php_module]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
}
