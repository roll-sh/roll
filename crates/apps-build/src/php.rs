//! PHP: Composer; Laravel and plain PHP, served by PHP's built-in server (no FPM or web server
//! in the image).

use serde_json::Value;

use crate::df::{Df, apt};
use crate::version::{self, Ver};
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, invalid, node};

const DEFAULT: Ver = Ver(8, 3);
const INSTALL: &str = "composer install --no-dev --optimize-autoloader --no-interaction";

/// Extensions `php:*-cli` already has.
const BUNDLED: [&str; 30] = [
    "ctype",
    "curl",
    "date",
    "dom",
    "fileinfo",
    "filter",
    "hash",
    "iconv",
    "json",
    "libxml",
    "mbstring",
    "mysqlnd",
    "openssl",
    "pcre",
    "pdo",
    "pdo_sqlite",
    "phar",
    "posix",
    "random",
    "readline",
    "reflection",
    "session",
    "simplexml",
    "sodium",
    "spl",
    "sqlite3",
    "standard",
    "tokenizer",
    "xml",
    "xmlwriter",
];

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let composer = match tree.read("composer.json") {
        Some(bytes) => {
            Some(serde_json::from_slice::<Value>(&bytes).map_err(|e| invalid("composer.json", e))?)
        }
        None => None,
    };
    let public = tree.read("public/index.php").is_some();
    if composer.is_none() && !public && tree.read("index.php").is_none() {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let require = composer
        .as_ref()
        .map(|c| c["require"].clone())
        .unwrap_or(Value::Null);
    let version = match require["php"]
        .as_str()
        .and_then(|c| version::pick(c, DEFAULT).map(|v| (c, v)))
    {
        Some((c, v)) => {
            notes.push(format!("composer.json requires php {c}: PHP {}", v.minor()));
            v
        }
        None => {
            notes.push(format!("no PHP version set: PHP {}", DEFAULT.minor()));
            DEFAULT
        }
    };
    let laravel = tree.exists("artisan") && require.get("laravel/framework").is_some();
    let docroot = if public { "public" } else { "." };
    let start = format!("php -S 0.0.0.0:$PORT -t {docroot}");
    notes.push(if laravel {
        "laravel/framework and artisan: Laravel".to_string()
    } else if composer.is_some() {
        "found composer.json: PHP with Composer".to_string()
    } else {
        "found index.php: plain PHP".to_string()
    });
    notes.push(format!("PHP's built-in server serves {docroot}/"));

    let mut extensions: Vec<String> = require
        .as_object()
        .map(|r| {
            r.keys()
                .filter_map(|k| k.strip_prefix("ext-"))
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    if laravel {
        extensions.extend(["pdo_mysql", "pdo_pgsql"].map(String::from));
    }
    extensions.retain(|e| {
        !BUNDLED.contains(&e.as_str()) && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    extensions.sort();
    extensions.dedup();
    if !extensions.is_empty() {
        notes.push(format!(
            "installing the extensions {}",
            extensions.join(", ")
        ));
    }
    let has_composer = composer.is_some();
    // Path repositories point into the tree: the install needs all of it.
    let manifests: Option<Vec<String>> = composer
        .as_ref()
        .filter(|c| !c["repositories"].to_string().contains("\"path\""))
        .map(|_| {
            ["composer.json", "composer.lock"]
                .into_iter()
                .filter(|f| tree.exists(f))
                .map(String::from)
                .collect()
        });
    let assets = node::assets(tree, &mut notes)?;
    let image = format!("php:{}-cli", version.minor());
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::with_assets(assets.as_ref(), c);
        df.from(&image, Some("base"));
        if !extensions.is_empty() {
            df.line("COPY --from=mlocati/php-extension-installer:2 /usr/bin/install-php-extensions /usr/local/bin/");
            df.line(&format!(
                "RUN install-php-extensions {}",
                extensions.join(" ")
            ));
        }
        df.line("WORKDIR /app");
        let built = has_composer || c.install.is_some() || c.build.is_some();
        if built {
            df.from("base", Some("build"));
            df.line("COPY --from=composer:2 /usr/bin/composer /usr/bin/composer");
            df.run(&[], &apt(&["git", "unzip"]));
            df.env(&[
                ("COMPOSER_ALLOW_SUPERUSER", "1"),
                ("COMPOSER_CACHE_DIR", "/tmp/composer"),
            ]);
            let caches = ["/tmp/composer"];
            match (&manifests, c.install) {
                // Dependencies first (their layer survives source edits), then the autoloader,
                // whose scripts (Laravel's package:discover) need the app.
                (Some(files), Some(INSTALL)) if !files.is_empty() => {
                    df.line(&format!("COPY {} ./", files.join(" ")));
                    df.run(&caches, "composer install --no-dev --no-interaction --no-autoloader --no-scripts --prefer-dist");
                    df.copy_source();
                    df.run(&caches, "composer dump-autoload --optimize --no-dev");
                }
                (_, install) => {
                    df.copy_source();
                    if let Some(install) = install {
                        df.run(&caches, install);
                    }
                }
            }
            if let Some(build) = c.build {
                df.run(&caches, build);
            }
            df.from("base", None);
            df.line("COPY --from=build --chown=www-data:www-data /app /app");
        } else {
            df.line("COPY --chown=www-data:www-data . .");
        }
        df.env(&[("PHP_CLI_SERVER_WORKERS", "4")]);
        df.serve(Some("www-data"), c.start);
        df.finish()
    });
    notes.push(format!("starts with {start}"));
    Ok(Some(Draft {
        provider: Provider::Php,
        framework: laravel.then_some("laravel"),
        language_version: Some(version.minor()),
        install: has_composer.then(|| INSTALL.to_string()),
        build: None,
        start: Some(start),
        notes,
        render,
    }))
}
