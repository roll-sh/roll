package sh.roll.secrets.spring;

import org.springframework.boot.context.config.ConfigDataException;

/** A {@code roll-secrets:} import that failed startup. It never holds a value. */
public class RollSecretsConfigDataException extends ConfigDataException {
    private static final long serialVersionUID = 1L;

    RollSecretsConfigDataException(String message, Throwable cause) {
        super(message, cause);
    }
}
