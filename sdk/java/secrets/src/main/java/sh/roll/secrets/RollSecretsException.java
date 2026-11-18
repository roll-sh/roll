package sh.roll.secrets;

/**
 * An answer that wasn't a success: its HTTP status, roll's code ({@code forbidden},
 * {@code unresolved}…) and message. It never holds a value. A request that never got an answer
 * (the network, a timeout, an interrupt) has status 0 and code {@code io} or {@code interrupted}.
 */
public class RollSecretsException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final int status;
    private final String code;

    public RollSecretsException(int status, String code, String message) {
        super(message);
        this.status = status;
        this.code = code;
    }

    RollSecretsException(int status, String code, String message, Throwable cause) {
        super(message, cause);
        this.status = status;
        this.code = code;
    }

    /** The HTTP status, or 0 when there was no answer. */
    public int status() {
        return status;
    }

    /** roll's code for the error, or null when the answer named none. */
    public String code() {
        return code;
    }
}
