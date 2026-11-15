//! `Character.UnicodeBlock` and `Character.UnicodeScript`.
//!
//! The block and script tables of Elasticsearch 9.5.5's JDK (Unicode 17) live in
//! `unicode_blocks.tsv`: block starts in code point order (gaps have no block), the script of
//! every range, the `UnicodeScript` constants in enum order with their ISO 15924 codes, the
//! names `UnicodeBlock.forName` accepts, and which constants Elasticsearch's allowlist exposes
//! as static fields. Blocks are values of their own (`Obj::UnicodeBlock`, compared by identity
//! like the JDK's singletons); scripts are enum constants.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{Obj, Value};
use crate::vm::{iae, Thrown, Vm, VR};

pub struct Data {
    /// (first code point, block index or `None` for a gap).
    starts: Vec<(u32, Option<u16>)>,
    /// Java constant names of the blocks.
    pub blocks: Vec<&'static str>,
    block_names: HashMap<String, u16>,
    /// (first code point, script ordinal).
    script_starts: Vec<(u32, u8)>,
    /// Constant names and ISO 15924 codes of the scripts, by ordinal.
    pub scripts: Vec<(&'static str, &'static str)>,
    allowed_blocks: Vec<&'static str>,
    allowed_scripts: Vec<&'static str>,
}

pub fn data() -> &'static Data {
    static D: OnceLock<Data> = OnceLock::new();
    D.get_or_init(|| {
        let mut d = Data {
            starts: Vec::new(),
            blocks: Vec::new(),
            block_names: HashMap::new(),
            script_starts: Vec::new(),
            scripts: Vec::new(),
            allowed_blocks: Vec::new(),
            allowed_scripts: Vec::new(),
        };
        let mut index: HashMap<&'static str, u16> = HashMap::new();
        let text: &'static str = include_str!("unicode_blocks.tsv");
        for line in text.lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            match f[0] {
                "B" => {
                    let start: u32 = f[1].parse().unwrap_or(0);
                    if f[2] == "null" {
                        d.starts.push((start, None));
                    } else {
                        let i = *index.entry(f[2]).or_insert_with(|| {
                            d.blocks.push(f[2]);
                            (d.blocks.len() - 1) as u16
                        });
                        d.starts.push((start, Some(i)));
                    }
                }
                "S" => d.script_starts.push((f[1].parse().unwrap_or(0), f[2].parse().unwrap_or(0))),
                "N" => d.scripts.push((f[2], f.get(3).copied().unwrap_or(""))),
                "K" => {
                    if let Some(&i) = index.get(f[1]) {
                        for name in &f[1..] {
                            d.block_names.insert(name.to_string(), i);
                        }
                    }
                }
                "A" if f[1] == "B" => d.allowed_blocks.push(f[2]),
                "A" if f[1] == "S" => d.allowed_scripts.push(f[2]),
                _ => {}
            }
        }
        d
    })
}

fn invalid_code_point(cp: i32, frame: &str) -> Thrown {
    iae(format!("Not a valid Unicode code point: 0x{:X}", cp as u32)).with_frames(&[frame])
}

/// `UnicodeBlock.of(codePoint)`: `None` for code points outside every block.
pub fn block_of(cp: u32) -> Option<u16> {
    let d = data();
    let i = d.starts.partition_point(|(s, _)| *s <= cp);
    if i == 0 { None } else { d.starts[i - 1].1 }
}

/// `UnicodeScript.of(codePoint)`.
pub fn script_of(cp: u32) -> u8 {
    let d = data();
    let i = d.script_starts.partition_point(|(s, _)| *s <= cp);
    if i == 0 { 0 } else { d.script_starts[i - 1].1 }
}

pub fn block_value(i: u16) -> Value {
    Value::obj(Obj::UnicodeBlock(i))
}

fn script_value(o: u8) -> Value {
    crate::time::enum_value(cls::UNICODE_SCRIPT, o)
}

fn code_point(v: &Value) -> i32 {
    crate::lib_lang::int(v)
}

fn ub_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let cp = code_point(&a[0]);
    if !(0..=0x10FFFF).contains(&cp) {
        return Err(invalid_code_point(cp, "java.base/java.lang.Character$UnicodeBlock.of(Character.java:4677)"));
    }
    Ok(block_of(cp as u32).map(block_value).unwrap_or(Value::Null))
}

fn ub_for_name(_: &mut Vm, a: &mut [Value]) -> VR {
    let name = crate::lib_lang::string(&a[0], "toUpperCase(java.util.Locale)")?.as_str().to_string();
    match data().block_names.get(&name.to_uppercase()) {
        Some(&i) => Ok(block_value(i)),
        None => Err(iae(format!("Not a valid block name: {name}")).with_frames(&["java.base/java.lang.Character$UnicodeBlock.forName(Character.java:4736)"])),
    }
}

fn ub_constant(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(block_value(crate::lib_lang::long(&a[0]) as u16))
}

fn us_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let cp = code_point(&a[0]);
    if !(0..=0x10FFFF).contains(&cp) {
        return Err(invalid_code_point(cp, "java.base/java.lang.Character$UnicodeScript.of(Character.java:9350)"));
    }
    Ok(script_value(script_of(cp as u32)))
}

fn no_constant(name: &str) -> Thrown {
    iae(format!("No enum constant java.lang.Character.UnicodeScript.{name}"))
}

fn us_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let name = crate::lib_lang::string(&a[0], "name")?.as_str().to_string();
    match data().scripts.iter().position(|(n, _)| *n == name) {
        Some(o) => Ok(script_value(o as u8)),
        None => Err(no_constant(&name)),
    }
}

fn us_for_name(_: &mut Vm, a: &mut [Value]) -> VR {
    let name = crate::lib_lang::string(&a[0], "toUpperCase(java.util.Locale)")?.as_str().to_uppercase();
    let d = data();
    match d.scripts.iter().position(|(_, iso)| iso.eq_ignore_ascii_case(&name)).or_else(|| d.scripts.iter().position(|(n, _)| *n == name)) {
        Some(o) => Ok(script_value(o as u8)),
        None => Err(no_constant(&name)),
    }
}

fn us_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = (0..data().scripts.len()).map(|o| script_value(o as u8)).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::UNICODE_SCRIPT), crate::value::ArrayData::Ref(items)))))
}

fn us_constant(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(script_value(crate::lib_lang::long(&a[0]) as u8))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(CHARACTER_SUBSET, &[OBJECT]);
    b.supers(UNICODE_BLOCK, &[CHARACTER_SUBSET, OBJECT]);
    b.supers(UNICODE_SCRIPT, &[ENUM, COMPARABLE, OBJECT]);
    let d = data();
    for name in &d.allowed_blocks {
        if let Some(i) = d.blocks.iter().position(|b| b == name) {
            b.sf_data(UNICODE_BLOCK, &format!("Character.UnicodeBlock {name}"), ub_constant, i as i64);
        }
    }
    b.s(UNICODE_BLOCK, "Character.UnicodeBlock forName(String)", ub_for_name);
    b.s(UNICODE_BLOCK, "Character.UnicodeBlock of(int)", ub_of);
    for name in &d.allowed_scripts {
        if let Some(o) = d.scripts.iter().position(|(n, _)| n == name) {
            b.sf_data(UNICODE_SCRIPT, &format!("Character.UnicodeScript {name}"), us_constant, o as i64);
        }
    }
    b.s(UNICODE_SCRIPT, "Character.UnicodeScript forName(String)", us_for_name);
    b.s(UNICODE_SCRIPT, "Character.UnicodeScript of(int)", us_of);
    b.s(UNICODE_SCRIPT, "Character.UnicodeScript valueOf(String)", us_value_of);
    b.s(UNICODE_SCRIPT, "Character.UnicodeScript[] values()", us_values);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_scripts() {
        let d = data();
        assert_eq!(d.blocks[block_of(65).unwrap() as usize], "BASIC_LATIN");
        assert_eq!(d.blocks[block_of(0x1F600).unwrap() as usize], "EMOTICONS");
        assert_eq!(block_of(12256), None);
        assert_eq!(d.scripts[script_of(65) as usize].0, "LATIN");
        assert_eq!(d.scripts[script_of(0x0416) as usize].0, "CYRILLIC");
        assert_eq!(d.block_names.get("BASIC LATIN").map(|&i| d.blocks[i as usize]), Some("BASIC_LATIN"));
    }
}
