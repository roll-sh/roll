# searchkick's own test suite (unmodified, pinned tag) against the server at ELASTICSEARCH_URL.
# The queue tests need Redis on localhost:6379 (the suite's RedisClient takes no URL): a
# redis-server bound to 127.0.0.1 runs inside the container for the length of the suite.
FROM ruby:3.4
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends redis-server > /dev/null && rm -rf /var/lib/apt/lists/*
ARG SEARCHKICK_VERSION=v6.1.2
RUN git clone -q --depth 1 --branch ${SEARCHKICK_VERSION} https://github.com/ankane/searchkick.git /searchkick
WORKDIR /searchkick
RUN bundle install --quiet
ENTRYPOINT ["sh", "-c", "redis-server --bind 127.0.0.1 --port 6379 --save '' --appendonly no --daemonize yes > /dev/null && exec bundle exec rake test \"$@\"", "searchkick"]
