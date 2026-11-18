package sh.roll.secrets.spring;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.springframework.boot.context.properties.bind.Binder;
import sh.roll.secrets.RollSecrets;
import sh.roll.secrets.RollSecretsException;

/**
 * The {@code roll.secrets.*} settings. What isn't set comes from the client's own environment
 * defaults ({@code ROLL_API_URL}, {@code ROLL_TOKEN}, {@code ROLL_PROJECT}, {@code ROLL_IDENTITY},
 * {@code ROLL_OIDC_TOKEN[_FILE]}). It has no {@code toString}: it holds the token.
 */
final class Settings {
    final String apiUrl;
    final String token;
    final String project;
    final String identity;
    final String oidcTokenFile;
    final boolean includeDynamic;
    final boolean refresh;

    private Settings(Binder b) {
        apiUrl = string(b, "api-url");
        token = string(b, "token");
        project = string(b, "project");
        identity = string(b, "identity");
        oidcTokenFile = string(b, "oidc-token-file");
        includeDynamic = b.bind("roll.secrets.include-dynamic", Boolean.class).orElse(false);
        refresh = b.bind("roll.secrets.refresh", Boolean.class).orElse(false);
    }

    static Settings bind(Binder b) {
        return new Settings(b);
    }

    private static String string(Binder b, String name) {
        String v = b.bind("roll.secrets." + name, String.class).orElse(null);
        return v == null || v.isBlank() ? null : v.trim();
    }

    /** A client for these settings. */
    RollSecrets client() {
        RollSecrets.Builder c = RollSecrets.builder();
        if (apiUrl != null) {
            c.api(apiUrl);
        }
        if (token != null) {
            c.token(token);
        } else if (identity != null) {
            c.token(null); // an identity set here wins over ROLL_TOKEN
        }
        if (project != null) {
            c.project(project);
        }
        if (identity != null) {
            c.identity(identity);
        }
        if (oidcTokenFile != null) {
            Path file = Path.of(oidcTokenFile);
            // Read for each login: Kubernetes replaces a projected token on disk.
            c.oidcToken(() -> {
                try {
                    return Files.readString(file);
                } catch (IOException e) {
                    throw new RollSecretsException(0, "io", "roll.secrets.oidc-token-file can't be read");
                }
            });
        }
        return c.build();
    }
}
