package sh.roll.secrets.spring;

import java.util.List;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicReference;
import org.apache.commons.logging.Log;
import org.springframework.boot.ConfigurableBootstrapContext;
import org.springframework.boot.context.config.ConfigData;
import org.springframework.boot.context.config.ConfigDataLoader;
import org.springframework.boot.context.config.ConfigDataLoaderContext;
import org.springframework.boot.logging.DeferredLogFactory;
import org.springframework.context.ApplicationEvent;
import org.springframework.context.ApplicationListener;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.context.event.ContextClosedEvent;
import org.springframework.context.event.ContextRefreshedEvent;
import sh.roll.secrets.RollSecrets;
import sh.roll.secrets.RollSecretsException;
import sh.roll.secrets.Watch;
import sh.roll.secrets.WatchOptions;

/**
 * Loads a {@code roll-secrets:} import into one {@link RollSecretsPropertySource}. A failure fails
 * startup, unless the import is {@code optional:}: then it's logged (no value is) and the app
 * starts without it. With {@code roll.secrets.refresh=true} the client's watch keeps the source
 * current and publishes a {@link RollSecretsRefreshedEvent} each time it changes.
 */
public class RollSecretsConfigDataLoader implements ConfigDataLoader<RollSecretsConfigDataResource> {
    /** How long a refreshing import waits for its first values. */
    private static final long FIRST_LOAD_SECONDS = 120;

    private final Log log;

    public RollSecretsConfigDataLoader(DeferredLogFactory logs) {
        this.log = logs.getLog(RollSecretsConfigDataLoader.class);
    }

    @Override
    public ConfigData load(ConfigDataLoaderContext context, RollSecretsConfigDataResource resource) {
        try {
            RollSecrets client = resource.settings.client();
            RollSecretsPropertySource source = resource.settings.refresh
                    ? watch(client, resource, context.getBootstrapContext())
                    : RollSecretsPropertySource.load(client, resource.environment(), resource.folder(), resource.settings.includeDynamic);
            return new ConfigData(List.of(source));
        } catch (RollSecretsException e) {
            if (resource.optional()) {
                log.warn("optional:" + resource + " wasn't loaded, starting without it: " + describe(e));
                return ConfigData.EMPTY;
            }
            throw new RollSecretsConfigDataException(resource + " couldn't be loaded: " + describe(e), e);
        }
    }

    /** A failure as a line: roll's message (it holds no value), its status and code. */
    static String describe(RuntimeException e) {
        if (!(e instanceof RollSecretsException r)) {
            return e.getClass().getName(); // not ours: its message could hold anything
        }
        String code = r.code() != null ? r.code() : "";
        String status = r.status() == 0 ? "no answer" : "HTTP " + r.status();
        return r.getMessage() + " (" + status + (code.isEmpty() ? "" : ", " + code) + ")";
    }

    /**
     * Starts the client's watch and waits for its first values, so the folder is read once at
     * startup (and a dynamic credential issued once, its lease then renewed by the watch).
     */
    private RollSecretsPropertySource watch(RollSecrets client, RollSecretsConfigDataResource resource, ConfigurableBootstrapContext bootstrap) {
        boolean includeDynamic = resource.settings.includeDynamic;
        CompletableFuture<RollSecretsPropertySource> first = new CompletableFuture<>();
        AtomicReference<RollSecretsPropertySource> current = new AtomicReference<>();
        AtomicReference<ConfigurableApplicationContext> publisher = new AtomicReference<>();
        WatchOptions o = WatchOptions.of(v -> {
            RollSecretsPropertySource s = current.get();
            if (s == null) {
                s = new RollSecretsPropertySource(resource.environment(), resource.folder(), v, includeDynamic);
                current.set(s);
                first.complete(s);
                return;
            }
            Set<String> changed = s.update(v);
            if (changed.isEmpty()) {
                return;
            }
            log.info(resource + " changed: " + changed);
            ConfigurableApplicationContext app = publisher.get();
            if (app != null) {
                app.publishEvent(new RollSecretsRefreshedEvent(s, changed));
            }
        }).path(resource.folder()).dynamic(includeDynamic).onError(e -> {
            if (!first.completeExceptionally(e)) {
                log.warn(resource + ": refreshing failed, keeping the values it has: " + describe(e));
            }
        });
        Watch w = client.watch(resource.environment(), o);
        try {
            RollSecretsPropertySource s = first.get(FIRST_LOAD_SECONDS, TimeUnit.SECONDS);
            // The context exists once the bootstrap context closes: events go to it once it's
            // refreshed, and its close stops the watch.
            bootstrap.addCloseListener(event -> {
                ConfigurableApplicationContext app = event.getApplicationContext();
                app.addApplicationListener(new Lifecycle(app, publisher, w));
            });
            return s;
        } catch (ExecutionException e) {
            w.close();
            throw e.getCause() instanceof RollSecretsException r ? r : new RollSecretsException(0, "refresh", describe((RuntimeException) e.getCause()));
        } catch (TimeoutException e) {
            w.close();
            throw new RollSecretsException(0, "io", "no values within " + FIRST_LOAD_SECONDS + " s");
        } catch (InterruptedException e) {
            w.close();
            Thread.currentThread().interrupt();
            throw new RollSecretsException(0, "interrupted", "interrupted");
        }
    }

    /** Hands a refreshed context to the watch's events, and stops the watch when it closes. */
    private record Lifecycle(ConfigurableApplicationContext app, AtomicReference<ConfigurableApplicationContext> publisher, Watch watch)
            implements ApplicationListener<ApplicationEvent> {
        @Override
        public void onApplicationEvent(ApplicationEvent event) {
            // A child context's events reach its parent's listeners too: only this context's count.
            if (event instanceof ContextRefreshedEvent r && r.getApplicationContext() == app) {
                publisher.set(app);
            } else if (event instanceof ContextClosedEvent c && c.getApplicationContext() == app) {
                publisher.set(null);
                watch.close();
            }
        }
    }
}
