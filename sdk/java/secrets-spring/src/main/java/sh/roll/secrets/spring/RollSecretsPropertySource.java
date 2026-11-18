package sh.roll.secrets.spring;

import java.util.AbstractMap;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.TreeSet;
import org.springframework.core.env.SystemEnvironmentPropertySource;
import sh.roll.secrets.Problem;
import sh.roll.secrets.RollSecrets;
import sh.roll.secrets.RollSecretsException;
import sh.roll.secrets.Values;

/**
 * One environment and folder's values as a Spring property source, read with the client's
 * {@link RollSecrets#values(String, String, boolean)} (its cache, if it has one). Keys keep their
 * names ({@code DB_URL}) and bind as environment variables do, so {@code db.url} and
 * {@code @ConfigurationProperties("db")} find {@code DB_URL} too. Dynamic keys are left out unless
 * asked for (nothing renews their leases but a refreshing import's watch).
 *
 * <p>{@link #toString()} names the folder and counts the keys; no value is ever printed. To use it
 * without Spring Boot's {@code spring.config.import}:
 * {@code environment.getPropertySources().addFirst(RollSecretsPropertySource.load(client, "production", "/api", false))}.
 */
public final class RollSecretsPropertySource extends SystemEnvironmentPropertySource {
    private final String environment;
    private final String folder;
    private final boolean includeDynamic;
    private final Snapshot snapshot;

    /** {@code values} (an answer for {@code environment} and {@code folder}) as a property source. */
    public RollSecretsPropertySource(String environment, String folder, Values values, boolean includeDynamic) {
        this(environment, normalize(folder), includeDynamic, new Snapshot());
        snapshot.current = entries(values);
    }

    private RollSecretsPropertySource(String environment, String folder, boolean includeDynamic, Snapshot snapshot) {
        // Spring Boot binds a SystemEnvironmentPropertySource whose name ends in -systemEnvironment
        // as it binds environment variables (DB_URL is db.url); with any other name only exact names
        // and getProperty's own fallbacks would match.
        super("roll-secrets:" + Objects.requireNonNull(environment, "environment") + folder + "-systemEnvironment", snapshot);
        this.environment = environment;
        this.folder = folder;
        this.includeDynamic = includeDynamic;
        this.snapshot = snapshot;
    }

    /**
     * Reads {@code environment}'s values under {@code folder} ({@code ""} or {@code "/"} for the
     * root). Without {@code includeDynamic}, the read asks for no dynamic credential.
     *
     * @throws RollSecretsException when the read fails, or a key doesn't resolve ({@code unresolved})
     */
    public static RollSecretsPropertySource load(RollSecrets client, String environment, String folder, boolean includeDynamic) {
        return new RollSecretsPropertySource(environment, folder, client.values(environment, normalize(folder), includeDynamic), includeDynamic);
    }

    /** The environment's name. */
    public String environment() {
        return environment;
    }

    /** The folder ({@code ""} for the root). */
    public String folder() {
        return folder;
    }

    /**
     * Swaps in newer values for the same folder, all at once. Answers the keys added, changed or
     * removed (names only).
     */
    public Set<String> update(Values values) {
        Map<String, Object> next = entries(values);
        Map<String, Object> before = snapshot.current;
        snapshot.current = next;
        Set<String> changed = new TreeSet<>();
        for (Map.Entry<String, Object> e : next.entrySet()) {
            if (!Objects.equals(before.get(e.getKey()), e.getValue())) {
                changed.add(e.getKey());
            }
        }
        for (String k : before.keySet()) {
            if (!next.containsKey(k)) {
                changed.add(k);
            }
        }
        return changed;
    }

    private Map<String, Object> entries(Values values) {
        if (!values.problems().isEmpty()) {
            Problem p = values.problems().get(0);
            throw new RollSecretsException(409, "unresolved", "roll-secrets:" + label() + " doesn't resolve: " + p.key() + ": " + p.problem());
        }
        Map<String, Object> m = new LinkedHashMap<>(values.values());
        if (!includeDynamic) {
            // An older server may have issued them anyway: they still stay out.
            m.keySet().removeAll(values.dynamic());
        }
        return Collections.unmodifiableMap(m);
    }

    private String label() {
        return environment + folder;
    }

    static String normalize(String folder) {
        if (folder == null) {
            return "";
        }
        String f = folder.replaceAll("/+$", "");
        return f.isEmpty() ? "" : f.startsWith("/") ? f : "/" + f;
    }

    @Override
    public String toString() {
        return "RollSecretsPropertySource[roll-secrets:" + label() + ", " + snapshot.current.size() + " keys]";
    }

    /**
     * The map Spring reads, a view of the latest values: a refresh replaces them in one write, so a
     * lookup sees either the old values or the new ones. It prints its keys, never its values.
     */
    private static final class Snapshot extends AbstractMap<String, Object> {
        volatile Map<String, Object> current = Map.of();

        @Override
        public Set<Map.Entry<String, Object>> entrySet() {
            return current.entrySet();
        }

        @Override
        public Set<String> keySet() {
            return current.keySet();
        }

        @Override
        public Object get(Object key) {
            return current.get(key);
        }

        @Override
        public boolean containsKey(Object key) {
            return current.containsKey(key);
        }

        @Override
        public int size() {
            return current.size();
        }

        @Override
        public String toString() {
            return current.keySet().toString();
        }
    }
}
