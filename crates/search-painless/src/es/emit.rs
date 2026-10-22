//! `emit(...)` of runtime-field scripts, with `AbstractFieldScript`'s limits: at most 100
//! values per field (`checkMaxSize`) and, for keyword fields, 1 MiB of characters per document
//! (`checkMaxChars`). Values collect in the instance's state; composite scripts collect them per
//! sub-field. `parse(...)` of date fields reads `strict_date_optional_time||epoch_millis`.

use super::{es_iae, es_value, EsObj};
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::Value;
use crate::vm::{npe, Thrown, Vm, VR};

pub const MAX_VALUES: usize = 100;
pub const MAX_CHARS: u64 = 1024 * 1024;

fn check_max_size(vm: &Vm, current: usize, emit_frames: &[&str]) -> Result<(), Thrown> {
    if current >= MAX_VALUES {
        let mut frames = vec!["script.AbstractFieldScript.checkMaxSize(AbstractFieldScript.java:150)"];
        frames.extend_from_slice(emit_frames);
        return Err(es_iae(
            format!("Runtime field [{}] is emitting [{}] values while the maximum number of values allowed is [{MAX_VALUES}]", vm.st.field_name, current + 1),
            &frames,
        ));
    }
    Ok(())
}

fn push(vm: &mut Vm, v: Value, frame: &str) -> VR {
    check_max_size(vm, vm.st.emitted.len(), &[frame])?;
    vm.st.emitted.push(v);
    Ok(Value::Null)
}

fn e_long(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = Value::Long(crate::lib_lang::long(&a[0]));
    push(vm, v, "script.LongFieldScript$Emit.emit(LongFieldScript.java:103)")
}

fn e_double(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = Value::Double(crate::lib_lang::double(&a[0]));
    push(vm, v, "script.DoubleFieldScript$Emit.emit(DoubleFieldScript.java:150)")
}

fn e_date(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = Value::Long(crate::lib_lang::long(&a[0]));
    push(vm, v, "script.DateFieldScript$Emit.emit(DateFieldScript.java:120)")
}

fn e_boolean(vm: &mut Vm, a: &mut [Value]) -> VR {
    // BooleanFieldScript only counts trues and falses: no limit
    vm.st.emitted.push(Value::Bool(crate::lib_lang::boolean(&a[0])));
    Ok(Value::Null)
}

fn e_keyword(vm: &mut Vm, a: &mut [Value]) -> VR {
    check_max_size(vm, vm.st.emitted.len(), &["script.StringFieldScript$Emit.emit(StringFieldScript.java:158)"])?;
    let Value::Str(s) = &a[0] else {
        let mut t = npe("Cannot invoke \"String.length()\" because \"v\" is null");
        t.frames = super::server_frames(&["script.StringFieldScript.checkMaxChars(StringFieldScript.java:136)", "script.StringFieldScript$Emit.emit(StringFieldScript.java:159)"]);
        return Err(t);
    };
    vm.st.emit_chars += s.len16() as u64;
    if vm.st.emit_chars > MAX_CHARS {
        return Err(es_iae(
            format!(
                "Runtime field [{}] is emitting [{}] characters while the maximum number of values allowed is [{MAX_CHARS}]",
                vm.st.field_name, vm.st.emit_chars
            ),
            &["script.StringFieldScript.checkMaxChars(StringFieldScript.java:139)", "script.StringFieldScript$Emit.emit(StringFieldScript.java:159)"],
        ));
    }
    vm.st.emitted.push(a[0].clone());
    Ok(Value::Null)
}

fn e_ip(vm: &mut Vm, a: &mut [Value]) -> VR {
    check_max_size(vm, vm.st.emitted.len(), &["script.IpFieldScript$Emit.emit(IpFieldScript.java:182)"])?;
    let Value::Str(s) = &a[0] else {
        let mut t = npe("Cannot invoke \"String.getBytes(java.nio.charset.Charset)\" because \"ipString\" is null");
        t.frames = super::server_frames(&[
            "common.network.InetAddresses.forString(InetAddresses.java:430)",
            "script.IpFieldScript.emit(IpFieldScript.java:158)",
            "script.IpFieldScript$Emit.emit(IpFieldScript.java:183)",
        ]);
        return Err(t);
    };
    let ip = super::misc::parse_ip(s.as_str()).map_err(|mut t| {
        t.frames.push(format!("{}org.elasticsearch.script.IpFieldScript.emit(IpFieldScript.java:158)", super::SERVER));
        t.frames.push(format!("{}org.elasticsearch.script.IpFieldScript$Emit.emit(IpFieldScript.java:183)", super::SERVER));
        t
    })?;
    vm.st.emitted.push(Value::string(super::misc::ip_to_string(&ip)));
    Ok(Value::Null)
}

fn e_geo_point(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = es_value(EsObj::GeoPoint(crate::lib_lang::double(&a[0]), crate::lib_lang::double(&a[1])));
    push(vm, v, "script.GeoPointFieldScript$Emit.emit(GeoPointFieldScript.java:206)")
}

fn e_geometry(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[0].clone();
    push(vm, v, "script.GeometryFieldScript$Emit.emit(GeometryFieldScript.java:150)")
}

fn composite_emit(vm: &mut Vm, field: &str, v: Value) -> Result<(), Thrown> {
    let key = format!("{}.{field}", vm.st.field_name);
    let idx = match vm.st.emitted_fields.iter().position(|(k, _)| *k == key) {
        Some(i) => i,
        None => {
            vm.st.emitted_fields.push((key, Vec::new()));
            vm.st.emitted_fields.len() - 1
        }
    };
    let n = vm.st.emitted_fields[idx].1.len();
    check_max_size(vm, n, &["script.CompositeFieldScript.emit(CompositeFieldScript.java:82)", "script.CompositeFieldScript$EmitField.emit(CompositeFieldScript.java:108)"])?;
    vm.st.emitted_fields[idx].1.push(v);
    Ok(())
}

fn e_composite_field(vm: &mut Vm, a: &mut [Value]) -> VR {
    let field = a[0].to_java_string();
    composite_emit(vm, &field, a[1].clone())?;
    Ok(Value::Null)
}

fn e_composite_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = a[0].clone();
    if m.is_null() {
        return Err(npe("Cannot invoke \"java.util.Map.entrySet()\" because \"subfields\" is null"));
    }
    for (k, v) in crate::lib_coll::map_entries(vm, &m)? {
        composite_emit(vm, &k.to_java_string(), v)?;
    }
    Ok(Value::Null)
}

/// `parse(def)` of date runtime fields: epoch milliseconds.
fn e_parse(vm: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        let mut t = npe("Cannot invoke \"Object.toString()\" because \"str\" is null");
        t.frames = super::server_frames(&["script.DateFieldScript$Parse.parse(DateFieldScript.java:137)"]);
        return Err(t);
    }
    let s = a[0].to_java_string();
    match super::score::parse_es_date(&s) {
        Some(ms) => Ok(Value::Long(ms)),
        None => {
            let _ = vm;
            let mut t = super::score::date_parse_error(&s);
            t.frames.push(format!("{}org.elasticsearch.common.time.DateFormatter.parseMillis(DateFormatter.java:49)", super::SERVER));
            t.frames.push(format!("{}org.elasticsearch.script.DateFieldScript$Parse.parse(DateFieldScript.java:137)", super::SERVER));
            Err(t)
        }
    }
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.s(BOOLEAN_FIELD_SCRIPT, "void emit(boolean)", e_boolean);
    b.s(DATE_FIELD_SCRIPT, "void emit(long)", e_date);
    b.s(DATE_FIELD_SCRIPT, "long parse(def)", e_parse);
    b.s(DOUBLE_FIELD_SCRIPT, "void emit(double)", e_double);
    b.s(GEO_POINT_FIELD_SCRIPT, "void emit(double,double)", e_geo_point);
    b.s(GEOMETRY_FIELD_SCRIPT, "void emit(Object)", e_geometry);
    b.s(IP_FIELD_SCRIPT, "void emit(String)", e_ip);
    b.s(STRING_FIELD_SCRIPT, "void emit(String)", e_keyword);
    b.s(LONG_FIELD_SCRIPT, "void emit(long)", e_long);
    b.s(COMPOSITE_FIELD_SCRIPT, "void emit(String,Object)", e_composite_field);
    b.s(COMPOSITE_FIELD_SCRIPT, "void emit(Map)", e_composite_map);
}
