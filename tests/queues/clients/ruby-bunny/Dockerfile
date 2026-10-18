FROM ruby:3.4-slim AS build
# openssl and rbtree (via sorted_set) are native extensions
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && apt-get update && apt-get install -y --no-install-recommends build-essential libssl-dev
WORKDIR /app
ENV BUNDLE_FROZEN=true
COPY Gemfile Gemfile.lock ./
RUN --mount=type=cache,target=/usr/local/bundle/cache bundle install

FROM ruby:3.4-slim
WORKDIR /app
ENV BUNDLE_FROZEN=true
COPY --from=build /usr/local/bundle /usr/local/bundle
COPY Gemfile Gemfile.lock main.rb ./
ENTRYPOINT ["bundle", "exec", "ruby", "main.rb"]
