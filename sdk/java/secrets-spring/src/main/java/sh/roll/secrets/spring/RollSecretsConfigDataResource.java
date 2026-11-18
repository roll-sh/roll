package sh.roll.secrets.spring;

import java.util.Objects;
import org.springframework.boot.context.config.ConfigDataResource;

/** One {@code roll-secrets:ENV[/FOLDER]} import: an environment and a folder. */
public final class RollSecretsConfigDataResource extends ConfigDataResource {
    private final String environment;
    private final String folder;
    private final boolean optional;
    final Settings settings;

    RollSecretsConfigDataResource(String environment, String folder, boolean optional, Settings settings) {
        super(optional);
        this.environment = environment;
        this.folder = folder;
        this.optional = optional;
        this.settings = settings;
    }

    /** The environment's name. */
    public String environment() {
        return environment;
    }

    /** The folder ({@code ""} for the root). */
    public String folder() {
        return folder;
    }

    /** Whether the import was {@code optional:}. */
    public boolean optional() {
        return optional;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof RollSecretsConfigDataResource r && r.environment.equals(environment) && r.folder.equals(folder);
    }

    @Override
    public int hashCode() {
        return Objects.hash(environment, folder);
    }

    @Override
    public String toString() {
        return RollSecretsConfigDataLocationResolver.PREFIX + environment + folder;
    }
}
