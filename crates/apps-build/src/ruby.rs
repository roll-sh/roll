//! Ruby: Bundler; Rails and Rack apps.

use crate::df::{Df, apt};
use crate::version::{self, Ver};
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, node, text};

const DEFAULT: Ver = Ver(3, 3);

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(gemfile) = text(tree, "Gemfile") else {
        return Ok(None);
    };
    let lock = text(tree, "Gemfile.lock");
    let mut notes = vec![match lock {
        Some(_) => "found Gemfile and Gemfile.lock: bundler".to_string(),
        None => "found Gemfile: bundler (no Gemfile.lock, so versions aren't pinned)".to_string(),
    }];
    let gem = |name: &str| {
        lock.as_deref()
            .is_some_and(|l| l.contains(&format!("\n    {name} (")))
            || gemfile.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with(&format!("gem \"{name}\"")) || l.starts_with(&format!("gem '{name}'"))
            })
    };
    let version = ruby_version(tree, &gemfile, lock.as_deref(), &mut notes);

    let rails = gem("rails") || (tree.exists("config/application.rb") && tree.exists("bin/rails"));
    let rails_cmd = if tree.read("bin/rails").is_some() {
        "bin/rails"
    } else {
        "bundle exec rails"
    };
    let assets = node::assets(tree, &mut notes)?;
    let (framework, build, start) = if rails {
        notes.push("rails in the Gemfile: Rails".into());
        let pipeline = gem("propshaft") || gem("sprockets") || gem("sprockets-rails");
        let build = pipeline.then(|| {
            notes.push("an asset pipeline: assets:precompile at build time".into());
            // The Node stage already ran the JS and CSS builds.
            let skip = if assets.is_some() {
                "SKIP_JS_BUILD=1 SKIP_CSS_BUILD=1 "
            } else {
                ""
            };
            format!("SECRET_KEY_BASE_DUMMY=1 {skip}{rails_cmd} assets:precompile")
        });
        (
            Some("rails"),
            build,
            Some(format!("{rails_cmd} server -b 0.0.0.0 -p $PORT")),
        )
    } else if tree.exists("config.ru") {
        let start = if gem("puma") {
            "bundle exec puma -b tcp://0.0.0.0:$PORT"
        } else {
            "bundle exec rackup -o 0.0.0.0 -p $PORT"
        };
        notes.push(format!(
            "found config.ru: a Rack app, served with {}",
            start.split_whitespace().nth(2).unwrap_or("rackup")
        ));
        (Some("rack"), None, Some(start.to_string()))
    } else {
        notes.push("no Rails or config.ru: set start".into());
        (None, None, None)
    };
    if let Some(s) = &start {
        notes.push(format!("starts with {s}"));
    }

    let mut build_packages = vec!["build-essential", "git", "pkg-config", "libyaml-dev"];
    let mut runtime_packages = Vec::new();
    if gem("pg") {
        build_packages.push("libpq-dev");
        runtime_packages.push("libpq5");
    }
    if gem("mysql2") {
        build_packages.push("default-libmysqlclient-dev");
        runtime_packages.push("libmariadb3");
    }
    // A Gemfile that reads other files (a gemspec, local paths) needs the whole tree to install.
    let reads_tree = gemfile.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("gemspec") || l.contains("path:") || l.contains(":path")
    });
    let manifests: Option<Vec<String>> = (!reads_tree).then(|| {
        ["Gemfile", "Gemfile.lock", ".ruby-version"]
            .into_iter()
            .filter(|f| tree.exists(f))
            .map(String::from)
            .collect()
    });
    let locked = lock.is_some();
    let image = format!("ruby:{}-slim", version.minor());
    let render = Box::new(move |c: &Commands<'_>| {
        let mut env = vec![
            ("BUNDLE_PATH", "/usr/local/bundle"),
            ("BUNDLE_WITHOUT", "development:test"),
            ("RAILS_ENV", "production"),
            ("RACK_ENV", "production"),
        ];
        if locked {
            env.push(("BUNDLE_DEPLOYMENT", "1"));
        }
        let mut df = Df::with_assets(assets.as_deref());
        df.from(&image, Some("build"));
        df.run(&[], &apt(&build_packages));
        df.line("WORKDIR /app");
        df.env(&env);
        let install = c
            .install
            .map(|i| format!("{i} && rm -rf ~/.bundle/ /usr/local/bundle/ruby/*/cache"));
        df.source(
            manifests.as_deref(),
            &[],
            &Commands {
                install: install.as_deref(),
                ..*c
            },
        );
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.from(&image, None);
        if !runtime_packages.is_empty() {
            df.run(&[], &apt(&runtime_packages));
        }
        df.line("WORKDIR /app");
        df.env(&env);
        df.env(&[
            ("RAILS_LOG_TO_STDOUT", "1"),
            ("RAILS_SERVE_STATIC_FILES", "1"),
        ]);
        df.line("RUN useradd --create-home --uid 10001 app");
        df.line("COPY --from=build /usr/local/bundle /usr/local/bundle");
        df.line("COPY --from=build --chown=app:app /app /app");
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Ruby,
        framework,
        language_version: Some(version.minor()),
        install: Some("bundle install".into()),
        build,
        start,
        notes,
        render,
    }))
}

fn ruby_version(
    tree: &dyn Tree,
    gemfile: &str,
    lock: Option<&str>,
    notes: &mut Vec<String>,
) -> Ver {
    if let Some(pin) = text(tree, ".ruby-version")
        && let Some(v) = version::parse(pin.trim())
    {
        notes.push(format!(
            ".ruby-version pins {}: Ruby {}",
            pin.trim(),
            v.minor()
        ));
        return v;
    }
    let declared = gemfile.lines().find_map(|l| {
        let rest = l.trim_start().strip_prefix("ruby ")?;
        let q = rest
            .trim_start()
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')?;
        rest.trim_start()[1..].split(q).next().map(String::from)
    });
    if let Some(c) = &declared
        && let Some(v) = version::pick(c, DEFAULT)
    {
        notes.push(format!("the Gemfile says ruby {c}: Ruby {}", v.minor()));
        return v;
    }
    if let Some(v) = lock
        .and_then(|l| l.split("RUBY VERSION").nth(1))
        .and_then(|r| r.lines().nth(1))
        .and_then(version::parse)
    {
        notes.push(format!("Gemfile.lock was locked with Ruby {}", v.minor()));
        return v;
    }
    notes.push(format!("no Ruby version set: Ruby {}", DEFAULT.minor()));
    DEFAULT
}
