package sh.roll.secrets.spring;

import java.util.List;
import org.springframework.boot.context.config.ConfigDataLocation;
import org.springframework.boot.context.config.ConfigDataLocationResolver;
import org.springframework.boot.context.config.ConfigDataLocationResolverContext;

/**
 * Resolves {@code spring.config.import=roll-secrets:ENV[/FOLDER]} (or {@code optional:roll-secrets:…})
 * into one environment and folder, with the {@code roll.secrets.*} settings bound at that point.
 */
public class RollSecretsConfigDataLocationResolver implements ConfigDataLocationResolver<RollSecretsConfigDataResource> {
    static final String PREFIX = "roll-secrets:";

    @Override
    public boolean isResolvable(ConfigDataLocationResolverContext context, ConfigDataLocation location) {
        return location.hasPrefix(PREFIX);
    }

    @Override
    public List<RollSecretsConfigDataResource> resolve(ConfigDataLocationResolverContext context, ConfigDataLocation location) {
        String spec = location.getNonPrefixedValue(PREFIX).trim();
        int slash = spec.indexOf('/');
        String env = slash < 0 ? spec : spec.substring(0, slash);
        String folder = RollSecretsPropertySource.normalize(slash < 0 ? "" : spec.substring(slash));
        if (env.isEmpty()) {
            // A typo, not a missing folder: optional: doesn't excuse it.
            throw new RollSecretsConfigDataException("'" + location + "' names no environment: write roll-secrets:ENV or roll-secrets:ENV/FOLDER", null);
        }
        return List.of(new RollSecretsConfigDataResource(env, folder, location.isOptional(), Settings.bind(context.getBinder())));
    }
}
