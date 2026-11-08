FROM ruby:3.4-slim@sha256:65680693964528b522a38d85ec5065538c7ab9e75268802aab9770f39ee56de0 AS build
# openssl and rbtree (via sorted_set) are native extensions
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && apt-get update && apt-get install -y --no-install-recommends build-essential libssl-dev
WORKDIR /app
ENV BUNDLE_FROZEN=true
COPY Gemfile Gemfile.lock ./
RUN --mount=type=cache,target=/usr/local/bundle/cache bundle install

FROM ruby:3.4-slim@sha256:65680693964528b522a38d85ec5065538c7ab9e75268802aab9770f39ee56de0
WORKDIR /app
ENV BUNDLE_FROZEN=true
COPY --from=build /usr/local/bundle /usr/local/bundle
COPY Gemfile Gemfile.lock main.rb ./
ENTRYPOINT ["bundle", "exec", "ruby", "main.rb"]
