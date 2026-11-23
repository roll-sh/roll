package sh.roll.secrets.spring;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.net.ServerSocket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.springframework.boot.Banner;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.boot.context.properties.bind.Bindable;
import org.springframework.boot.context.properties.bind.Binder;
import org.springframework.context.ApplicationListener;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.context.annotation.Configuration;
import org.springframework.core.env.ConfigurableEnvironment;
import org.springframework.core.env.PropertySource;

class RollSecretsSpringTest {
    @Configuration(proxyBeanMethods = false)
    static class App { }

    record Db(String url, String password) { }

    // Output is captured for the whole test: no value may reach it (Spring logs to stderr here).
    private final ByteArrayOutputStream output = new ByteArrayOutputStream();
    private PrintStream err;
    private final List<ConfigurableApplicationContext> contexts = new ArrayList<>();

    @BeforeEach
    void captureOutput() {
        err = System.err;
        System.setErr(new PrintStream(output, true, StandardCharsets.UTF_8));
    }

    @AfterEach
    void restoreOutput() {
        contexts.forEach(ConfigurableApplicationContext::close);
        System.setErr(err);
        String out = output.toString(StandardCharsets.UTF_8);
        for (String value : List.of("fake-db-url", "fake-api-key", "fake-dynamic-password", FakeRolld.TOKEN)) {
            assertFalse(out.contains(value), "the output holds a value");
        }
    }

    private ConfigurableApplicationContext start(String api, String imports, String... more) {
        List<String> args = new ArrayList<>(List.of("--spring.config.import=" + imports, "--roll.secrets.api-url=" + api,
                "--roll.secrets.token=" + FakeRolld.TOKEN, "--roll.secrets.project=prj_test"));
        args.addAll(List.of(more));
        ConfigurableApplicationContext c = new SpringApplicationBuilder(App.class).web(WebApplicationType.NONE)
                .bannerMode(Banner.Mode.OFF).logStartupInfo(false).listeners(new Events()).run(args.toArray(String[]::new));
        contexts.add(c);
        return c;
    }

    static final BlockingQueue<RollSecretsRefreshedEvent> EVENTS = new LinkedBlockingQueue<>();

    static final class Events implements ApplicationListener<RollSecretsRefreshedEvent> {
        @Override
        public void onApplicationEvent(RollSecretsRefreshedEvent e) {
            EVENTS.add(e);
        }
    }

    @Test
    void anImportPutsTheFoldersValuesInTheEnvironmentWithoutItsDynamicKeys() throws Exception {
        try (FakeRolld rolld = new FakeRolld()) {
            rolld.put("DB_URL", "postgres://fake-db-url/shop");
            rolld.put("API_KEY", "fake-api-key");
            rolld.putDynamic("DB_PASSWORD", "fake-dynamic-password");

            ConfigurableEnvironment env = start(rolld.url(), "roll-secrets:production/api").getEnvironment();
            assertEquals("postgres://fake-db-url/shop", env.getProperty("DB_URL"));
            assertEquals("fake-api-key", env.getProperty("api.key")); // relaxed, as an environment variable
            assertEquals("postgres://fake-db-url/shop", Binder.get(env).bind("db", Db.class).get().url());
            assertEquals(Map.of("url", "postgres://fake-db-url/shop"), Binder.get(env).bind("db", Bindable.mapOf(String.class, String.class)).get());
            // Dynamic keys stay out, and none was issued: the read said dynamic=false.
            assertNull(env.getProperty("DB_PASSWORD"));
            assertTrue(rolld.seen().contains("GET " + FakeRolld.ENV + "/values?path=%2Fapi&dynamic=false"), rolld.seen().toString());

            PropertySource<?> source = env.getPropertySources().stream().filter(s -> s instanceof RollSecretsPropertySource).findFirst().orElseThrow();
            assertEquals("RollSecretsPropertySource[roll-secrets:production/api, 2 keys]", source.toString());
            assertFalse(String.valueOf(source.getSource()).contains("fake"));

            // Unless asked for.
            env = start(rolld.url(), "roll-secrets:production/api", "--roll.secrets.include-dynamic=true").getEnvironment();
            assertEquals("fake-dynamic-password", env.getProperty("db.password"));
        }
    }

    @Test
    void anOptionalImportSurvivesAnUnreachableServerOrA403AndARequiredOneFailsStartup() throws Exception {
        String unreachable;
        try (ServerSocket s = new ServerSocket(0)) {
            unreachable = "http://127.0.0.1:" + s.getLocalPort();
        }
        assertNull(start(unreachable, "optional:roll-secrets:production/api").getEnvironment().getProperty("DB_URL"));

        RollSecretsConfigDataException e = assertThrows(RollSecretsConfigDataException.class, () -> start(unreachable, "roll-secrets:production/api"));
        assertEquals("roll-secrets:production/api couldn't be loaded: roll secrets didn't answer: ConnectException (no answer, io)", e.getMessage());

        try (FakeRolld rolld = new FakeRolld()) {
            rolld.put("DB_URL", "postgres://fake-db-url/shop");
            rolld.answerWith(403);
            ConfigurableEnvironment env = start(rolld.url(), "optional:roll-secrets:production/api").getEnvironment();
            assertNull(env.getProperty("DB_URL"));
            e = assertThrows(RollSecretsConfigDataException.class, () -> start(rolld.url(), "roll-secrets:production/api"));
            assertTrue(e.getMessage().contains("(HTTP 403, forbidden)"), e.getMessage());
        }
        assertTrue(output.toString(StandardCharsets.UTF_8).contains("optional:roll-secrets:production/api wasn't loaded"));
    }

    @Test
    void aRefreshingImportFollowsTheDigestAndPublishesAnEvent() throws Exception {
        EVENTS.clear();
        try (FakeRolld rolld = new FakeRolld()) {
            rolld.put("DB_URL", "postgres://fake-db-url/one");
            ConfigurableApplicationContext app = start(rolld.url(), "roll-secrets:production/api", "--roll.secrets.refresh=true");
            ConfigurableEnvironment env = app.getEnvironment();
            assertEquals("postgres://fake-db-url/one", env.getProperty("DB_URL"));
            // One read at startup: the watch's first (in full: this rolld has no delta route).
            assertEquals(1, rolld.seen().stream().filter(r -> r.startsWith("GET ") && r.contains("/values")).count(), rolld.seen().toString());

            rolld.put("DB_URL", "postgres://fake-db-url/two");
            rolld.put("API_KEY", "fake-api-key");
            RollSecretsRefreshedEvent e = EVENTS.poll(10, TimeUnit.SECONDS);
            assertNotNull(e, "no refresh event");
            // The two puts may arrive as one change or as two.
            while (!env.containsProperty("API_KEY") && (e = EVENTS.poll(10, TimeUnit.SECONDS)) != null) {
                assertNotNull(e);
            }
            assertEquals("postgres://fake-db-url/two", env.getProperty("DB_URL"));
            assertEquals("fake-api-key", Binder.get(env).bind("api.key", String.class).get());
            assertNotNull(e, "no second refresh event");
            assertTrue(Set.of("DB_URL", "API_KEY").containsAll(e.keys()), e.keys().toString());
            assertFalse(e.toString().contains("fake"));

            // Closing the context stops the watch.
            app.close();
            int asked = rolld.seen().size();
            Thread.sleep(1_500);
            assertEquals(asked, rolld.seen().size());
        }
    }
}
