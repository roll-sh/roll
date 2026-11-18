package sh.roll.secrets.spring;

import java.util.Set;
import org.springframework.context.ApplicationEvent;

/**
 * Published when a refreshing import ({@code roll.secrets.refresh=true}) swapped in new values:
 * {@code Environment} lookups see them from then on. Beans that copied a value at startup
 * ({@code @Value}, {@code @ConfigurationProperties}) don't; listen for this to rebuild them. It names
 * the keys that changed, never their values.
 */
public class RollSecretsRefreshedEvent extends ApplicationEvent {
    private static final long serialVersionUID = 1L;

    private final transient Set<String> keys;

    public RollSecretsRefreshedEvent(RollSecretsPropertySource source, Set<String> keys) {
        super(source);
        this.keys = Set.copyOf(keys);
    }

    /** The property source that changed. */
    public RollSecretsPropertySource getPropertySource() {
        return (RollSecretsPropertySource) getSource();
    }

    /** The keys added, changed or removed. */
    public Set<String> keys() {
        return keys;
    }

    @Override
    public String toString() {
        return "RollSecretsRefreshedEvent[" + getSource() + ", changed " + keys + "]";
    }
}
