package sh.roll.secrets;

import java.time.Duration;
import java.util.Objects;
import java.util.function.Consumer;

/** What {@link RollSecrets#watch(String, WatchOptions)} follows and how. */
public final class WatchOptions {
    final Consumer<Values> onChange;
    String path = "";
    String consumer;
    Duration every = Duration.ofSeconds(5);
    Duration renewEvery = Duration.ofMinutes(10);
    Consumer<RuntimeException> onError = e -> { };

    private WatchOptions(Consumer<Values> onChange) {
        this.onChange = Objects.requireNonNull(onChange, "onChange");
    }

    /**
     * {@code onChange} gets the values at the start and again each time they change. Values that
     * don't resolve never reach it: what's running keeps the previous ones. An exception it throws
     * goes to {@link #onError}.
     */
    public static WatchOptions of(Consumer<Values> onChange) {
        return new WatchOptions(onChange);
    }

    /** The folder ({@code ""}, the default, for the root). */
    public WatchOptions path(String path) {
        this.path = path == null ? "" : path;
        return this;
    }

    /** Resolve as this consumer ({@code app:web}) instead of reading: dynamic leases are the consumer's. */
    public WatchOptions consumer(String consumer) {
        this.consumer = consumer;
        return this;
    }

    /**
     * How soon to ask again when the server answered at once (one without long polls): the
     * default is 5 s. A long poll is asked again as soon as it answers.
     */
    public WatchOptions every(Duration every) {
        this.every = positive(every, "every");
        return this;
    }

    /** How often the latest values' dynamic leases are renewed (default 10 minutes). */
    public WatchOptions renewEvery(Duration renewEvery) {
        this.renewEvery = positive(renewEvery, "renewEvery");
        return this;
    }

    /** A check, a fetch or a renewal that failed; the watch goes on. */
    public WatchOptions onError(Consumer<RuntimeException> onError) {
        this.onError = Objects.requireNonNull(onError, "onError");
        return this;
    }

    private static Duration positive(Duration d, String name) {
        if (d == null || d.isNegative() || d.isZero()) {
            throw new IllegalArgumentException(name + " must be positive");
        }
        return d;
    }
}
