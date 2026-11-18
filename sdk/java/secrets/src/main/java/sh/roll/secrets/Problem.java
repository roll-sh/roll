package sh.roll.secrets;

import java.util.Map;

/** A key that didn't resolve (a reference to nothing, a cycle, a link that failed). It holds no value. */
public record Problem(String key, String problem) {
    static Problem of(Map<String, Object> o) {
        return new Problem(Json.str(o, "key"), Json.str(o, "problem"));
    }
}
