package sh.roll.secrets;

import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;

/**
 * An environment's values. Dynamic keys hold a short-lived credential that ends with its lease
 * ({@link #leases()}; {@link RollSecrets#renew} keeps it); linked keys come from a link to a
 * resource. It can't be changed, and {@link #toString()} names keys, never values.
 */
public final class Values {
    private final Map<String, String> values;
    private final List<Problem> problems;
    private final List<String> leases;
    private final List<String> dynamic;
    private final List<String> linked;

    Values(Map<String, String> values, List<Problem> problems, List<String> leases, List<String> dynamic, List<String> linked) {
        this.values = Collections.unmodifiableMap(new LinkedHashMap<>(values));
        this.problems = List.copyOf(problems);
        this.leases = List.copyOf(leases);
        this.dynamic = List.copyOf(dynamic);
        this.linked = List.copyOf(linked);
    }

    /** An answer's values, or {@code plain} in place of its {@code values} (a sealed resolve's, opened). */
    static Values of(Map<String, Object> o, Map<String, Object> plain) {
        Map<String, Object> raw = plain != null ? plain : Json.obj(o, "values");
        Map<String, String> values = new LinkedHashMap<>();
        if (raw != null) {
            raw.forEach((k, v) -> {
                if (v instanceof String) {
                    values.put(k, (String) v);
                }
            });
        }
        List<Problem> problems = Json.objects(o, "problems").stream().map(Problem::of).collect(Collectors.toList());
        return new Values(values, problems, Json.strings(o, "leases"), Json.strings(o, "dynamic"), Json.strings(o, "linked"));
    }

    /** Every key's value. */
    public Map<String, String> values() {
        return values;
    }

    /** One key's value, or null when there's none. */
    public String get(String key) {
        return values.get(key);
    }

    /** Keys that didn't resolve. */
    public List<Problem> problems() {
        return problems;
    }

    /** The leases of the dynamic credentials issued for this answer. */
    public List<String> leases() {
        return leases;
    }

    /** Keys that hold a dynamic credential. */
    public List<String> dynamic() {
        return dynamic;
    }

    /** Keys that come from a link to a resource. */
    public List<String> linked() {
        return linked;
    }

    @Override
    public String toString() {
        return "Values{keys=" + values.keySet() + ", problems=" + problems + ", leases=" + leases
                + ", dynamic=" + dynamic + ", linked=" + linked + "}";
    }
}
