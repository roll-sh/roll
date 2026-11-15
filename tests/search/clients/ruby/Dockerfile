FROM ruby:3.4
WORKDIR /app
COPY Gemfile .
RUN bundle install --quiet
COPY *.rb ./
ENTRYPOINT ["bundle", "exec", "ruby"]
CMD ["es_client.rb"]
