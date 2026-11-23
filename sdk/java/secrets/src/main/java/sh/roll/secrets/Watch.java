package sh.roll.secrets;

import java.time.Duration;
import java.util.List;

/**
 * A running {@link RollSecrets#watch}: a daemon thread that follows the digest. {@link #close()}
 * stops it; nothing is called back once it returns.
 */
public final class Watch implements AutoCloseable {
    /** A long poll's wait, the server's most. */
    private static final Duration WAIT = Duration.ofSeconds(25);

    private final RollSecrets client;
    private final String env;
    private final WatchOptions o;
    private final Thread thread;
    private final Object lock = new Object();
    /** The last answer and its stamps, so a change reads only what changed (`…/values/delta`). */
    private final RollSecrets.Held held = new RollSecrets.Held();
    private volatile boolean stopped;

    Watch(RollSecrets client, String env, WatchOptions o) {
        this.client = client;
        this.env = env;
        this.o = o;
        this.thread = new Thread(this::run, "roll-secrets-watch-" + env);
        thread.setDaemon(true);
    }

    void start() {
        thread.start();
    }

    /** Whether the watch is still running. */
    public boolean running() {
        return thread.isAlive() && !stopped;
    }

    /** Stops the watch, abandoning a held long poll, and waits for its thread to end. */
    @Override
    public void close() {
        synchronized (lock) {
            stopped = true;
        }
        thread.interrupt();
        if (Thread.currentThread() != thread) {
            try {
                thread.join(5_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    private void fail(RuntimeException e) {
        // A stop's interrupt surfaces as a failed request: it isn't one.
        if (!stopped) {
            o.onError.accept(e);
        }
    }

    private void run() {
        String last = null;
        List<String> leases = List.of();
        long renewAt = System.nanoTime() + o.renewEvery.toNanos();
        while (!stopped) {
            long next = o.every.toMillis();
            try {
                long asked = System.nanoTime();
                String now = client.digestSince(env, o.path, last, WAIT);
                if (stopped) {
                    return;
                }
                boolean changed = !now.equals(last);
                // At once after a changed or held answer; `every` after an unchanged one that came
                // straight back (a server without long polls).
                if (changed || System.nanoTime() - asked >= 1_000_000_000L) {
                    next = 0;
                }
                if (changed) {
                    Values v = client.load(env, o, held); // a failure is retried on the next round
                    String before = last;
                    last = now;
                    if (!v.problems().isEmpty()) {
                        // Nothing running yet: try again next round. Otherwise the next change tries again.
                        if (before == null) {
                            last = null;
                        }
                        Problem p = v.problems().get(0);
                        throw new RollSecretsException(409, "unresolved", env + " doesn't resolve: " + p.key() + ": " + p.problem());
                    }
                    leases = v.leases();
                    synchronized (lock) {
                        if (stopped) {
                            return;
                        }
                        o.onChange.accept(v);
                    }
                }
            } catch (RuntimeException e) {
                next = o.every.toMillis(); // never at once after a failure: that would spin
                fail(e);
            }
            if (System.nanoTime() - renewAt >= 0) {
                renewAt = System.nanoTime() + o.renewEvery.toNanos();
                for (String l : leases) {
                    if (stopped) {
                        return;
                    }
                    try {
                        client.renew(l, 0);
                    } catch (RuntimeException e) {
                        fail(e);
                    }
                }
            }
            if (next > 0 && !pause(Math.min(next, Math.max(1, (renewAt - System.nanoTime()) / 1_000_000)))) {
                return;
            }
        }
    }

    /** Sleeps {@code ms}; false when the watch was stopped meanwhile. */
    private boolean pause(long ms) {
        try {
            Thread.sleep(ms);
            return !stopped;
        } catch (InterruptedException e) {
            return false;
        }
    }

    @Override
    public String toString() {
        return "Watch[" + env + (stopped ? ", stopped" : "") + "]";
    }
}
