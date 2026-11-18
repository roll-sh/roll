package sh.roll.secrets;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The JSON the client reads and writes: objects are {@code Map<String, Object>}, arrays
 * {@code List<Object>}, numbers {@code Long} (or {@code Double} with a fraction or exponent),
 * strings, booleans and {@code null}. Errors never quote the input: it may hold a value.
 */
final class Json {
    private static final int MAX_DEPTH = 64;

    private final String s;
    private int i;

    private Json(String s) {
        this.s = s;
    }

    static Object parse(String text) {
        Json p = new Json(text);
        p.space();
        Object v = p.value(0);
        p.space();
        if (p.i != p.s.length()) {
            throw p.fail();
        }
        return v;
    }

    /** {@code text} as an object; anything else is an error. */
    @SuppressWarnings("unchecked")
    static Map<String, Object> object(String text) {
        Object v = parse(text);
        if (!(v instanceof Map)) {
            throw new IllegalArgumentException("the answer isn't a JSON object");
        }
        return (Map<String, Object>) v;
    }

    private IllegalArgumentException fail() {
        return new IllegalArgumentException("the answer isn't JSON (at offset " + i + ")");
    }

    private void space() {
        while (i < s.length()) {
            char c = s.charAt(i);
            if (c != ' ' && c != '\t' && c != '\n' && c != '\r') {
                return;
            }
            i++;
        }
    }

    private Object value(int depth) {
        if (depth > MAX_DEPTH || i >= s.length()) {
            throw fail();
        }
        char c = s.charAt(i);
        switch (c) {
            case '{':
                return object(depth + 1);
            case '[':
                return array(depth + 1);
            case '"':
                return string();
            case 't':
                return literal("true", Boolean.TRUE);
            case 'f':
                return literal("false", Boolean.FALSE);
            case 'n':
                return literal("null", null);
            default:
                if (c == '-' || (c >= '0' && c <= '9')) {
                    return number();
                }
                throw fail();
        }
    }

    private Object literal(String word, Object v) {
        if (!s.startsWith(word, i)) {
            throw fail();
        }
        i += word.length();
        return v;
    }

    private Map<String, Object> object(int depth) {
        Map<String, Object> out = new LinkedHashMap<>();
        i++;
        space();
        if (i < s.length() && s.charAt(i) == '}') {
            i++;
            return out;
        }
        while (true) {
            space();
            if (i >= s.length() || s.charAt(i) != '"') {
                throw fail();
            }
            String k = string();
            space();
            if (i >= s.length() || s.charAt(i) != ':') {
                throw fail();
            }
            i++;
            space();
            out.put(k, value(depth));
            space();
            if (i >= s.length()) {
                throw fail();
            }
            char c = s.charAt(i++);
            if (c == '}') {
                return out;
            }
            if (c != ',') {
                throw fail();
            }
        }
    }

    private List<Object> array(int depth) {
        List<Object> out = new ArrayList<>();
        i++;
        space();
        if (i < s.length() && s.charAt(i) == ']') {
            i++;
            return out;
        }
        while (true) {
            space();
            out.add(value(depth));
            space();
            if (i >= s.length()) {
                throw fail();
            }
            char c = s.charAt(i++);
            if (c == ']') {
                return out;
            }
            if (c != ',') {
                throw fail();
            }
        }
    }

    private String string() {
        StringBuilder b = new StringBuilder();
        i++;
        while (true) {
            if (i >= s.length()) {
                throw fail();
            }
            char c = s.charAt(i++);
            if (c == '"') {
                return b.toString();
            }
            if (c < 0x20) {
                throw fail();
            }
            if (c != '\\') {
                b.append(c);
                continue;
            }
            if (i >= s.length()) {
                throw fail();
            }
            char e = s.charAt(i++);
            switch (e) {
                case '"': b.append('"'); break;
                case '\\': b.append('\\'); break;
                case '/': b.append('/'); break;
                case 'b': b.append('\b'); break;
                case 'f': b.append('\f'); break;
                case 'n': b.append('\n'); break;
                case 'r': b.append('\r'); break;
                case 't': b.append('\t'); break;
                case 'u':
                    if (i + 4 > s.length()) {
                        throw fail();
                    }
                    try {
                        b.append((char) Integer.parseInt(s.substring(i, i + 4), 16));
                    } catch (NumberFormatException x) {
                        throw fail();
                    }
                    i += 4;
                    break;
                default:
                    throw fail();
            }
        }
    }

    private Object number() {
        int start = i;
        boolean integral = true;
        if (s.charAt(i) == '-') {
            i++;
        }
        while (i < s.length()) {
            char c = s.charAt(i);
            if (c >= '0' && c <= '9') {
                i++;
            } else if (c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-') {
                integral = false;
                i++;
            } else {
                break;
            }
        }
        String n = s.substring(start, i);
        try {
            if (integral) {
                try {
                    return Long.parseLong(n);
                } catch (NumberFormatException tooBig) {
                    return Double.parseDouble(n);
                }
            }
            return Double.parseDouble(n);
        } catch (NumberFormatException x) {
            throw fail();
        }
    }

    static String write(Object v) {
        StringBuilder b = new StringBuilder();
        write(b, v);
        return b.toString();
    }

    private static void write(StringBuilder b, Object v) {
        if (v == null) {
            b.append("null");
        } else if (v instanceof String) {
            quote(b, (String) v);
        } else if (v instanceof Boolean || v instanceof Long || v instanceof Integer) {
            b.append(v);
        } else if (v instanceof Double && Double.isFinite((Double) v)) {
            b.append(v);
        } else if (v instanceof Map) {
            b.append('{');
            boolean first = true;
            for (Map.Entry<?, ?> e : ((Map<?, ?>) v).entrySet()) {
                if (e.getValue() == null) {
                    continue; // as the other clients: an unset field isn't sent
                }
                if (!first) {
                    b.append(',');
                }
                first = false;
                quote(b, (String) e.getKey());
                b.append(':');
                write(b, e.getValue());
            }
            b.append('}');
        } else if (v instanceof List) {
            b.append('[');
            boolean first = true;
            for (Object x : (List<?>) v) {
                if (!first) {
                    b.append(',');
                }
                first = false;
                write(b, x);
            }
            b.append(']');
        } else {
            throw new IllegalArgumentException("can't write a " + v.getClass().getSimpleName() + " as JSON");
        }
    }

    private static void quote(StringBuilder b, String s) {
        b.append('"');
        for (int k = 0; k < s.length(); k++) {
            char c = s.charAt(k);
            switch (c) {
                case '"': b.append("\\\""); break;
                case '\\': b.append("\\\\"); break;
                case '\n': b.append("\\n"); break;
                case '\r': b.append("\\r"); break;
                case '\t': b.append("\\t"); break;
                default:
                    if (c < 0x20) {
                        b.append(String.format("\\u%04x", (int) c));
                    } else {
                        b.append(c);
                    }
            }
        }
        b.append('"');
    }

    // Reading fields of a parsed answer.

    static String str(Map<String, Object> o, String k) {
        Object v = o.get(k);
        return v instanceof String ? (String) v : null;
    }

    static long num(Map<String, Object> o, String k) {
        Object v = o.get(k);
        return v instanceof Number ? ((Number) v).longValue() : 0;
    }

    static List<String> strings(Map<String, Object> o, String k) {
        Object v = o.get(k);
        List<String> out = new ArrayList<>();
        if (v instanceof List) {
            for (Object x : (List<?>) v) {
                if (x instanceof String) {
                    out.add((String) x);
                }
            }
        }
        return out;
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> obj(Map<String, Object> o, String k) {
        Object v = o.get(k);
        return v instanceof Map ? (Map<String, Object>) v : null;
    }

    @SuppressWarnings("unchecked")
    static List<Map<String, Object>> objects(Map<String, Object> o, String k) {
        Object v = o.get(k);
        List<Map<String, Object>> out = new ArrayList<>();
        if (v instanceof List) {
            for (Object x : (List<?>) v) {
                if (x instanceof Map) {
                    out.add((Map<String, Object>) x);
                }
            }
        }
        return out;
    }
}
