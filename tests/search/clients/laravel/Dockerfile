# Laravel Scout with babenkoivan/elastic-scout-driver (elastic-adapter, elastic-client on
# elasticsearch-php 9) in a stock Laravel application: smoke.php boots it and drives Scout.
FROM composer:2 AS app
WORKDIR /app
RUN composer create-project --no-interaction --no-progress --quiet --prefer-dist laravel/laravel . "^13.0" \
 && composer require --no-interaction --no-progress --quiet laravel/scout:^11.0 babenkoivan/elastic-scout-driver:^6.0 \
 && rm -rf tests node_modules
COPY Book.php app/Models/Book.php
COPY smoke.php smoke.php

FROM php:8.4-cli
WORKDIR /app
COPY --from=app /app /app
ENTRYPOINT ["php", "smoke.php"]
